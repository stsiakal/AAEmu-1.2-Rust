//! An in-process connection without a socket (`TestClient`'s production counterpart).
//!
//! The server-side player bots act like clients: they build the same `CS*` packets and dispatch
//! them through the normal handlers, so every permission and gameplay check runs. A normal
//! client is owned by a connection task (reader loop + writer task); this type owns the
//! [`GameConnection`] itself, keeps the frames the server writes in its boxed outbox so the caller
//! can read them, and services the [`ConnCommand`] channel (logout timer, shutdown return to the
//! world list) that a socket task would service in `net::listener::serve`.

use std::net::SocketAddr;

use tokio::sync::mpsc;

use crate::net::connection::{ConnCommand, GameConnection};
use crate::net::outbox::{outbox, OutboxReceiver};
use crate::net::registry::ClientPacket;
use crate::server::GameServer;

/// A [`GameConnection`] whose replies stay in memory and whose commands the caller services.
pub struct SocketlessConnection {
    /// The connection state (the same type a socket task owns).
    pub conn: GameConnection,
    out: OutboxReceiver,
    commands: mpsc::UnboundedReceiver<ConnCommand>,
}

impl SocketlessConnection {
    /// Creates the connection state, its outbox and its command channel, and registers the
    /// connection handle in `server.connections` (a socket task does the same on connect).
    /// The session id comes from the server's counter, so it is unique.
    pub fn new(server: &GameServer, account_id: u32) -> Self {
        let id = server.next_connection_id();
        let peer: SocketAddr = "0.0.0.0:0".parse().expect("socketless peer address");
        let (out_tx, out) = outbox();
        let (cmd_tx, commands) = mpsc::unbounded_channel();
        let (mut conn, handle) = GameConnection::new(id, peer, out_tx, cmd_tx);
        conn.set_account_id(account_id);
        server.connections.add(handle);
        Self {
            conn,
            out,
            commands,
        }
    }

    /// The session id (also the `conn.id`).
    pub fn id(&self) -> u32 {
        self.conn.id
    }

    /// Frames written to this connection since the last call, as `(opcode, body)`. Level-2
    /// frames are tagged `0x8000 | opcode` (the same shape `TestClient::drain` returns).
    pub fn drain(&mut self) -> Vec<(u16, Vec<u8>)> {
        let mut frames = Vec::new();
        while let Ok(f) = self.out.try_recv() {
            if f[1] == 1 {
                frames.push((u16::from_le_bytes([f[4], f[5]]), f[6..].to_vec()));
            } else {
                frames.push((0x8000 | u16::from_le_bytes([f[2], f[3]]), f[4..].to_vec()));
            }
        }
        frames
    }

    /// Opcodes of the drained frames.
    pub fn drain_ops(&mut self) -> Vec<u16> {
        self.drain().into_iter().map(|f| f.0).collect()
    }

    /// Queues an encoded/decoded packet like a client handler would: the packet is recorded in
    /// the connection's bug-report ring (as `handle_frame` does for a wire frame), then
    /// [`ClientPacket::handle`] runs, then the per-frame achievement work
    /// `GameServer::handle_frame` does after a handler.
    pub async fn dispatch<P: ClientPacket + std::fmt::Debug>(
        &mut self,
        server: &GameServer,
        packet: P,
    ) -> crate::Result<()> {
        if let Some(ring) = self.conn.packet_ring().cloned() {
            let (level, opcode) = server
                .registry
                .find_by_name(P::NAME)
                .unwrap_or((crate::net::packet::LEVEL_PLAIN, 0));
            crate::systems::services::bugreport::packet_ring::record_incoming_packet(
                Some(&ring),
                level,
                opcode,
                P::NAME,
                &packet,
            );
        }
        let result = packet.handle(server, &mut self.conn).await;
        crate::systems::economy::flush_deferred_glider_reequips(server);
        crate::systems::flush_achievements(server);
        result
    }

    /// Runs the `ConnCommand` arms of `net::listener::serve` for everything queued on this
    /// connection: sends from other tasks, the expired logout timer (`LeaveWorld`), and the
    /// shutdown `ReturnToWorldList`. Returns `false` when the connection is closing (the caller
    /// should run `net::listener::on_disconnect` and drop it).
    pub async fn service_commands(&mut self, server: &GameServer) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(ConnCommand::Send(packet)) => self.conn.send(packet.as_ref()),
                Ok(ConnCommand::LeaveWorld(target)) => {
                    let active = self.conn.active_char.clone();
                    crate::world::session::leave_world_task(
                        server,
                        Some(&mut self.conn),
                        target,
                        active,
                    )
                    .await;
                }
                Ok(ConnCommand::ReturnToWorldList) => {
                    crate::world::session::return_to_world_list(server, &mut self.conn).await;
                }
                Ok(ConnCommand::DetachAndClose) => {
                    self.conn.active_char = None;
                    return false;
                }
                Ok(ConnCommand::Close) => return false,
                Err(mpsc::error::TryRecvError::Empty) => return true,
                Err(mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::test_server;

    /// The connection is registered, replies are readable, and a `Send` command reaches the
    /// outbox (the command channel is serviced without a socket task).
    #[tokio::test]
    async fn replies_and_commands_are_serviced_without_a_socket() {
        let server = test_server();
        let mut conn = super::SocketlessConnection::new(&server, 1000);
        assert!(server.connections.get(conn.id()).is_some());
        conn.conn.send(&crate::packets::world::SCChatMessagePacket {
            chat_type: 0,
            sender: None,
            message: "hello".into(),
            ability: 0,
            language_type: 0,
        });
        let frames = conn.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, crate::opcodes::sc::SCChatMessagePacket);
        conn.conn
            .handle()
            .send(crate::packets::world::SCChatMessagePacket {
                chat_type: 0,
                sender: None,
                message: "again".into(),
                ability: 0,
                language_type: 0,
            });
        assert!(conn.service_commands(&server).await);
        assert_eq!(
            conn.drain_ops(),
            vec![crate::opcodes::sc::SCChatMessagePacket]
        );
        conn.conn.shutdown();
        assert!(!conn.service_commands(&server).await);
    }
}

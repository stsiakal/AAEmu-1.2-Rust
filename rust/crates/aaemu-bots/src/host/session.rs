//! One live bot: its socketless connection, the replies it can read, and its think schedule.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aaemu_game::net::registry::ClientPacket;
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::world::session::{self, leave_target};
use aaemu_game::GameServer;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Idle cadence of a bot between thinks (the cadence table / governor comes in P2/P3).
const IDLE_THINK: Duration = Duration::from_millis(1_000);

/// A bot character in the world on a socketless connection.
pub struct BotSession {
    pub character_id: u32,
    pub name: String,
    conn: Mutex<SocketlessConnection>,
    next_think: Mutex<Instant>,
    disconnected: AtomicBool,
}

impl BotSession {
    pub fn new(conn: SocketlessConnection, character_id: u32, name: String) -> Self {
        // Stagger the first think by character id so a roster does not tick in lockstep.
        let jitter = Duration::from_millis(u64::from(character_id % 250));
        Self {
            character_id,
            name,
            conn: Mutex::new(conn),
            next_think: Mutex::new(Instant::now() + jitter),
            disconnected: AtomicBool::new(false),
        }
    }

    /// Dispatches one client packet through the real handler on this bot's connection.
    pub async fn dispatch<P: ClientPacket>(
        &self,
        server: &GameServer,
        packet: P,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().await;
        conn.dispatch(server, packet).await?;
        Ok(())
    }

    /// The S->C frames the server wrote to this bot since the last call (the replies a client
    /// would read off the socket).
    pub async fn drain_replies(&self) -> Vec<(u16, Vec<u8>)> {
        self.conn.lock().await.drain()
    }

    /// `(obj_id, is_online)` of the active character.
    pub async fn state(&self) -> (u32, bool) {
        let conn = self.conn.lock().await;
        match &conn.conn.active_char {
            Some(player) => {
                let online = player.data().is_online;
                (player.obj_id, online)
            }
            None => (0, false),
        }
    }

    /// One scheduler pass. Services the connection's commands every tick; the think itself
    /// (draining replies; the AI in P2) runs on the per-bot `next_think` cadence.
    /// `false` = the connection is closing and the host must disconnect it.
    pub async fn tick(&self, server: &GameServer) -> bool {
        let mut conn = self.conn.lock().await;
        if !conn.service_commands(server).await {
            return false;
        }
        let now = Instant::now();
        let mut next = self.next_think.lock().await;
        if now >= *next {
            let jitter = Duration::from_millis(u64::from(self.character_id % 250));
            *next = now + IDLE_THINK + jitter;
            let replies = conn.drain();
            if !replies.is_empty() {
                tracing::trace!(
                    character_id = self.character_id,
                    frames = replies.len(),
                    "Bots: drained replies"
                );
            }
        }
        true
    }

    /// The normal logout without the client's logout timer: save + remove from the world. The
    /// connection stays alive (in the lobby) until [`BotSession::disconnect`].
    pub async fn logout(&self, server: &GameServer) {
        let mut conn = self.conn.lock().await;
        session::leave_world_now(server, &mut conn.conn, leave_target::CHARACTER_SELECT).await;
    }

    /// The `GameConnection.OnDisconnect` cleanup (frees the account/connection and any session
    /// reservation). Idempotent: the host calls it once after a logout or a closed connection.
    pub async fn disconnect(&self, server: &GameServer) {
        if self.disconnected.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(server) = server.self_arc() else {
            return;
        };
        let mut conn = self.conn.lock().await;
        aaemu_game::net::listener::on_disconnect(&server, &mut conn.conn).await;
    }
}

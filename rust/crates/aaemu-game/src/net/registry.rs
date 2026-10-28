//! Packet registry: port of `GameProtocolHandler._packets` / `GameNetwork.RegisterPacket`,
//! keyed by `(level, opcode)`.

use std::collections::HashMap;

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::packets::c2g_world::CSMoveUnitPacket;
use crate::server::GameServer;
use crate::systems::housing::packets::{CSConstructHouseTaxPacket, CSCreateHousePacket};
use crate::systems::skills::handlers::{CSCreateSkillControllerPacket, CSStartSkillPacket};
use crate::systems::vehicles::packets::{
    CSBindSlavePacket, CSChangeMateTargetPacket, CSDespawnSlavePacket, CSDiscardSlavePacket,
    CSMountMatePacket, CSUnMountMatePacket,
};

/// Packets whose decoded fields are added to the opt-in
/// [`crate::net::FIELDS_TRACE_TARGET`] trace. The name is the opcode's 1.2 name.
const FIELD_TRACE_PACKETS: &[&str] = &[
    "CSCreateHousePacket",
    "CSConstructHouseTaxPacket",
    "CSMoveUnitPacket",
    "CSStartSkillPacket",
    "CSCreateSkillControllerPacket",
    "CSChangeMateTargetPacket",
    "CSMountMatePacket",
    "CSUnMountMatePacket",
    "CSBindSlavePacket",
    "CSDiscardSlavePacket",
    "CSDespawnSlavePacket",
];

/// Logs the parsed fields of [`FIELD_TRACE_PACKETS`] packets. Only called when the field
/// trace is enabled, so `Debug` formatting costs nothing otherwise.
fn trace_fields<P: ClientPacket>(packet: &P) {
    use std::any::Any;

    if !FIELD_TRACE_PACKETS.contains(&P::NAME) {
        return;
    }
    let any = packet as &dyn Any;
    if let Some(mov) = any.downcast_ref::<CSMoveUnitPacket>() {
        tracing::trace!(
            target: crate::net::FIELDS_TRACE_TARGET,
            "C->S CSMoveUnitPacket fields: obj_id={} move_type={:?} flags={:?} position={:?} rotation={:?} velocity={:?} detail={:?}",
            mov.obj_id,
            mov.move_type.move_type(),
            mov.move_type.flags,
            mov.move_type.position(),
            mov.move_type.base_rotation(),
            mov.move_type.velocity(),
            mov.move_type
        );
        return;
    }
    macro_rules! log_debug {
        ($($ty:ty),* $(,)?) => {
            $(
                if let Some(packet) = any.downcast_ref::<$ty>() {
                    tracing::trace!(
                        target: crate::net::FIELDS_TRACE_TARGET,
                        "C->S {} fields: {:?}",
                        P::NAME,
                        packet
                    );
                    return;
                }
            )*
        };
    }
    log_debug!(
        CSCreateHousePacket,
        CSConstructHouseTaxPacket,
        CSStartSkillPacket,
        CSCreateSkillControllerPacket,
        CSChangeMateTargetPacket,
        CSMountMatePacket,
        CSUnMountMatePacket,
        CSBindSlavePacket,
        CSDiscardSlavePacket,
        CSDespawnSlavePacket,
    );
}

/// Type-erased handler: reads the packet body and runs its behaviour (`Read` + `Execute`).
pub type HandlerFn =
    for<'a> fn(&'a GameServer, &'a mut GameConnection, PacketStream) -> BoxFuture<'a, Result<()>>;

/// A C->S packet: `Read` is [`ClientPacket::read`], the side effects are [`ClientPacket::handle`].
pub trait ClientPacket: Sized + Send + 'static {
    const NAME: &'static str;

    fn read(stream: &mut PacketStream) -> Result<Self>;

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>>;
}

fn dispatch<'a, P: ClientPacket>(
    server: &'a GameServer,
    conn: &'a mut GameConnection,
    mut stream: PacketStream,
) -> BoxFuture<'a, Result<()>> {
    match P::read(&mut stream) {
        Ok(packet) => {
            tracing::debug!("GamePacket: C->S {} conn={}", P::NAME, conn.id);
            if tracing::enabled!(
                target: crate::net::FIELDS_TRACE_TARGET,
                tracing::Level::TRACE
            ) {
                trace_fields(&packet);
            }
            packet.handle(server, conn)
        }
        Err(e) => {
            tracing::error!("GamePacket: C->S {} failed to read: {e}", P::NAME);
            Box::pin(async move { Err(e) })
        }
    }
}

#[derive(Clone, Copy)]
pub struct Registration {
    pub name: &'static str,
    pub handler: HandlerFn,
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

#[derive(Debug, Default)]
pub struct PacketRegistry {
    packets: HashMap<(u8, u16), Registration>,
    /// (level, opcode, replaced name, new name) of every registration that replaced another.
    duplicates: Vec<(u8, u16, &'static str, &'static str)>,
}

impl PacketRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `RegisterPacket(type, level, typeof(P))`. A later registration replaces an earlier one
    /// (C# `_packets[level][type] = classType`).
    pub fn register<P: ClientPacket>(&mut self, level: u8, opcode: u16) -> &mut Self {
        let old = self.packets.insert(
            (level, opcode),
            Registration {
                name: P::NAME,
                handler: dispatch::<P>,
            },
        );
        if let Some(old) = old {
            self.duplicates.push((level, opcode, old.name, P::NAME));
        }
        self
    }

    /// Registrations that replaced an earlier one (two modules claiming the same opcode is a
    /// porting mistake; `packets::tests::no_duplicate_registrations` guards against it).
    pub fn duplicates(&self) -> &[(u8, u16, &'static str, &'static str)] {
        &self.duplicates
    }

    pub fn get(&self, level: u8, opcode: u16) -> Option<Registration> {
        self.packets.get(&(level, opcode)).copied()
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// Every `((level, opcode), registration)` pair (unordered).
    pub fn iter(&self) -> impl Iterator<Item = ((u8, u16), Registration)> + '_ {
        self.packets.iter().map(|(k, v)| (*k, *v))
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::trace_fields;
    use crate::systems::housing::packets::CSCreateHousePacket;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn field_trace_logs_decoded_house_fields() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            trace_fields(&CSCreateHousePacket {
                design_id: 42,
                x: 1.5,
                y: -2.25,
                z: 3.0,
                z_rot: 0.75,
                item_id: 9,
                money_amount: 100,
                ht: 2,
                auto_use_aa_point: true,
            });
        });
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("C->S CSCreateHousePacket fields:"),
            "{output}"
        );
        assert!(output.contains("design_id: 42"), "{output}");
        assert!(output.contains("z_rot: 0.75"), "{output}");
        assert!(output.contains("auto_use_aa_point: true"), "{output}");
    }

    #[test]
    fn field_trace_ignores_unlisted_packets() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            trace_fields(&crate::packets::c2g_world::CSNotifyInGameCompletedPacket);
        });
        assert!(capture.0.lock().unwrap().is_empty());
    }
}

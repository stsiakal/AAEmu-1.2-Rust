//! Fixed-size per-connection packet ring.
//!
//! Entries are appended where the connection encodes / dispatches a packet (`net::connection`
//! and `GameServer::handle_frame`) and only when bug reports are enabled. The ring is created
//! once per connection from [`new_for_connection`]; a disabled reporter allocates nothing.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::net::packet::ServerPacket;

/// Configured capacity for new connections (`0` = disabled).
static CAPACITY: AtomicUsize = AtomicUsize::new(0);

/// Body snippets kept per entry (characters for the `Debug` form, bytes for client packets).
const BODY_CHARS: usize = 512;
const BODY_BYTES: usize = 256;

/// Sets the capacity for connections created from now on.
pub fn set_capacity(capacity: usize) {
    CAPACITY.store(capacity, Ordering::Relaxed);
}

pub fn capacity() -> usize {
    CAPACITY.load(Ordering::Relaxed)
}

/// A ring for a new connection, or `None` when bug reports / the ring are disabled.
pub fn new_for_connection() -> Option<Arc<PacketRing>> {
    let capacity = capacity();
    (capacity > 0).then(|| Arc::new(PacketRing::new(capacity)))
}

/// One packet in the ring. `body` is the truncated `Debug` of the packet (game -> client) or a
/// capped hex dump of the raw body (client -> game).
#[derive(Debug, Clone, Serialize)]
pub struct PacketEntry {
    /// `"in"` (client -> game) or `"out"` (game -> client).
    pub direction: &'static str,
    pub level: u8,
    pub opcode: u16,
    pub name: &'static str,
    pub size: usize,
    pub timestamp_ms: i64,
    pub body: String,
}

/// Fixed-capacity ring shared by a connection and its handle.
#[derive(Debug)]
pub struct PacketRing {
    capacity: usize,
    entries: Mutex<VecDeque<PacketEntry>>,
}

impl PacketRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(VecDeque::with_capacity(capacity.min(256))),
        }
    }

    /// Appends an entry, dropping the oldest when full. A poisoned/contended ring drops the
    /// entry instead of blocking a packet path.
    pub fn push(&self, entry: PacketEntry) {
        let Ok(mut entries) = self.entries.try_lock() else {
            return;
        };
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// The current contents, oldest first.
    pub fn snapshot(&self) -> Vec<PacketEntry> {
        self.entries
            .lock()
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// `fmt::Write` sink that stops the `Debug` implementation once `max` chars were written.
struct LimitedWriter {
    out: String,
    remaining: usize,
}

impl fmt::Write for LimitedWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.remaining == 0 {
            return Err(fmt::Error);
        }
        let count = s.chars().count();
        if count <= self.remaining {
            self.out.push_str(s);
            self.remaining -= count;
            return Ok(());
        }
        self.out.extend(s.chars().take(self.remaining));
        self.out.push('…');
        self.remaining = 0;
        Err(fmt::Error)
    }
}

fn debug_prefix<T: fmt::Debug + ?Sized>(value: &T, max: usize) -> String {
    use std::fmt::Write;
    let mut writer = LimitedWriter {
        out: String::with_capacity(max.min(256)),
        remaining: max,
    };
    let _ = write!(&mut writer, "{value:?}");
    writer.out
}

fn hex_prefix(bytes: &[u8], max: usize) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len().min(max) * 3);
    for b in bytes.iter().take(max) {
        let _ = write!(out, "{b:02x} ");
    }
    if bytes.len() > max {
        out.push('…');
    }
    out
}

/// Records a packet the server encoded for the client.
pub fn record_outgoing(ring: Option<&Arc<PacketRing>>, packet: &dyn ServerPacket, size: usize) {
    let Some(ring) = ring else { return };
    ring.push(PacketEntry {
        direction: "out",
        level: packet.level(),
        opcode: packet.opcode(),
        name: packet.name(),
        size,
        timestamp_ms: crate::world::now_ms(),
        body: debug_prefix(packet, BODY_CHARS),
    });
}

/// Records a decoded client packet before its handler runs.
pub fn record_incoming(
    ring: Option<&Arc<PacketRing>>,
    level: u8,
    opcode: u16,
    name: &'static str,
    body: &[u8],
) {
    let Some(ring) = ring else { return };
    ring.push(PacketEntry {
        direction: "in",
        level,
        opcode,
        name,
        size: body.len(),
        timestamp_ms: crate::world::now_ms(),
        body: hex_prefix(body, BODY_BYTES),
    });
}

/// Records a client packet a server-side caller dispatched directly (the bots): the same `in`
/// entry as [`record_incoming`], with the packet's `Debug` as the body (there is no wire body).
pub fn record_incoming_packet<T: fmt::Debug + ?Sized>(
    ring: Option<&Arc<PacketRing>>,
    level: u8,
    opcode: u16,
    name: &'static str,
    packet: &T,
) {
    let Some(ring) = ring else { return };
    ring.push(PacketEntry {
        direction: "in",
        level,
        opcode,
        name,
        size: 0,
        timestamp_ms: crate::world::now_ms(),
        body: debug_prefix(packet, BODY_CHARS),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(opcode: u16) -> PacketEntry {
        PacketEntry {
            direction: "in",
            level: 1,
            opcode,
            name: "X",
            size: 0,
            timestamp_ms: 0,
            body: String::new(),
        }
    }

    #[test]
    fn ring_keeps_only_the_last_capacity_entries() {
        let ring = PacketRing::new(2);
        ring.push(entry(1));
        ring.push(entry(2));
        ring.push(entry(3));
        let ops: Vec<u16> = ring.snapshot().iter().map(|e| e.opcode).collect();
        assert_eq!(ops, [2, 3]);
    }

    #[test]
    fn body_snippets_are_capped() {
        assert_eq!(hex_prefix(&[0xAB], 4), "ab ");
        assert!(hex_prefix(&[0; 10], 4).ends_with('…'));
        assert!(debug_prefix("abcdef", 6).ends_with('…'));
        assert_eq!(debug_prefix("abc", 32), "\"abc\"");
    }
}

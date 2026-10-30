//! Looting after a kill, through the same `CSLootOpenBagPacket` a client sends.
//!
//! The server's looting container on the corpse decides what may be taken (tagging, the
//! loot rule, the take range); `loot_all` is the client's "take everything" button. A bot walks
//! into `LOOT_TAKE_RANGE` first because a real client does, and because the take range is
//! checked server-side.

use std::sync::Arc;

use aaemu_game::server::GameServer;
use aaemu_game::systems::economy::looting::{CSLootOpenBagPacket, LOOT_TAKE_RANGE};
use aaemu_game::world::WorldObject;

use crate::host::BotSession;

use super::target::dist2;
use super::CombatState;

/// Opens the corpse's loot bag and takes everything the server allows.
pub async fn open_corpse(
    bot: &Arc<BotSession>,
    server: &GameServer,
    state: &mut CombatState,
    corpse: u32,
) -> bool {
    let Some(items) = loot_count(server, corpse) else {
        return false;
    };
    if items == 0 {
        return false;
    }
    let packet = CSLootOpenBagPacket {
        obj_id: corpse,
        obj2_id: 0,
        loot_all: true,
    };
    let _ = bot.dispatch(server, packet).await;
    let left = loot_count(server, corpse).unwrap_or(0);
    state.looted_items += (items - left) as u32;
    tracing::debug!(
        character = %bot.name,
        corpse,
        items,
        left,
        "Bots: opened the corpse's loot"
    );
    true
}

/// How many items the corpse's looting container still holds (`None` without a container).
pub fn loot_count(server: &GameServer, corpse: u32) -> Option<usize> {
    server.systems.economy.loot.get(corpse).map(|c| {
        c.lock()
            .expect("loot container poisoned")
            .items
            .len()
    })
}

/// Where a corpse stands (its object may already be gone).
pub fn corpse_position(server: &GameServer, corpse: u32) -> Option<(f32, f32)> {
    let object = server.world.object(corpse)?;
    let t = object.transform().local();
    Some((t.x, t.y))
}

/// Is the bot close enough for the server's take range?
pub fn in_loot_range(server: &GameServer, player_pos: (f32, f32), corpse: u32) -> bool {
    corpse_position(server, corpse)
        .is_some_and(|(x, y)| dist2(player_pos.0, player_pos.1, x, y) <= LOOT_TAKE_RANGE)
}

/// The loot object of a corpse as a world object (used to check it still exists).
pub fn corpse_object(server: &GameServer, corpse: u32) -> Option<WorldObject> {
    server.world.object(corpse)
}

#[cfg(test)]
mod tests {
    use aaemu_game::test_support::test_server;

    use super::*;

    #[tokio::test]
    async fn an_unknown_corpse_has_no_loot_and_no_position() {
        let server = test_server();
        assert!(loot_count(&server, 0x9999).is_none());
        assert!(corpse_position(&server, 0x9999).is_none());
        assert!(!in_loot_range(&server, (100.0, 100.0), 0x9999));
    }
}

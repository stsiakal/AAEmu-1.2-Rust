//! Budgeted slow invariant checks (every 5 s on the blocking pool).
//!
//! * unit HP below zero or above its maximum,
//! * a character / unit far below the terrain floor,
//! * a unit parented to an object that is not in the world,
//! * a slave whose position jumped more than [`SHIP_JUMP_CAP`] between two checks.
//!
//! Every check reports through the normal pipeline, so repeated hits dedup into one
//! fingerprint with counters.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::{report, Trigger};
use crate::server::GameServer;
use crate::world::{ObjectKind, WorldObject};

/// Timer period.
const INTERVAL: Duration = Duration::from_secs(5);
/// How far below the terrain floor a unit must be before it counts as "under the world".
const UNDER_WORLD_DEPTH: f32 = 200.0;
/// Maximum plausible slave movement between two checks (m); ships that launch/eject exceed it.
const SHIP_JUMP_CAP: f32 = 500.0;
/// NPCs checked per tick (`all_npcs` is rotated with a cursor).
const NPC_BUDGET: usize = 400;

#[derive(Default)]
struct State {
    slave_positions: HashMap<u32, (f32, f32, f32)>,
    npc_cursor: usize,
}

/// Starts the 5 s checker. No-op outside a Tokio runtime.
pub fn start_task(server: &Arc<GameServer>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval`'s first tick is immediate: skip it so startup work is not scanned.
        interval.tick().await;
        let mut state = State::default();
        loop {
            interval.tick().await;
            let Some(server) = weak.upgrade() else {
                break;
            };
            let checked = server.clone();
            state = match tokio::task::spawn_blocking(move || {
                check_with_state(&checked, &mut state);
                state
            })
            .await
            {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!("bugreport: invariant task failed: {e}");
                    break;
                }
            };
        }
    });
}

/// Runs every check once (tests, GM commands).
pub fn check_now(server: &GameServer) {
    let mut state = State::default();
    check_with_state(server, &mut state);
}

fn check_with_state(server: &GameServer, state: &mut State) {
    for player in server.world.all_characters() {
        let obj = WorldObject::Player(player);
        check_unit(server, &obj);
        check_floor(server, &obj);
        check_parent(server, &obj);
    }
    let worlds = server.world.worlds();
    let slaves: Vec<_> = worlds.iter().flat_map(|w| w.all_slaves()).collect();
    let mut positions = HashMap::with_capacity(slaves.len());
    for slave in slaves {
        let obj = WorldObject::Slave(slave);
        check_unit(server, &obj);
        check_parent(server, &obj);
        if let Some(pos) = check_slave_jump(server, &obj, state) {
            positions.insert(obj.obj_id(), pos);
        }
    }
    state.slave_positions = positions;

    let npcs: Vec<_> = worlds.iter().flat_map(|w| w.all_npcs()).collect();
    if !npcs.is_empty() {
        let start = state.npc_cursor % npcs.len();
        let count = npcs.len().min(NPC_BUDGET);
        for i in 0..count {
            let npc = &npcs[(start + i) % npcs.len()];
            let obj = WorldObject::Npc(npc.clone());
            check_unit(server, &obj);
            check_parent(server, &obj);
        }
        state.npc_cursor = (start + count) % npcs.len();
    }
}

fn check_unit(server: &GameServer, obj: &WorldObject) {
    let Some(unit) = obj.with_unit(|u| {
        let u = u.unit();
        (u.name.clone(), u.hp, u.max_hp)
    }) else {
        return;
    };
    let (name, hp, max_hp) = unit;
    if hp < 0 || (max_hp > 0 && hp > max_hp) {
        report(
            server,
            Trigger::invariant(
                "unit_hp",
                format!("unit {} ({name}) has hp {hp} (max {max_hp})", obj.obj_id()),
            ),
            Some(obj.obj_id()),
            None,
            json!({
                "check": "unit_hp",
                "kind": kind_name(obj.kind()),
                "name": name,
                "hp": hp,
                "max_hp": max_hp,
            }),
        );
    }
}

fn check_floor(server: &GameServer, obj: &WorldObject) {
    let world = server.world.world_of(obj);
    let Some(world) = world else {
        return;
    };
    let pos = world.world_pos_rot(obj);
    let Some(heightmaps) = server.systems.npcs.heightmaps.get() else {
        return;
    };
    let floor = heightmaps.get_height_at(Some(world.template.id), pos.x, pos.y, pos.z);
    if floor != 0.0 && pos.z < floor - UNDER_WORLD_DEPTH {
        report(
            server,
            Trigger::invariant(
                "under_world",
                format!(
                    "unit {} is {:.0} m below the terrain floor",
                    obj.obj_id(),
                    floor - pos.z
                ),
            ),
            Some(obj.obj_id()),
            None,
            json!({
                "check": "under_world",
                "kind": kind_name(obj.kind()),
                "position": [pos.x, pos.y, pos.z],
                "floor": floor,
                "depth": floor - pos.z,
            }),
        );
    }
}

fn check_parent(server: &GameServer, obj: &WorldObject) {
    let parent = obj.transform().parent_obj_id;
    if parent != 0 && server.world.object(parent).is_none() {
        report(
            server,
            Trigger::invariant(
                "missing_parent",
                format!("unit {} has a missing parent {parent}", obj.obj_id()),
            ),
            Some(obj.obj_id()),
            Some(parent),
            json!({
                "check": "missing_parent",
                "kind": kind_name(obj.kind()),
                "parent_obj_id": parent,
            }),
        );
    }
}

/// `Some(position)` = the position stored for the next check.
fn check_slave_jump(
    server: &GameServer,
    obj: &WorldObject,
    state: &State,
) -> Option<(f32, f32, f32)> {
    let world = server.world.world_of(obj)?;
    let pos = world.world_pos_rot(obj);
    let current = (pos.x, pos.y, pos.z);
    if let Some(previous) = state.slave_positions.get(&obj.obj_id()) {
        let (dx, dy, dz) = (
            current.0 - previous.0,
            current.1 - previous.1,
            current.2 - previous.2,
        );
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        if distance > SHIP_JUMP_CAP {
            report(
                server,
                Trigger::invariant(
                    "slave_position_jump",
                    format!(
                        "slave {} jumped {:.0} m between checks",
                        obj.obj_id(),
                        distance
                    ),
                ),
                Some(obj.obj_id()),
                None,
                json!({
                    "check": "slave_position_jump",
                    "from": [previous.0, previous.1, previous.2],
                    "to": [current.0, current.1, current.2],
                    "distance": distance,
                    "cap": SHIP_JUMP_CAP,
                }),
            );
        }
    }
    Some(current)
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Player => "player",
        ObjectKind::Npc => "npc",
        ObjectKind::Doodad => "doodad",
        ObjectKind::Slave => "slave",
        ObjectKind::Mate => "mate",
        ObjectKind::House => "house",
        ObjectKind::Transfer => "transfer",
        ObjectKind::Shipyard => "shipyard",
        ObjectKind::Gimmick => "gimmick",
    }
}

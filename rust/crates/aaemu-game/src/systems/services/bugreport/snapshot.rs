//! World snapshots for a report: actor, target, nearby units / doodads, packet ring, logs and
//! the ship audit.
//!
//! Locking: every helper copies one object's data and drops the guard before looking at the
//! next object or calling into the world (see `world/object.rs` locking rules). The writer
//! thread is the only caller, so a slow snapshot never delays a packet or a tick.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::models::item::Item;
use crate::models::unit::UnitBase;
use crate::server::GameServer;
use crate::systems::vehicles::audit;
use crate::world::{ObjectKind, PlayerRef, SlaveRef, WorldObject};

use super::packet_ring::PacketEntry;

/// One captured log event (the bounded ring behind `logs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub timestamp: String,
    pub timestamp_ms: i64,
    pub level: String,
    pub target: String,
    pub message: String,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

pub struct Snapshot {
    pub actor: Value,
    pub target: Value,
    pub nearby: Value,
    pub ship_audit: Value,
    pub packets: Value,
    pub logs: Vec<Value>,
}

/// Everything the writer needs at report time.
pub fn collect(
    server: &GameServer,
    actor: Option<u32>,
    target: Option<u32>,
    observed: &[u32],
    radius: f32,
    log_ring: &[LogLine],
) -> Snapshot {
    let mut actor = actor.and_then(|id| server.world.object(id));
    let mut target = target.and_then(|id| server.world.object(id));
    for id in observed {
        let Some(obj) = server.world.object(*id) else {
            continue;
        };
        if actor.is_none() && obj.kind() == ObjectKind::Player {
            actor = Some(obj);
        } else if target.is_none() {
            target = Some(obj);
        }
    }

    let focus = actor.clone().or_else(|| target.clone());
    let nearby = match &focus {
        Some(focus) => nearby(server, focus, radius),
        None => json!({"radius": radius, "units": [], "doodads": []}),
    };
    let packets = packets(
        actor
            .as_ref()
            .and_then(player_of)
            .or_else(|| target.as_ref().and_then(player_of)),
    );
    let logs = logs_for(log_ring, &ids_of(actor.as_ref(), target.as_ref()));
    let ship_audit = ship_audit(server, actor.as_ref(), target.as_ref());

    Snapshot {
        actor: actor
            .as_ref()
            .map_or(Value::Null, |o| object_fields(server, o)),
        target: target
            .as_ref()
            .map_or(Value::Null, |o| object_fields(server, o)),
        nearby,
        ship_audit,
        packets,
        logs,
    }
}

fn player_of(obj: &WorldObject) -> Option<&PlayerRef> {
    obj.as_player()
}

fn ids_of(actor: Option<&WorldObject>, target: Option<&WorldObject>) -> Vec<u32> {
    let mut ids = Vec::new();
    for obj in [actor, target].into_iter().flatten() {
        if !ids.contains(&obj.obj_id()) {
            ids.push(obj.obj_id());
        }
        if let Some(p) = obj.as_player() {
            if !ids.contains(&p.character_id) {
                ids.push(p.character_id);
            }
        }
    }
    ids
}

fn packets(player: Option<&PlayerRef>) -> Value {
    let Some(player) = player else {
        return json!({"in": [], "out": []});
    };
    let Some(ring) = player.conn.packet_ring() else {
        return json!({"in": [], "out": []});
    };
    let (out, incoming): (Vec<PacketEntry>, Vec<PacketEntry>) = ring
        .snapshot()
        .into_iter()
        .partition(|e| e.direction == "out");
    json!({
        "in": incoming.into_iter().map(packet_row).collect::<Vec<_>>(),
        "out": out.into_iter().map(packet_row).collect::<Vec<_>>(),
    })
}

/// Auth / login / ticket / key packets never expose their body in a report.
fn packet_row(mut entry: PacketEntry) -> PacketEntry {
    let name = entry.name.to_ascii_lowercase();
    if [
        "auth",
        "login",
        "ticket",
        "token",
        "password",
        "passwd",
        "secret",
        "key",
        "enterworld",
    ]
    .iter()
    .any(|part| name.contains(part))
    {
        entry.body = "[redacted]".into();
    }
    entry
}

/// Log lines that mention the actor's / target's object or character id.
fn logs_for(ring: &[LogLine], ids: &[u32]) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    ring.iter()
        .filter(|line| mentions(line, ids))
        .map(|line| json!(line))
        .collect()
}

fn mentions(line: &LogLine, ids: &[u32]) -> bool {
    let mut text = line.message.clone();
    for value in line.fields.values() {
        text.push(' ');
        text.push_str(value);
    }
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<u32>().ok())
        .any(|n| ids.contains(&n))
}

fn nearby(server: &GameServer, focus: &WorldObject, radius: f32) -> Value {
    let focus_pos = world_pos(server, focus);
    let mut units = Vec::new();
    let mut doodads = Vec::new();
    let Some(world) = server.world.world_of(focus) else {
        return json!({"radius": radius, "units": [], "doodads": []});
    };
    for obj in world.objects_around(focus) {
        let pos = world_pos(server, &obj);
        let dx = pos.x - focus_pos.x;
        let dy = pos.y - focus_pos.y;
        let dz = pos.z - focus_pos.z;
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        if distance > radius {
            continue;
        }
        let mut row = Map::new();
        row.insert("obj_id".into(), json!(obj.obj_id()));
        row.insert("kind".into(), json!(kind_name(obj.kind())));
        row.insert("distance".into(), json!(distance));
        row.insert("position".into(), position(pos));
        if let Some(unit) = obj.with_unit(|u| {
            let u = u.unit();
            json!({
                "name": u.name,
                "template_id": u.template_id,
                "level": u.level,
                "hp": u.hp,
                "max_hp": u.max_hp,
                "is_dead": u.is_dead(),
            })
        }) {
            if let Value::Object(fields) = unit {
                row.extend(fields);
            }
            units.push(Value::Object(row));
        } else if let Some(doodad) = obj.as_doodad() {
            let d = doodad.data();
            row.insert("name".into(), json!(d.name));
            row.insert("template_id".into(), json!(d.template_id));
            row.insert("db_id".into(), json!(d.db_id));
            row.insert("hp".into(), Value::Null);
            drop(d);
            doodads.push(Value::Object(row));
        }
    }
    units.sort_by(|a, b| {
        a["distance"]
            .as_f64()
            .unwrap_or(f64::MAX)
            .total_cmp(&b["distance"].as_f64().unwrap_or(f64::MAX))
    });
    doodads.sort_by(|a, b| {
        a["distance"]
            .as_f64()
            .unwrap_or(f64::MAX)
            .total_cmp(&b["distance"].as_f64().unwrap_or(f64::MAX))
    });
    units.truncate(128);
    doodads.truncate(128);
    json!({"radius": radius, "units": units, "doodads": doodads})
}

fn world_pos(
    server: &GameServer,
    obj: &WorldObject,
) -> crate::models::transform::PositionAndRotation {
    server
        .world
        .world_of(obj)
        .map(|w| w.world_pos_rot(obj))
        .unwrap_or_else(|| obj.transform().local())
}

fn position(pos: crate::models::transform::PositionAndRotation) -> Value {
    json!({
        "x": pos.x,
        "y": pos.y,
        "z": pos.z,
        "rot_x": pos.rot_x,
        "rot_y": pos.rot_y,
        "rot_z": pos.rot_z,
    })
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

fn object_fields(server: &GameServer, obj: &WorldObject) -> Value {
    let local = obj.transform();
    let mut map = Map::new();
    map.insert("obj_id".into(), json!(obj.obj_id()));
    map.insert("kind".into(), json!(kind_name(obj.kind())));
    map.insert("visible".into(), json!(obj.is_visible()));
    map.insert("zone_id".into(), json!(local.zone_id));
    map.insert("instance_id".into(), json!(local.instance_id));
    map.insert("parent_obj_id".into(), json!(local.parent_obj_id));
    map.insert("sticky_parent_id".into(), json!(obj.core().sticky_parent()));
    map.insert("character_id".into(), Value::Null);
    map.insert("money".into(), Value::Null);
    map.insert("labor_power".into(), Value::Null);
    let mut equipment = Value::Null;

    match obj {
        WorldObject::Player(p) => {
            let d = p.data();
            map.insert("character_id".into(), json!(d.id));
            map.insert("account_id".into(), json!(d.account_id));
            map.insert("money".into(), json!(d.money));
            map.insert("labor_power".into(), json!(d.labor_power));
            map.insert("is_online".into(), json!(d.is_online));
            fill_unit(&mut map, &d.unit);
            let container = d.inventory.equipment().clone();
            drop(d);
            let slots = container
                .lock()
                .expect("equipment container poisoned")
                .slotted_items();
            equipment = item_slots(&slots);
        }
        WorldObject::Npc(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("spawner_id".into(), json!(d.spawner_id));
            equipment = item_slots(&d.equipment);
            drop(d);
        }
        WorldObject::Doodad(e) => {
            let d = e.data();
            map.insert("name".into(), json!(d.name));
            map.insert("template_id".into(), json!(d.template_id));
            map.insert("faction_id".into(), json!(d.faction_id));
            map.insert("db_id".into(), json!(d.db_id));
            map.insert("owner_id".into(), json!(d.owner_id));
            map.insert("item_template_id".into(), json!(d.item_template_id));
            drop(d);
        }
        WorldObject::Slave(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("slave_id".into(), json!(d.id));
            map.insert("summoner_id".into(), json!(d.summoner_id));
            map.insert("summoner_obj_id".into(), json!(d.summoner_obj_id));
            map.insert("summoning_item_id".into(), json!(d.summoning_item_id));
            map.insert("owner_obj_id".into(), json!(d.owner_obj_id));
            map.insert("attach_point_id".into(), json!(d.attach_point_id));
            map.insert("attached_characters".into(), json!(d.attached_characters));
            map.insert("attached_doodads".into(), json!(d.attached_doodads));
            map.insert("attached_slaves".into(), json!(d.attached_slaves));
            equipment = item_slots(&d.equipment);
            drop(d);
        }
        WorldObject::Mate(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("mate_id".into(), json!(d.id));
            map.insert("owner_obj_id".into(), json!(d.owner_obj_id));
            equipment = item_slots(&d.equipment);
            drop(d);
        }
        WorldObject::House(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("house_id".into(), json!(d.id));
            drop(d);
        }
        WorldObject::Transfer(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("bonding_obj_id".into(), json!(d.bonding_obj_id));
            drop(d);
        }
        WorldObject::Shipyard(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("shipyard_id".into(), json!(d.data.id));
            drop(d);
        }
        WorldObject::Gimmick(e) => {
            let d = e.data();
            fill_unit(&mut map, &d.unit);
            map.insert("gimmick_id".into(), json!(d.gimmick_id));
            drop(d);
        }
    }

    let pos = world_pos(server, obj);
    map.insert("position".into(), position(pos));
    map.insert("equipment".into(), equipment);
    Value::Object(map)
}

fn fill_unit(map: &mut Map<String, Value>, unit: &UnitBase) {
    map.insert("name".into(), json!(unit.name));
    map.insert("template_id".into(), json!(unit.template_id));
    map.insert("level".into(), json!(unit.level));
    map.insert("hp".into(), json!(unit.hp));
    map.insert("max_hp".into(), json!(unit.max_hp));
    map.insert("mp".into(), json!(unit.mp));
    map.insert("max_mp".into(), json!(unit.max_mp));
    map.insert("faction_id".into(), json!(unit.faction_id));
    map.insert("owner_id".into(), json!(unit.owner_id));
    map.insert("current_target".into(), json!(unit.current_target));
    map.insert("is_dead".into(), json!(unit.is_dead()));
    map.insert("is_in_battle".into(), json!(unit.is_in_battle));
    map.insert(
        "buffs".into(),
        json!(unit
            .buffs
            .effects
            .iter()
            .map(|b| b.template.id)
            .collect::<Vec<_>>()),
    );
}

fn item_slots(slots: &[Option<Item>]) -> Value {
    Value::Array(
        slots
            .iter()
            .enumerate()
            .filter_map(|(slot, item)| {
                let item = item.as_ref()?;
                Some(json!({
                    "slot": slot,
                    "item_id": item.id,
                    "template_id": item.template_id,
                    "count": item.count,
                    "grade": item.grade,
                }))
            })
            .collect(),
    )
}

/// A compact `systems/vehicles/audit.rs` snapshot when the actor or target is a slave or is
/// attached to one.
fn ship_audit(
    server: &GameServer,
    actor: Option<&WorldObject>,
    target: Option<&WorldObject>,
) -> Value {
    let mut seen = Vec::new();
    for obj in [actor, target].into_iter().flatten() {
        let Some(slave) = slave_of(server, obj) else {
            continue;
        };
        if seen.contains(&slave.obj_id) {
            continue;
        }
        seen.push(slave.obj_id);
        let (summoner_id, item_id) = {
            let d = slave.data();
            (d.summoner_id, d.summoning_item_id)
        };
        if item_id == 0 {
            return json!({
                "slave_obj_id": slave.obj_id,
                "source": "systems/vehicles/audit.rs",
                "audit": Value::Null,
                "note": "slave has no summoning item",
            });
        }
        let request = audit::Request {
            character_id: summoner_id,
            item_id,
            nonce: "bugreport".into(),
        };
        return json!({
            "slave_obj_id": slave.obj_id,
            "source": "systems/vehicles/audit.rs",
            "audit": audit::snapshot(server, &request),
        });
    }
    Value::Null
}

fn slave_of(server: &GameServer, obj: &WorldObject) -> Option<SlaveRef> {
    if let Some(slave) = obj.as_slave() {
        return Some(slave.clone());
    }
    let local = obj.transform();
    let mut parents = vec![local.parent_obj_id, obj.core().sticky_parent()];
    parents.retain(|id| *id != 0);
    parents.sort_unstable();
    parents.dedup();
    for id in parents {
        if let Some(slave) = server.world.object(id).and_then(|o| o.as_slave().cloned()) {
            return Some(slave);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};
    use std::sync::Arc;

    #[tokio::test]
    async fn object_snapshot_has_unit_equipment_and_position() {
        let server = Arc::new(test_server());
        server.set_self();
        let client = TestClient::new(&server, 1, 1001);
        let p = spawn_player(
            &server,
            &client,
            test_character(1, "Bob", 10.0, 20.0),
            0x100,
        );
        let fields = object_fields(&server, &WorldObject::Player(p.clone()));
        assert_eq!(fields["obj_id"], 0x100);
        assert_eq!(fields["name"], "Bob");
        assert_eq!(fields["level"], 10);
        assert_eq!(fields["zone_id"], crate::test_support::TEST_ZONE);
        assert_eq!(fields["character_id"], 1);
        assert!(fields["equipment"].is_array());
        assert!(fields["buffs"].is_array());
    }

    #[test]
    fn auth_and_key_packet_bodies_are_redacted() {
        let entry = |name: &'static str| PacketEntry {
            direction: "out",
            level: 1,
            opcode: 1,
            name,
            size: 6,
            timestamp_ms: 0,
            body: "secret".into(),
        };
        assert_eq!(
            packet_row(entry("X2EnterWorldResponsePacket")).body,
            "[redacted]"
        );
        assert_eq!(
            packet_row(entry("SCSecondPassKeyTablesPacket")).body,
            "[redacted]"
        );
        assert_eq!(packet_row(entry("SCUnitPointsPacket")).body, "secret");
    }

    #[test]
    fn log_lines_mentioning_the_actor_are_selected() {
        let line = |message: &str| LogLine {
            timestamp: String::new(),
            timestamp_ms: 0,
            level: "WARN".into(),
            target: "test".into(),
            message: message.into(),
            fields: BTreeMap::new(),
        };
        let ring = vec![line("unit 42 exploded"), line("unit 43 is fine")];
        let logs = logs_for(&ring, &[42]);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["message"], "unit 42 exploded");
    }
}

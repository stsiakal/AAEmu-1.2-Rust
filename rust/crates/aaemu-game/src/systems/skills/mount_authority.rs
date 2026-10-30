//! Client-side mate cast authority. Skill eligibility and slave seat mapping are separate rules.

use crate::systems::vehicles::data::attach_point;
use crate::world::{MateRef, PlayerRef, SlaveRef, WorldObject};

pub(super) fn may_use_mate(player: &PlayerRef, mate: &MateRef) -> bool {
    let (point, riding, parent) = {
        let player = player.data();
        (
            player.attached_point,
            player.is_riding,
            player.unit.transform.parent_obj_id,
        )
    };
    let mate_data = mate.data();
    if mate_data.owner_obj_id == player.obj_id {
        return true; // Pet commands also work while the owner is unmounted.
    }
    riding
        && point != attach_point::NONE
        && parent == mate.obj_id
        && mate_data
            .passenger(point)
            .is_some_and(|seat| seat.obj_id == player.obj_id)
}

/// Whether `player` summoned `slave` (by object id, or by character id after a relog).
fn summoned_by(slave: &SlaveRef, player: &PlayerRef, character_id: u32) -> bool {
    let data = slave.data();
    data.summoner_obj_id == player.obj_id || (character_id != 0 && data.summoner_id == character_id)
}

/// A slave (ship, cannon, cart, ...) answers the skills of its owner and of the characters
/// attached to it (driver, gunner, passengers); a harpoon child also answers the owner of the
/// slave it is mounted on. A ship cannon answers only the character manning it.
pub(super) fn may_use_slave(
    server: &crate::server::GameServer,
    player: &PlayerRef,
    slave: &SlaveRef,
) -> bool {
    let (parent, point) = {
        let player = player.data();
        (player.unit.transform.parent_obj_id, player.attached_point)
    };
    let character_id = player.character_id;
    // A cannon mounted on a ship answers its gunner only: manning it is what grants the shot.
    if crate::systems::vehicles::slaves::is_manned_cannon(server, slave) {
        return parent == slave.obj_id
            && point == attach_point::DRIVER
            && slave
                .data()
                .attached_characters
                .iter()
                .any(|(seat, obj)| *seat == attach_point::DRIVER && *obj == player.obj_id);
    }
    if parent == slave.obj_id || summoned_by(slave, player, character_id) {
        return true;
    }
    let (attached, bearer) = {
        let data = slave.data();
        (
            data.attached_characters
                .iter()
                .any(|(_, obj)| *obj == player.obj_id),
            data.owner_obj_id,
        )
    };
    if attached {
        return true;
    }
    // A child slave mounted on another slave of the player (one level, like the attach tree).
    bearer != 0
        && super::geometry::world_of(server, &WorldObject::from(player))
            .and_then(|w| w.slave(bearer))
            .is_some_and(|bearer| summoned_by(&bearer, player, character_id))
}

/// Who may a `Mount` caster packet of `player` name as the caster: a mate the player may use or
/// a slave the player owns or is attached to. Any other object (the player itself, an NPC, a
/// doodad, another player, a foreign slave) is refused: a "mount" cast is a vehicle bar skill,
/// never a way to cast a player skill without its ability checks.
pub(super) fn may_cast_as(
    server: &crate::server::GameServer,
    player: &PlayerRef,
    caster: &WorldObject,
) -> bool {
    match caster {
        WorldObject::Mate(mate) => may_use_mate(player, mate),
        WorldObject::Slave(slave) => may_use_slave(server, player, slave),
        _ => false,
    }
}

/// Whether `skill_id` is on the mount / slave bar of `caster`: the mate's `skills` entries and
/// the slave template's `slave_mount_skills`, each a `mount_skills.id` that maps to the skill the
/// client casts, plus the entries `buff_mount_skills` grants through the vehicle's active buffs.
/// The bar also decides the rider / operator skill through `mount_attached_skills`.
pub(super) fn may_use_vehicle_skill(
    server: &crate::server::GameServer,
    caster: &WorldObject,
    skill_id: u32,
) -> bool {
    let data = &server.game_data.systems.vehicles;
    let mut mount_ids = Vec::new();
    match caster {
        WorldObject::Mate(mate) => mount_ids.extend(mate.data().skills.iter().copied()),
        WorldObject::Slave(slave) => {
            mount_ids.extend(data.slave_mount_skill_list(slave.data().unit.template_id))
        }
        _ => return false,
    }
    if mount_ids
        .iter()
        .any(|id| data.mount_skill_skill_id(*id) == skill_id)
    {
        return true;
    }
    let buffs: Vec<u32> = caster
        .with_unit(|u| {
            u.unit()
                .buffs
                .effects
                .iter()
                .map(|b| b.template.id)
                .collect()
        })
        .unwrap_or_default();
    buffs.iter().any(|buff_id| {
        data.buff_mount_skill_ids(*buff_id)
            .any(|id| data.mount_skill_skill_id(id) == skill_id)
    })
}

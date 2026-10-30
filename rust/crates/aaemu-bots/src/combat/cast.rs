//! Casting like a client: face the target with a real `CSMoveUnitPacket`, send
//! `CSStartSkillPacket` through the bot's socketless connection, and read the server's answer
//! from its own unit state plus the `SCSkillStarted` / `SCSkillFired` / `SCSkillEnded` frames.
//!
//! A cast is *taken* when the server changed one of the marks a use leaves on the unit
//! (`skill_last_used_ms`, the global cooldown, the skill cooldown, a cast task, the auto-attack
//! slot). When nothing changed the server refused it (its `start_skill` returns before any cast
//! state is touched); the skill is then backed off for this target, position and loadout so a
//! refusal can never become a tight retry loop. The server's quiet `precheck` runs before every
//! cast, so a refusal normally means a divergence worth a log line.

use std::sync::Arc;

use aaemu_game::models::movement::{
    GameStanceType, MoveKind, MoveType, MoveTypeActorFlags, MoveTypeAlertness, MoveTypeFlags,
    UnitMove,
};
use aaemu_game::opcodes::sc;
use aaemu_game::packets::c2g_world::CSMoveUnitPacket;
use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::controllers::roll_pitch_yaw_sbytes_movement;
use aaemu_game::systems::skills::handlers::CSStartSkillPacket;
use aaemu_game::systems::skills::types::{
    skill_result, SkillCastTarget, SkillCaster, SkillObject,
};
use aaemu_game::world::PlayerRef;

use crate::host::BotSession;

use super::CombatState;

/// How long a skill the server refused on this target / spot / loadout stays out of the
/// rotation. Longer than the global cooldown, so a refusal cannot become a retry loop, and
/// short enough that a transient reason (a moving target, a server-side race) recovers.
pub const REFUSAL_BACKOFF_MS: i64 = 1_500;

/// What a refusal was tied to: the skill is not retried on the same target from the same 2 m
/// cell with the same loadout until [`REFUSAL_BACKOFF_MS`] passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalKey {
    pub target: u32,
    pub cell: (i32, i32),
    pub loadout: u64,
    pub until: i64,
}

impl RefusalKey {
    pub fn matches(&self, target: u32, cell: (i32, i32), loadout: u64, now: i64) -> bool {
        now < self.until && self.target == target && self.cell == cell && self.loadout == loadout
    }
}

/// What a cast attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastOutcome {
    /// The server took the cast.
    Started,
    /// The target moved out of (or too close into) the skill's range between the precheck and
    /// the server's check: close in rather than back the skill off.
    OutOfRange,
    /// The unit was on cooldown / already casting: wait, do not back the skill off.
    Cooldown,
    /// It refused for another reason; the skill is in back-off.
    Refused,
}

/// The result byte of the last `SCSkillStarted` in `frames` (`None` when absent or unparsed).
fn cast_result(frames: &[(u16, Vec<u8>)]) -> Option<u8> {
    use aaemu_commons::PacketStream;

    for (op, body) in frames.iter().rev() {
        if *op != sc::SCSkillStartedPacket {
            continue;
        }
        let mut s = PacketStream::from_bytes(body.clone());
        let parsed = (|| -> Option<u8> {
            let _id = s.read_u32().ok()?;
            let _tl = s.read_u16().ok()?;
            let caster_type = s.read_u8().ok()?;
            SkillCaster::read(caster_type, &mut s).ok()?;
            let target_type = s.read_u8().ok()?;
            SkillCastTarget::read(target_type, &mut s).ok()?;
            let object_flag = s.read_u8().ok()?;
            SkillObject::read(object_flag, &mut s).ok()?;
            let _real_cast = s.read_u16().ok()?;
            let _base_cast = s.read_u16().ok()?;
            let _synergy = s.read_u8().ok()?;
            let flags = s.read_u8().ok()?;
            if flags & 1 != 0 {
                s.read_u8().ok()
            } else {
                Some(0)
            }
        })();
        if parsed.is_some() {
            return parsed;
        }
    }
    None
}

/// The `S->C` skill frames the server wrote to this bot during a cast (what a client sees).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CastReplies {
    pub started: usize,
    pub fired: usize,
    pub ended: usize,
}

impl CastReplies {
    pub fn of(frames: &[(u16, Vec<u8>)]) -> Self {
        let mut replies = CastReplies::default();
        for (op, _) in frames {
            match *op {
                sc::SCSkillStartedPacket => replies.started += 1,
                sc::SCSkillFiredPacket => replies.fired += 1,
                sc::SCSkillEndedPacket => replies.ended += 1,
                _ => {}
            }
        }
        replies
    }
}

/// The marks one cast use leaves on a unit, read before and after the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CastMarks {
    last_used: i64,
    gcd: i64,
    cooldown: Option<i64>,
    casting: bool,
    auto: bool,
}

fn cast_marks(player: &PlayerRef, skill_id: u32) -> CastMarks {
    let c = player.data();
    CastMarks {
        last_used: c.unit.skill_last_used_ms,
        gcd: c.unit.global_cooldown_ms,
        cooldown: c.unit.cooldowns.cooldowns.get(&skill_id).copied(),
        casting: c.unit.skill_state.skill_task.is_some(),
        auto: c.unit.is_auto_attack,
    }
}

/// Sends one `CSStartSkillPacket` through the real handler and reads whether the server took it.
#[allow(clippy::too_many_arguments)]
pub async fn cast(
    bot: &Arc<BotSession>,
    server: &GameServer,
    state: &mut CombatState,
    skill_id: u32,
    caster: SkillCaster,
    target_obj_id: u32,
    target_template: u32,
    now: i64,
) -> CastOutcome {
    let Some(player) = bot.player().await else {
        return CastOutcome::Refused;
    };
    let self_target = target_obj_id == 0 || target_obj_id == player.obj_id;
    if !self_target {
        face_target(bot, server, &player, target_obj_id).await;
    }
    let before = cast_marks(&player, skill_id);
    let packet = CSStartSkillPacket {
        skill_id,
        caster,
        target: SkillCastTarget::Unit(if self_target {
            player.obj_id
        } else {
            target_obj_id
        }),
        flag: 0,
        skill_object: SkillObject::None,
    };
    let _ = bot.dispatch(server, packet).await;
    state.last_cast_ms = now;
    let after = cast_marks(&player, skill_id);
    let frames = bot.drain_replies().await;
    let replies = CastReplies::of(&frames);
    // A refused cast also writes an `SCSkillStarted` (carrying the result), so only the unit
    // marks say whether the server took it.
    let took = after != before;
    if took {
        state.casts_started += 1;
        state.last_cast_skill = skill_id;
        state.refused_on.remove(&skill_id);
        let (x, y, _) = {
            let t = player.data().unit.transform;
            (t.x, t.y, t.z)
        };
        tracing::debug!(
            character = %bot.name,
            skill = skill_id,
            target = target_obj_id,
            fired = replies.fired,
            "Bots: cast started"
        );
        let _ = (x, y);
        CastOutcome::Started
    } else {
        match cast_result(&frames) {
            Some(result) if result == skill_result::TOO_FAR_RANGE || result == skill_result::TOO_CLOSE_RANGE => {
                state.casts_out_of_range += 1;
                return CastOutcome::OutOfRange;
            }
            Some(result) if result == skill_result::COOLDOWN_TIME || result == skill_result::ON_CASTING => {
                state.casts_on_cooldown += 1;
                return CastOutcome::Cooldown;
            }
            _ => {}
        }
        state.casts_refused += 1;
        let (x, y, _) = {
            let t = player.data().unit.transform;
            (t.x, t.y, t.z)
        };
        let loadout = state.loadout_signature(&player);
        state.refused_on.insert(
            skill_id,
            RefusalKey {
                target: target_obj_id,
                cell: ((x / 2.0).floor() as i32, (y / 2.0).floor() as i32),
                loadout,
                until: now + REFUSAL_BACKOFF_MS,
            },
        );
        tracing::info!(
            character = %bot.name,
            skill = skill_id,
            target = target_obj_id,
            target_template,
            "Bots: the server refused a cast every check said was usable; backing the skill off"
        );
        CastOutcome::Refused
    }
}

/// Uses a healing item (a potion) on the bot, like a client's item use.
pub async fn use_item(
    bot: &Arc<BotSession>,
    server: &GameServer,
    state: &mut CombatState,
    item_id: u64,
    item_template_id: u32,
    use_skill: u32,
    now: i64,
) -> CastOutcome {
    let Some(player) = bot.player().await else {
        return CastOutcome::Refused;
    };
    cast(
        bot,
        server,
        state,
        use_skill,
        SkillCaster::Item {
            obj_id: player.obj_id,
            item_id,
            item_template_id,
            type1: 0,
            type2: 0,
        },
        player.obj_id,
        0,
        now,
    )
    .await
}

/// Turns the bot toward the target with the stopping `CSMoveUnitPacket` a client sends when it
/// faces a direction (the angle check of a cast then sees the target in front).
async fn face_target(
    bot: &Arc<BotSession>,
    server: &GameServer,
    player: &PlayerRef,
    target_obj_id: u32,
) {
    let Some(target) = server.world.object(target_obj_id) else {
        return;
    };
    let (x, y, z) = {
        let t = player.data().unit.transform;
        (t.x, t.y, t.z)
    };
    let t = target.transform().local();
    let dx = t.x - x;
    let dy = t.y - y;
    if dx.abs() + dy.abs() < 0.01 {
        return;
    }
    let rot_z = (f64::from(dy).atan2(f64::from(dx)) - std::f64::consts::FRAC_PI_2) as f32;
    let (_, _, rotation_z) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, rot_z);
    let unit = UnitMove {
        x,
        y,
        z,
        rotation_z,
        stance: GameStanceType::COMBAT,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags: MoveTypeActorFlags(
            MoveTypeActorFlags::STANDING_ON_SOLID.0 | MoveTypeActorFlags::FLAG2.0,
        ),
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(unit));
    mt.flags = MoveTypeFlags::STOPPING;
    mt.time = bot.next_move_time_ms();
    let _ = bot
        .dispatch(
            server,
            CSMoveUnitPacket {
                obj_id: player.obj_id,
                move_type: mt,
            },
        )
        .await;
}

/// The stopping packet one face turn would send (for tests: opcode and rotation).
pub fn face_packet(player: &PlayerRef, target: (f32, f32), time: u32) -> CSMoveUnitPacket {
    let t = player.data().unit.transform;
    let rot_z = (f64::from(target.1 - t.y).atan2(f64::from(target.0 - t.x))
        - std::f64::consts::FRAC_PI_2) as f32;
    let (_, _, rotation_z) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, rot_z);
    let unit = UnitMove {
        x: t.x,
        y: t.y,
        z: t.z,
        rotation_z,
        stance: GameStanceType::COMBAT,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags: MoveTypeActorFlags(
            MoveTypeActorFlags::STANDING_ON_SOLID.0 | MoveTypeActorFlags::FLAG2.0,
        ),
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(unit));
    mt.flags = MoveTypeFlags::STOPPING;
    mt.time = time;
    CSMoveUnitPacket {
        obj_id: player.obj_id,
        move_type: mt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_frames_are_counted_from_the_opcodes() {
        let frames = vec![
            (sc::SCSkillStartedPacket, Vec::new()),
            (sc::SCSkillFiredPacket, Vec::new()),
            (sc::SCSkillEndedPacket, Vec::new()),
            (sc::SCUnitPointsPacket, Vec::new()),
        ];
        let replies = CastReplies::of(&frames);
        assert_eq!(replies.started, 1);
        assert_eq!(replies.fired, 1);
        assert_eq!(replies.ended, 1);
    }

    #[test]
    fn a_refused_skill_started_frame_carries_its_result() {
        use aaemu_commons::PacketStream;
        use aaemu_game::net::packet::ServerPacket;

        let mut packet = aaemu_game::systems::skills::packets::SCSkillStartedPacket::new(
            60001,
            7,
            SkillCaster::Unit(0x100),
            SkillCastTarget::Unit(0x400),
            SkillObject::None,
        );
        packet.set_skill_result(skill_result::TOO_FAR_RANGE);
        let mut s = PacketStream::new();
        packet.write_body(&mut s);
        let frames = vec![(sc::SCSkillStartedPacket, s.as_bytes().to_vec())];
        assert_eq!(cast_result(&frames), Some(skill_result::TOO_FAR_RANGE));

        let mut packet = aaemu_game::systems::skills::packets::SCSkillStartedPacket::new(
            60001,
            7,
            SkillCaster::Unit(0x100),
            SkillCastTarget::Unit(0x400),
            SkillObject::None,
        );
        packet.set_skill_result(skill_result::SUCCESS);
        let mut s = PacketStream::new();
        packet.write_body(&mut s);
        let frames = vec![(sc::SCSkillStartedPacket, s.as_bytes().to_vec())];
        assert_eq!(cast_result(&frames), Some(0));
    }

    #[test]
    fn a_refusal_key_covers_only_its_target_cell_and_loadout() {
        let key = RefusalKey {
            target: 7,
            cell: (1, 2),
            loadout: 9,
            until: 1_000,
        };
        assert!(key.matches(7, (1, 2), 9, 999));
        assert!(!key.matches(7, (1, 2), 9, 1_000), "the back-off expired");
        assert!(!key.matches(8, (1, 2), 9, 999), "another target");
        assert!(!key.matches(7, (1, 3), 9, 999), "another cell");
        assert!(!key.matches(7, (1, 2), 10, 999), "a new loadout");
    }
}

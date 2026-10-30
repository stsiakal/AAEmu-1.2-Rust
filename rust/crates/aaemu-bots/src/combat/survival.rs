//! Survival of the bot combat: resting between fights, healing, fleeing from monsters it cannot
//! fight, and the player-legal death / resurrection policy.
//!
//! Ported from `aaemu-game/src/systems/bots/combat.rs` and `mod.rs` (rest thresholds, the flee
//! direction, the revive choice). A dead bot waits its `RezWaitDuration` like a client, then
//! sends `CSResurrectCharacterPacket`: in place only when a resurrection offer is pending and
//! nothing dangerous stands near the body, otherwise at the temple / return point. No GM revive
//! is used.

use aaemu_game::models::character::Character;
use aaemu_game::server::GameServer;
use aaemu_game::world::PlayerRef;

use aaemu_game::world::NpcRef;

use super::target::{dist2, hostiles_near, me_pos, scan_npcs, HOSTILE_NEAR_CORPSE_M};
use super::CombatState;

/// A bot starts resting below this fraction of hit points / mana ...
pub const REST_BELOW: f32 = 0.25;
/// ... and fights again from this fraction.
pub const REST_UNTIL: f32 = 0.60;
/// Health fraction below which a fight uses heals / potions.
pub const HEAL_BELOW: f32 = 0.5;
/// Health / mana fraction a bot wants before it starts a fight.
pub const ENGAGE_MIN: f32 = 0.6;
/// A bot attacked below this health fraction runs instead of fighting (when it cannot heal).
pub const LOSING_BELOW: f32 = 0.25;
/// How far a bot runs from a monster it cannot fight (beyond its aggro and sight).
pub const FLEE_DISTANCE_M: f32 = 70.0;
/// A killer seen longer ago than this was not the one that killed the bot.
pub const KILLER_MEMORY_MS: i64 = 30_000;
/// The safe position is refreshed this often while the bot is calm and healthy.
const SAFE_POS_REFRESH_MS: i64 = 5_000;

/// A monster seen attacking the bot (the likely killer when it dies).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttackerSeen {
    pub obj_id: u32,
    pub level: u8,
    pub at_ms: i64,
}

/// Where a dead bot asks to be revived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviveChoice {
    pub in_place: bool,
    /// `safe`, `no_offer`, `killer_much_higher`, `killer_near` or `hostiles_near`.
    pub reason: &'static str,
}

/// Hit point / mana fractions of the bot and whether it is in battle.
pub fn vitals(player: &PlayerRef) -> (f32, f32, bool) {
    let c = player.data();
    let hp = c.unit.hp as f32 / c.unit.max_hp.max(1) as f32;
    let mp = if c.unit.max_mp > 0 {
        c.unit.mp as f32 / c.unit.max_mp as f32
    } else {
        1.0
    };
    (hp, mp, c.unit.is_in_battle)
}

/// Updates the resting flag: below `rest_below` out of combat the bot rests until
/// [`CombatState::rest_until`]; a monster that attacks it breaks the rest at once (a resting
/// bot that is hit fights back instead of sitting through the next 2 s tick). Returns `true`
/// while it rests.
pub fn update_rest(player: &PlayerRef, state: &mut CombatState, attacked: bool) -> bool {
    let (hp, mp, in_battle) = vitals(player);
    let low = hp.min(mp);
    if attacked {
        if state.resting {
            tracing::debug!(target: "bots", "Bots: rest interrupted by an attacker");
        }
        state.resting = false;
        return false;
    }
    if state.resting {
        if low >= state.rest_until() {
            state.resting = false;
            tracing::debug!(target: "bots", "Bots: rested (hp {hp:.2}, mp {mp:.2})");
        }
    } else if low < state.rest_below && !in_battle {
        state.resting = true;
        state.disengage();
        tracing::debug!(target: "bots", "Bots: resting (hp {hp:.2}, mp {mp:.2})");
    }
    state.resting
}

/// Remembers the bot's position as safe while it is healthy and not in a fight.
pub fn note_safe_position(player: &PlayerRef, state: &mut CombatState, now: i64) {
    let (hp, mp, in_battle) = vitals(player);
    if in_battle || hp.min(mp) < 0.5 || now - state.safe_pos_ms < SAFE_POS_REFRESH_MS {
        return;
    }
    let (x, y, _) = me_pos(player);
    state.safe_pos = Some((x, y));
    state.safe_pos_ms = now;
}

/// Where to run from `attacker`: back to the safe position when that lies away from it, else
/// straight away; every failed walk turns the direction by another 45 degrees.
pub fn flee_target(
    me: (f32, f32),
    attacker: (f32, f32),
    safe: Option<(f32, f32)>,
    fails: u8,
) -> (f32, f32) {
    let (dx, dy) = (me.0 - attacker.0, me.1 - attacker.1);
    let len = (dx * dx + dy * dy).sqrt();
    let (ux, uy) = if len < 0.01 {
        (1.0, 0.0)
    } else {
        (dx / len, dy / len)
    };
    if fails == 0 {
        if let Some(safe) = safe {
            let away = (safe.0 - me.0) * ux + (safe.1 - me.1) * uy > 0.0;
            if away && dist2(safe.0, safe.1, attacker.0, attacker.1) > len + 10.0 {
                return safe;
            }
        }
    }
    let turn = f32::from(fails.div_ceil(2))
        * std::f32::consts::FRAC_PI_4
        * if fails.is_multiple_of(2) { 1.0 } else { -1.0 };
    let (sin, cos) = turn.sin_cos();
    let (vx, vy) = (ux * cos - uy * sin, ux * sin + uy * cos);
    (me.0 + vx * FLEE_DISTANCE_M, me.1 + vy * FLEE_DISTANCE_M)
}

/// The revive policy of a sensible player: take the free in-place revive only when nothing that
/// can hurt a 1%-health character stands near the body; when the killer is still around or was
/// far above the bot's level, go back to the resurrection point (the temple) instead.
pub fn choose_revive(
    offer: bool,
    my_level: u8,
    gap: u8,
    killer: Option<&AttackerSeen>,
    killer_alive_near: bool,
    hostiles_near: bool,
    now: i64,
) -> ReviveChoice {
    if !offer {
        return ReviveChoice {
            in_place: false,
            reason: "no_offer",
        };
    }
    let killer = killer.filter(|k| now - k.at_ms <= KILLER_MEMORY_MS);
    let reason = if killer.is_some_and(|k| k.level > my_level.saturating_add(gap)) {
        "killer_much_higher"
    } else if killer_alive_near {
        "killer_near"
    } else if hostiles_near {
        "hostiles_near"
    } else {
        return ReviveChoice {
            in_place: true,
            reason: "safe",
        };
    };
    ReviveChoice {
        in_place: false,
        reason,
    }
}

/// [`choose_revive`] on the live world around the corpse.
pub fn revive_choice(
    server: &GameServer,
    player: &PlayerRef,
    state: &CombatState,
    now: i64,
) -> ReviveChoice {
    let (offer, my_level) = {
        let c = player.data();
        (c.skill_char.resurrect_offer, c.unit.level)
    };
    let gap = state.max_target_level_gap;
    let scan = scan_npcs(server, player, HOSTILE_NEAR_CORPSE_M);
    let near = hostiles_near(server, &scan, player, HOSTILE_NEAR_CORPSE_M);
    let killer = state.last_attacker;
    let killer_alive_near = killer.is_some_and(|k| near.iter().any(|v| v.obj_id == k.obj_id));
    choose_revive(
        offer,
        my_level,
        gap,
        killer.as_ref(),
        killer_alive_near,
        !near.is_empty(),
        now,
    )
}

/// Has the bot waited its resurrection delay?
pub fn may_resurrect(character: &Character, now_ms: i64) -> bool {
    if character.unit.hp > 0 {
        return false;
    }
    let dead_since = character.dead_time.saturating_mul(1000);
    now_ms.saturating_sub(dead_since) >= i64::from(character.rez_wait_duration) * 1000
}

/// Records the monster attacking the bot (the last one seen wins while it is fresh).
pub fn note_attacker(state: &mut CombatState, view: super::target::NpcView, now: i64) {
    state.last_attacker = Some(AttackerSeen {
        obj_id: view.obj_id,
        level: view.level,
        at_ms: now,
    });
}

/// A hostile monster within the aggro radius that is above the level gap (or a pack the bot
/// cannot survive): what it flees from.
pub fn threat_to_flee(
    all: &[NpcRef],
    player: &PlayerRef,
    state: &mut CombatState,
    now: i64,
    attacker: Option<super::target::NpcView>,
) -> Option<super::target::NpcView> {
    let (hp, _, _) = vitals(player);
    let attacker = attacker?;
    note_attacker(state, attacker, now);
    let my_level = player.data().unit.level;
    let too_strong = attacker.level > my_level.saturating_add(state.max_target_level_gap);
    let losing = hp < LOSING_BELOW && super::target::attackers_on_me(all, player) > 1;
    (too_strong || losing).then_some(attacker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn killer(level: u8, at_ms: i64) -> AttackerSeen {
        AttackerSeen {
            obj_id: 7,
            level,
            at_ms,
        }
    }

    #[test]
    fn rest_starts_below_the_threshold_and_ends_at_the_recovery_fraction() {
        // The flag logic is covered end-to-end in the flow test; the thresholds are the port.
        assert_eq!(
            CombatState::default().rest_until(),
            ENGAGE_MIN.max(REST_UNTIL)
        );
    }

    #[test]
    fn fleeing_prefers_the_safe_spot_only_when_it_is_away_from_the_attacker() {
        let me = (0.0, 0.0);
        let attacker = (10.0, 0.0);
        // Straight away from the attacker.
        let (x, y) = flee_target(me, attacker, None, 0);
        assert!(x < -60.0 && y.abs() < 0.01);
        // A safe spot behind the attacker is not taken.
        let (x, y) = flee_target(me, attacker, Some((50.0, 0.0)), 0);
        assert!(x < -60.0, "{x} {y}");
        // A safe spot away from it is.
        let (x, y) = flee_target(me, attacker, Some((-50.0, 0.0)), 0);
        assert_eq!((x, y), (-50.0, 0.0));
        // Each failed walk turns the direction.
        let (x1, y1) = flee_target(me, attacker, None, 1);
        let (x2, y2) = flee_target(me, attacker, None, 2);
        assert!((x1 - x2).abs() + (y1 - y2).abs() > 1.0);
    }

    #[test]
    fn the_revive_policy_goes_to_the_temple_when_the_killer_is_dangerous() {
        let now = 100_000;
        assert_eq!(
            choose_revive(false, 5, 3, None, false, false, now).reason,
            "no_offer"
        );
        assert_eq!(
            choose_revive(true, 5, 3, None, false, false, now).reason,
            "safe"
        );
        let strong = killer(20, now);
        assert_eq!(
            choose_revive(true, 5, 3, Some(&strong), false, false, now).reason,
            "killer_much_higher"
        );
        let near = killer(6, now);
        assert_eq!(
            choose_revive(true, 5, 3, Some(&near), true, false, now).reason,
            "killer_near"
        );
        assert_eq!(
            choose_revive(true, 5, 3, Some(&near), false, true, now).reason,
            "hostiles_near"
        );
        // A killer seen long ago is forgotten.
        let old = killer(6, now - KILLER_MEMORY_MS - 1);
        assert_eq!(
            choose_revive(true, 5, 3, Some(&old), false, false, now).reason,
            "safe"
        );
    }

    #[test]
    fn the_resurrection_delay_is_the_characters_own() {
        let mut c = Character::new(1, 1000, Default::default());
        c.unit.hp = 0;
        c.dead_time = 100;
        c.rez_wait_duration = 5;
        assert!(!may_resurrect(&c, 100_000));
        assert!(may_resurrect(&c, 105_000));
        c.unit.hp = 10;
        assert!(!may_resurrect(&c, 200_000));
    }
}

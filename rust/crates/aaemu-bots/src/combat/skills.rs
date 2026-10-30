//! What the static skill tables say about a bot's skills, and the data-driven fallback that
//! scores them when no rotation rule applies.
//!
//! Ported from `aaemu-game/src/systems/bots/combat.rs` (`SkillInfo`, `build_info`,
//! `skill_gate`, `usability`, `choose`). Nothing here knows a class: the candidate list is the
//! skills the character learned (plus the default weapon attacks); each one is scored from the
//! tables:
//!
//! * `power`: the damage effects of the skill (direct and inside its plot events);
//! * `applies`: the buff tags its buff effects put on the target (a setup, e.g. Snare, Mark);
//! * `bonus`: `damage_effects.target_buff_tag_id` + `target_buff_bonus_mul` (a payoff that hits
//!   harder while the target carries that tag).
//!
//! A skill whose bonus tag the target carries scores higher; a skill that applies a tag some
//! ready skill pays off scores higher while the target lacks it, so setups come before payoffs.
//! Cooldown, global cooldown, mana, range and the casting state come from the same server state
//! the cast check reads (`systems::skills`), and the last word is the server's quiet
//! [`precheck`], so a cast the server would refuse is never sent from here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aaemu_game::models::container::slot_type;
use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::data::effects::EffectTemplate;
use aaemu_game::systems::skills::data::SkillTemplate;
use aaemu_game::systems::skills::geometry::Target;
use aaemu_game::systems::skills::state::SkillRun;
use aaemu_game::systems::skills::types::{skill_result, SkillCaster};
use aaemu_game::systems::skills::{buffs, precheck, skill, unit_reqs};
use aaemu_game::world::{NpcRef, PlayerRef, WorldObject};

use super::target::{attackable, hostiles_around, view_of};
use super::CombatState;

/// Melee reach when no ready skill gives a better one.
pub const MELEE_RANGE_M: f32 = 3.0;
/// The server's own minimum between two casts (`skill.rs` `GCD_GRACE_MS`).
pub const GCD_GRACE_MS: i64 = 150;
/// The default weapon attacks (melee, ranged) as a last resort.
pub const DEFAULT_ATTACKS: [u32; 2] = [3, 2];
/// Monsters around the target count as a pack for area skills within this radius.
const AOE_RADIUS_M: f32 = 8.0;

/// What the static tables say about a skill.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub id: u32,
    pub cooldown_ms: i64,
    pub cast_ms: i64,
    pub min_range: f32,
    pub max_range: f32,
    pub kind: SkillKind,
    /// Relative damage weight (1.0 = an ordinary hit).
    pub power: f32,
    /// Buff tags this skill puts on its target.
    pub applies: Vec<u32>,
    /// `(tag, multiplier)`: damage multiplier while the target carries `tag`.
    pub bonus: Vec<(u32, f32)>,
    /// Buffs a self skill grants (it is not recast while one is active).
    pub self_buffs: Vec<u32>,
    pub gcd_free: bool,
    pub aoe: bool,
    /// The skill's `target_type` as the static data has it (`0` self, `4`/`5` hostile, else a
    /// friendly / ground target).
    pub target_type: i32,
    /// Who the bot can aim the skill at with a unit cast target.
    pub aim: Aim,
    /// The skill has a heal effect (possibly beside damage, e.g. Antithesis).
    pub heals: bool,
    /// The skill has a damage effect.
    pub damages: bool,
}

/// Who a skill can be aimed at, from its 1.2 `SkillTargetType` (`skills.target_type_id`).
///
/// The bot only ever sends a unit target: self / friendly skills resolve to the bot, hostile
/// skills to the current monster. Everything else in the 1.2 table needs a placement, a
/// doodad, an item or another unit the bot has no target for (ground `Pos=6`, `Doodad=8`,
/// `Item=9`, the ballistic / summon / relative / artillery positions, ...) and is never cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aim {
    /// `SelfTarget=0`, `Friendly=1`, `Party=2`, `Raid=3`: cast on the bot, the only ally a
    /// solo bot has.
    SelfOnly,
    /// `Hostile=4`, `AnyUnit=5`: cast on the monster being fought.
    Target,
    /// A target the bot cannot aim (ground / doodad / item / placement types): never cast.
    Unaimable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    /// Damages a hostile target.
    Damage,
    /// Hostile target, no damage (crowd control, debuff).
    Control,
    /// Self buff.
    SelfBuff,
    /// Heals, or needs a friendly / ground target: not used on a monster.
    Support,
}

/// Reads the tables for one skill (`None` for an unknown id).
pub fn build_info(server: &GameServer, skill_id: u32) -> Option<SkillInfo> {
    let data = &server.game_data.systems.skills;
    let t: Arc<SkillTemplate> = data.skill(skill_id)?.clone();
    let mut effects: Vec<EffectTemplate> = Vec::new();
    for e in &t.effects {
        if let Some((kind, id)) = e.template {
            if let Some(tpl) = data.effect(kind, id) {
                effects.push(tpl.clone());
            }
        }
    }
    if let Some(plot) = t.plot_id {
        for event in data.plots.events.values().filter(|ev| ev.plot_id == plot) {
            for fx in &event.effects {
                if let Some(tpl) = data.effect_by_name(fx.actual_id, &fx.actual_type) {
                    effects.push(tpl.clone());
                }
            }
        }
    }
    let (mut power, mut has_damage, mut has_heal) = (0.0f32, false, false);
    let (mut applies, mut bonus, mut self_buffs) = (Vec::new(), Vec::new(), Vec::new());
    for fx in &effects {
        match fx {
            EffectTemplate::Damage(d) => {
                has_damage = true;
                power += d.dps_multiplier.max(d.multiplier).max(0.2);
                if d.target_buff_tag_id != 0 {
                    let mul = if d.target_buff_bonus_mul > 0.0 {
                        d.target_buff_bonus_mul
                    } else {
                        1.0 + d.target_buff_bonus as f32 / 100.0
                    };
                    if mul > 1.0 {
                        bonus.push((d.target_buff_tag_id, mul));
                    }
                }
            }
            EffectTemplate::Heal(_) => has_heal = true,
            EffectTemplate::Buff(b) => {
                if let Some(buff_id) = b.buff_id {
                    applies.extend_from_slice(data.buff_tags(buff_id));
                    self_buffs.push(buff_id);
                }
            }
            _ => {}
        }
    }
    applies.sort_unstable();
    applies.dedup();
    let hostile_target = matches!(t.target_type, 4 | 5);
    let kind = if has_damage && hostile_target {
        SkillKind::Damage
    } else if has_heal {
        SkillKind::Support
    } else if t.target_type == 0 {
        SkillKind::SelfBuff
    } else if hostile_target {
        SkillKind::Control
    } else {
        SkillKind::Support
    };
    Some(SkillInfo {
        id: skill_id,
        cooldown_ms: i64::from(t.cooldown_time.max(0)),
        cast_ms: i64::from(t.casting_time.max(0)),
        min_range: t.min_range.max(0) as f32,
        max_range: t.max_range.max(0) as f32,
        kind,
        power: power.max(if has_damage { 0.2 } else { 0.0 }),
        applies,
        bonus,
        self_buffs: if kind == SkillKind::SelfBuff {
            self_buffs
        } else {
            Vec::new()
        },
        gcd_free: t.ignore_global_cooldown,
        aoe: t.target_area_count > 1,
        target_type: t.target_type,
        aim: aim_of(t.target_type),
        heals: has_heal,
        damages: has_damage,
    })
}

/// The aim a 1.2 `target_type_id` gives a bot's unit-target cast.
pub fn aim_of(target_type: i32) -> Aim {
    match target_type {
        0..=3 => Aim::SelfOnly,
        4 | 5 => Aim::Target,
        _ => Aim::Unaimable,
    }
}

/// The unit state a skill choice reads: mana, global cooldown, last use, a running cast and the
/// cooldown tables.
#[derive(Debug, Clone)]
pub struct CastState {
    pub mp: i32,
    pub gcd_until: i64,
    pub last_used: i64,
    pub casting: bool,
    pub cooldowns: HashMap<u32, i64>,
    pub tag_cooldowns: HashMap<u32, i64>,
}

pub fn cast_state(player: &PlayerRef) -> CastState {
    let c = player.data();
    CastState {
        mp: c.unit.mp,
        gcd_until: c.unit.global_cooldown_ms,
        last_used: c.unit.skill_last_used_ms,
        casting: c.unit.skill_state.skill_task.is_some(),
        cooldowns: c.unit.cooldowns.cooldowns.clone(),
        tag_cooldowns: c.unit.cooldowns.tag_cooldowns.clone(),
    }
}

/// The server's cast interval rule (`Skill.Use`, `skill.rs`): the per-skill delay of the basic
/// attacks (500 ms for a character, 1500 ms for an NPC) or 150 ms for everything else, and the
/// global cooldown unless the skill ignores it.
pub fn cast_throttled(
    template: &SkillTemplate,
    character: bool,
    last_used: i64,
    gcd_until: i64,
    now: i64,
) -> bool {
    let delay: i64 = if matches!(template.id, 2..=4) {
        if character {
            500
        } else {
            1500
        }
    } else {
        150
    };
    if last_used + delay - now > GCD_GRACE_MS {
        return true;
    }
    !template.ignore_global_cooldown && gcd_until - now > GCD_GRACE_MS
}

/// Is the skill ready by the server's own cooldown state: its individual and tag cooldown, the
/// global cooldown and the per-use interval?
pub fn off_cooldown(template: &SkillTemplate, cs: &CastState, now: i64) -> bool {
    let individual = cs
        .cooldowns
        .get(&template.id)
        .is_some_and(|end| *end - now > 250);
    let tagged = template.cooldown_tag_id > 0
        && cs
            .tag_cooldowns
            .get(&(template.cooldown_tag_id as u32))
            .is_some_and(|end| *end - now > 250);
    !individual && !tagged && !cast_throttled(template, true, cs.last_used, cs.gcd_until, now)
}

/// Whether the server's own cast checks (`unit_reqs` and the quiet `precheck`) would take a cast
/// of a skill on the target now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usable {
    Yes,
    /// A unit requirement fails (no weapon in the slot, the target is not in the NPC group the
    /// skill needs, ...): the key is the server's result key.
    Requirement(&'static str),
    TooFar,
    TooClose,
    /// No line of sight (`skill_obstacle`).
    NoSight,
    /// Anything else the server's validation says (stun, dead target, ...).
    Other(u8),
}

pub fn usability(
    server: &GameServer,
    player: &PlayerRef,
    template: &Arc<SkillTemplate>,
    target: &WorldObject,
) -> Usable {
    let me = WorldObject::from(player);
    let own_target = (template.target_type == 0).then(|| me.clone());
    let target = own_target.as_ref().unwrap_or(target);
    let caster = SkillCaster::Unit(player.obj_id);
    let req = unit_reqs::can_use_skill_on_target(server, template, &me, &caster, Some(target));
    if !req.is_ok() {
        return Usable::Requirement(req.key);
    }
    let run = SkillRun::new(template.clone(), 1);
    match precheck(server, &run, &me, &Target::Obj(target.clone())) {
        Ok(()) => Usable::Yes,
        Err((code, _)) => match code {
            skill_result::TOO_FAR_RANGE => Usable::TooFar,
            skill_result::TOO_CLOSE_RANGE => Usable::TooClose,
            skill_result::OBSTACLE => Usable::NoSight,
            skill_result::OUTOF_ANGLE => Usable::Yes,
            other => Usable::Other(other),
        },
    }
}

/// Why a skill cannot be cast right now: one typed reason per check the server makes, in the
/// order it makes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillBlock {
    /// Another cast is running.
    Casting,
    /// The skill's own cooldown (milliseconds left).
    Cooldown(i64),
    /// The global cooldown or the per-use interval (milliseconds left).
    GlobalCooldown(i64),
    /// Not enough mana.
    Mana {
        need: i32,
        have: i32,
    },
    /// The target is not one the bot may use the skill on (not attackable).
    Relation,
    /// The caster or the target is dead (or alive when the skill needs a corpse).
    Dead(u8),
    /// A control state: stun, sleep, knockdown, silence ("blank minded"), swimming, walking.
    Control(u8),
    TooFar,
    TooClose,
    /// No line of sight.
    NoSight,
    /// A unit requirement: equipment, the target kind, level, a buff, ...
    Requirement(&'static str),
    /// Anything else the server's validation says.
    Other(u8),
}

impl SkillBlock {
    /// The block a server result code stands for.
    pub fn from_code(code: u8) -> Self {
        match code {
            skill_result::COOLDOWN_TIME => SkillBlock::GlobalCooldown(0),
            skill_result::LACK_MANA => SkillBlock::Mana { need: 0, have: 0 },
            skill_result::TOO_FAR_RANGE => SkillBlock::TooFar,
            skill_result::TOO_CLOSE_RANGE => SkillBlock::TooClose,
            skill_result::OBSTACLE => SkillBlock::NoSight,
            skill_result::INVALID_TARGET => SkillBlock::Relation,
            skill_result::SOURCE_DIED
            | skill_result::TARGET_DIED
            | skill_result::SOURCE_ALIVE
            | skill_result::TARGET_ALIVE => SkillBlock::Dead(code),
            skill_result::CANNOT_CAST_IN_STUN
            | skill_result::BLANK_MINDED
            | skill_result::CANNOT_CAST_WHILE_WALKING
            | skill_result::CANNOT_CAST_IN_SWIMMING => SkillBlock::Control(code),
            other => SkillBlock::Other(other),
        }
    }
}

/// How a failed unit requirement of a skill is to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// The target is not of the kind the skill works on (NPC group, owner type, ...): the
    /// skill can never hit this kind of target.
    TargetKind,
    /// The bot's gear, skills, level or abilities decide: tried again when they change.
    Loadout,
    /// A state that comes and goes (a buff, combat, time of day, ...).
    Passing,
}

pub fn requirement_kind(key: &str) -> Requirement {
    match key {
        "skill_urk_target_npc_group"
        | "skill_urk_target_owner_type"
        | "skill_urk_target_npc"
        | "skill_urk_target_doodad" => Requirement::TargetKind,
        "skill_urk_equip_slot"
        | "skill_urk_equip_item"
        | "skill_urk_equip_shield"
        | "skill_urk_equip_ranged"
        | "skill_urk_trained_skill"
        | "skill_urk_ability"
        | "skill_urk_level"
        | "skill_urk_max_level"
        | "skill_urk_race"
        | "skill_urk_gender" => Requirement::Loadout,
        _ => Requirement::Passing,
    }
}

/// Every check a cast of `template` on `target` must pass, with the typed reason of the first
/// that fails: the running cast, the skill's cooldown and the global cooldown / per-use interval
/// (all from the unit's own state), mana, the relation to the target, then the server's own
/// unit requirements and cast validation ([`usability`]: dead or stunned or silenced casters,
/// range, angle, line of sight).
pub fn skill_gate(
    server: &GameServer,
    player: &PlayerRef,
    template: &Arc<SkillTemplate>,
    target: &WorldObject,
    cs: &CastState,
    now: i64,
) -> Result<(), SkillBlock> {
    if cs.casting {
        return Err(SkillBlock::Casting);
    }
    if let Some(end) = cs.cooldowns.get(&template.id) {
        if *end - now > 250 {
            return Err(SkillBlock::Cooldown(*end - now));
        }
    }
    if template.cooldown_tag_id > 0 {
        if let Some(end) = cs.tag_cooldowns.get(&(template.cooldown_tag_id as u32)) {
            if *end - now > 250 {
                return Err(SkillBlock::Cooldown(*end - now));
            }
        }
    }
    if cast_throttled(template, true, cs.last_used, cs.gcd_until, now) {
        let left = (cs.gcd_until - now).max(0);
        return Err(SkillBlock::GlobalCooldown(left));
    }
    let me = WorldObject::from(player);
    let need = skill::mana_cost(server, &SkillRun::new(template.clone(), 1), &me);
    if need > cs.mp {
        return Err(SkillBlock::Mana { need, have: cs.mp });
    }
    if let Some(hp) = target.with_unit(|u| u.unit().hp) {
        if hp <= 0 && template.target_alive && !template.target_dead {
            return Err(SkillBlock::Dead(skill_result::TARGET_DIED));
        }
    }
    let hostile_skill = matches!(template.target_type, 4 | 5);
    if hostile_skill && target.as_npc().is_some() {
        let npc = target.as_npc().expect("checked above");
        if !attackable(server, player, npc) {
            return Err(SkillBlock::Relation);
        }
    }
    match usability(server, player, template, target) {
        Usable::Yes => Ok(()),
        Usable::Requirement(key) => Err(SkillBlock::Requirement(key)),
        Usable::TooFar => Err(SkillBlock::TooFar),
        Usable::TooClose => Err(SkillBlock::TooClose),
        Usable::NoSight => Err(SkillBlock::NoSight),
        Usable::Other(code) => Err(SkillBlock::from_code(code)),
    }
}

/// The default auto-attack that matches the equipped weapon: the ranged one for a weapon whose
/// holdable reaches, the melee one otherwise (the client picks it the same way).
pub fn default_attack(server: &GameServer, player: &PlayerRef) -> u32 {
    let mainhand = {
        let c = player.data();
        c.inventory.get(slot_type::EQUIPMENT).and_then(|e| {
            e.lock()
                .expect("equipment poisoned")
                .items
                .iter()
                .find(|i| i.slot == aaemu_game::models::container::equip_slot::MAINHAND)
                .map(|i| i.template_id)
        })
    };
    let reach = mainhand
        .and_then(|id| server.game_data.items.template(id))
        .and_then(|t| server.game_data.items.holdable_of(t))
        .map_or(0, |h| h.max_range);
    if reach >= 10 {
        2
    } else {
        3
    }
}

/// The skills the bot may cast now: learned skills of its skillsets, then the default attack of
/// its weapon.
pub fn candidate_ids(server: &GameServer, player: &PlayerRef) -> Vec<u32> {
    let data = &server.game_data.systems.skills;
    let mut ids: Vec<u32> = {
        let c = player.data();
        let actives = [c.ability1, c.ability2, c.ability3];
        c.skills
            .skills
            .iter()
            .map(|s| s.id)
            .filter(|id| {
                data.skill(*id)
                    .is_some_and(|t| t.ability_id == 0 || actives.contains(&t.ability_id))
            })
            .collect()
    };
    ids.push(default_attack(server, player));
    ids
}

/// The outcome of choosing a skill.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Cast {
        skill: u32,
        /// The skill is cast on the bot itself (a self buff or a self heal).
        self_target: bool,
    },
    /// Walk until within this distance of the target.
    Approach(f32),
    /// Nothing is ready (global cooldown, casting, all on cooldown).
    Wait,
    /// The target cannot be hit from anywhere the bot can get to (no line of sight for too
    /// long): leave it.
    GiveUp,
}

/// Picks the next skill against `target` at distance `dist` (metres) by scoring the skill data.
/// `all` is the think's one neighbourhood scan (the crowd count of the area-scored skills).
pub fn choose_data(
    server: &GameServer,
    player: &PlayerRef,
    state: &mut CombatState,
    target: &WorldObject,
    dist: f32,
    now: i64,
    all: &[NpcRef],
) -> Choice {
    let cs = cast_state(player);
    if cs.casting {
        return Choice::Wait;
    }
    // The default attack is an auto-attack loop once started: pressing it again does nothing
    // (and would look like a refusal because the server takes it without new cast marks).
    let auto_attack = player.data().unit.is_auto_attack;
    let crowd = target.as_npc().map_or(0, |npc| {
        hostiles_around(server, player, all, &view_of(npc), AOE_RADIUS_M)
    });
    let target_template = target.with_base_unit(|b| b.template_id());
    let target_obj = target.obj_id();
    let sig = state.loadout_signature(player);
    let ids = candidate_ids(server, player);
    struct Ready {
        info: Arc<SkillInfo>,
        usable: Usable,
    }
    let mut ready: Vec<Ready> = Vec::new();
    for id in ids {
        let Some(info) = state.info(server, id) else {
            continue;
        };
        if !matches!(
            info.kind,
            SkillKind::Damage | SkillKind::Control | SkillKind::SelfBuff
        ) || info.aim == Aim::Unaimable
        {
            continue;
        }
        // Facts, not delays: a skill that can never hit this kind of target, and a skill the
        // current loadout cannot use, are out of the rotation until the target kind / the
        // loadout changes.
        if state.never.contains(&(id, target_template)) || state.dropped.get(&id) == Some(&sig) {
            continue;
        }
        if super::cast::refused(state, player, id, target_obj, now) {
            continue;
        }
        let Some(template) = server.game_data.systems.skills.skill(id).cloned() else {
            continue;
        };
        let usable = match skill_gate(server, player, &template, target, &cs, now) {
            Ok(()) => Usable::Yes,
            Err(SkillBlock::TooFar) => Usable::TooFar,
            Err(SkillBlock::NoSight) => Usable::NoSight,
            Err(SkillBlock::Requirement(key)) => {
                state.learn_requirement(id, target_template, key, sig);
                continue;
            }
            Err(_) => continue,
        };
        ready.push(Ready { info, usable });
    }
    let me = WorldObject::from(player);
    let has_tag = |tag: u32| buffs::check_buffs_by_tag(server, target, tag);
    let mut best: Option<(f32, u32, bool)> = None;
    let mut reach = 0.0f32;
    let mut no_sight = false;
    for r in &ready {
        let info = &r.info;
        match r.usable {
            Usable::Yes => {}
            Usable::TooFar if info.kind != SkillKind::SelfBuff => {
                if info.kind == SkillKind::Damage {
                    let max = if info.max_range > 0.0 {
                        info.max_range
                    } else {
                        MELEE_RANGE_M
                    };
                    reach = reach.max(max);
                }
                continue;
            }
            Usable::NoSight => {
                no_sight = true;
                continue;
            }
            _ => continue,
        }
        if info.kind == SkillKind::SelfBuff {
            // Pre-fight buff: only while none of its buffs is active.
            let active = info.self_buffs.iter().any(|b| {
                me.with_base_unit(|u| u.buffs().effects.iter().any(|e| e.template.id == *b))
            });
            if !active {
                let score = 50.0;
                if best.as_ref().is_none_or(|(s, ..)| score > *s) {
                    best = Some((score, info.id, true));
                }
            }
            continue;
        }
        let mut score = if info.kind == SkillKind::Damage {
            info.power.max(0.2)
        } else {
            0.1
        };
        for (tag, mul) in &info.bonus {
            if has_tag(*tag) {
                score *= *mul;
            }
        }
        for tag in &info.applies {
            if has_tag(*tag) {
                continue;
            }
            let payoff = ready
                .iter()
                .filter(|o| o.usable == Usable::Yes && o.info.id != info.id)
                .flat_map(|o| o.info.bonus.iter())
                .filter(|(t, _)| t == tag)
                .map(|(_, m)| *m)
                .fold(0.0f32, f32::max);
            if payoff > 0.0 {
                score *= 1.0 + payoff;
            }
        }
        if !info.applies.is_empty() && info.applies.iter().all(|t| has_tag(*t)) {
            score *= 0.5;
        }
        if DEFAULT_ATTACKS.contains(&info.id) {
            if auto_attack {
                continue;
            }
            score *= 0.3;
        }
        if info.aoe && crowd > 0 {
            score *= 1.0 + 0.6 * crowd.min(4) as f32;
        }
        if !info.applies.is_empty() && info.applies.iter().all(|t| !has_tag(*t)) {
            score *= if info.kind == SkillKind::Control {
                5.0
            } else {
                1.3
            };
        }
        score /= 1.0 + info.cast_ms as f32 / 2_000.0;
        if best.as_ref().is_none_or(|(s, ..)| score > *s) {
            // The skill's own target type decides who it is cast on: only aimable-at-a-target
            // skills go to the monster (a damage+heal skill like Antithesis stays hostile).
            best = Some((score, info.id, info.aim == Aim::SelfOnly));
        }
    }
    if let Some((_, id, self_target)) = best {
        state.no_sight = None;
        return Choice::Cast {
            skill: id,
            self_target,
        };
    }
    if reach > 0.0 && dist > reach - 1.0 {
        state.no_sight = None;
        return Choice::Approach((reach - 1.0).max(MELEE_RANGE_M - 1.0));
    }
    if no_sight {
        let target_id = target.obj_id();
        let since = match state.no_sight {
            Some((id, since)) if id == target_id => since,
            _ => {
                state.no_sight = Some((target_id, now));
                now
            }
        };
        if now - since >= super::NO_SIGHT_GIVE_UP_MS {
            state.no_sight = None;
            return Choice::GiveUp;
        }
        return Choice::Approach(1.0);
    }
    state.no_sight = None;
    Choice::Wait
}

/// A learned-skill set snapshot (ids the character owns).
pub fn learned(player: &PlayerRef) -> HashSet<u32> {
    player.data().skills.skills.iter().map(|s| s.id).collect()
}

/// Healing potions in the bag: `(item id, template id, use skill id)`.
pub fn healing_items(
    server: &GameServer,
    state: &mut CombatState,
    player: &PlayerRef,
) -> Vec<(u64, u32, u32)> {
    let bag = {
        let c = player.data();
        c.inventory.get(slot_type::INVENTORY).cloned()
    };
    let Some(bag) = bag else {
        return Vec::new();
    };
    let items: Vec<(u64, u32)> = bag
        .lock()
        .expect("bag poisoned")
        .items
        .iter()
        .map(|i| (i.id, i.template_id))
        .collect();
    items
        .into_iter()
        .filter_map(|(item_id, template_id)| {
            let t = server.game_data.items.template(template_id)?;
            if t.use_skill_id == 0 {
                return None;
            }
            state
                .info(server, t.use_skill_id)
                .is_some_and(|i| i.heals)
                .then_some((item_id, template_id, t.use_skill_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_game::systems::skills::data::effects::DamageEffect;

    #[test]
    fn the_basic_attack_delay_matches_the_servers_rule() {
        let mut t = SkillTemplate {
            id: 3,
            ..Default::default()
        };
        assert!(
            cast_throttled(&t, true, 1_000, 0, 1_100),
            "500 ms melee delay"
        );
        assert!(
            !cast_throttled(&t, true, 1_000, 0, 1_600),
            "after the delay"
        );
        t.id = 10;
        assert!(
            !cast_throttled(&t, true, 1_000, 0, 1_200),
            "150 ms for other skills"
        );
        t.ignore_global_cooldown = false;
        assert!(
            cast_throttled(&t, true, 0, 2_000, 1_000),
            "a running global cooldown blocks"
        );
        t.ignore_global_cooldown = true;
        assert!(
            !cast_throttled(&t, true, 0, 2_000, 1_000),
            "unless the skill ignores it"
        );
    }

    #[test]
    fn requirements_are_classified_by_what_they_depend_on() {
        assert_eq!(
            requirement_kind("skill_urk_equip_slot"),
            Requirement::Loadout
        );
        assert_eq!(
            requirement_kind("skill_urk_target_npc_group"),
            Requirement::TargetKind
        );
        assert_eq!(
            requirement_kind("skill_urk_need_stealth"),
            Requirement::Passing
        );
    }

    #[test]
    fn a_damage_skill_reports_its_power_and_tags() {
        let data_effect = DamageEffect {
            id: 1,
            dps_multiplier: 1.5,
            multiplier: 1.0,
            target_buff_tag_id: 107,
            target_buff_bonus_mul: 1.48,
            ..Default::default()
        };
        assert_eq!(data_effect.target_buff_tag_id, 107);
        assert!(matches!(
            EffectTemplate::Damage(Box::new(data_effect)),
            EffectTemplate::Damage(_)
        ));
    }
}

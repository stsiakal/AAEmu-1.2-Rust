//! Bot combat: target selection, the data-driven / rotation skill choice, casting through the
//! real `CS*` packets, auto-attack, looting, resting and survival.
//!
//! This is the P2 port of `aaemu-game/src/systems/bots/combat.rs` and the survival half of its
//! `mod.rs`/`session.rs`, adapted to the current tree where a bot is a normal player on a
//! [`SocketlessConnection`](aaemu_game::net::socketless::SocketlessConnection): every action is
//! the packet a client sends (`CSChangeTargetPacket`, `CSStartSkillPacket`,
//! `CSLootOpenBagPacket`, `CSMoveUnitPacket`, `CSResurrectCharacterPacket`), and every rule the
//! server applies runs unchanged. No GM action is used in combat.
//!
//! The think loop (one per bot, driven by the host's scheduler) is:
//! 1. read the frames the server wrote (skill started / fired / ended, counters for the report);
//! 2. a dead bot waits its `RezWaitDuration` and asks for a player-legal resurrection;
//! 3. the learned skills grow along the archetype's learn order (packets);
//! 4. rest, heal / potion, flee from what it cannot fight;
//! 5. loot the corpse of the last kill;
//! 6. acquire a target (attacker first, then a sane pull), turn, close in with the nav walker,
//!    and cast the rotation's / data scoring's choice; the target's death starts over.

pub mod cast;
pub mod loot;
pub mod progress;
pub mod rotation;
pub mod skills;
pub mod survival;
pub mod target;

#[cfg(test)]
mod data_tests;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::systems::skills::handlers::CSChangeTargetPacket;
use aaemu_game::systems::skills::types::SkillCaster;
use aaemu_game::world::{PlayerRef, WorldObject};

use crate::host::BotSession;
use crate::nav::danger::DangerMark;
use crate::rotation::{Archetype, Rotation};

use self::cast::{CastOutcome, RefusalKey};
use self::skills::{cast_state, Choice, SkillInfo, MELEE_RANGE_M};
use self::target::{dist2, me_pos, NpcView, PullRules};

pub use self::skills::DEFAULT_ATTACKS;
pub use self::survival::{AttackerSeen, HEAL_BELOW, REST_BELOW, REST_UNTIL};

/// Monsters that could not be reached are left alone this long.
pub const IGNORE_MS: i64 = 120_000;
/// Without line of sight to a target for this long the bot gives it up.
pub const NO_SIGHT_GIVE_UP_MS: i64 = 8_000;
/// The server's own minimum between two casts (a trace detail, the gate reads its own copy).
pub const CAST_SPACING_MS: i64 = 200;
/// A death to a monster this many levels above the bot is a level problem, not bad luck.
pub const LEVEL_GAP_CAUSE: u8 = 3;
/// Loot is taken from this close (the server's take range is 5 m, keep a metre of slack).
pub const LOOT_DISTANCE_M: f32 = 4.0;
/// A grind with no monster in range walks to a new spot within its radius this often.
const WANDER_MS: i64 = 15_000;
/// How far from the grind centre a bot may wander (half its radius).
const WANDER_FRACTION: f32 = 0.5;

/// What the bot is doing between fights.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Added but idle (`/bot add`; P3's life state machine replaces this).
    Idle,
    /// Fight monsters of these templates (empty = any) within `radius` (`/bot fight`).
    Fight { wanted: Vec<u32>, radius: f32 },
    /// Kill and loot around `center` (`/bot grind`).
    Grind { center: (f32, f32), radius: f32 },
}

/// Per-bot combat state (owned by [`BotSession`](crate::host::BotSession)'s brain lock).
pub struct CombatState {
    pub mode: Mode,
    /// The monster currently fought (object id).
    pub target: Option<u32>,
    /// What the bot last told the server with `CSChangeTargetPacket`.
    target_sent: u32,
    infos: HashMap<u32, Option<Arc<SkillInfo>>>,
    pub resting: bool,
    /// Player-visible counters (the live report and `/bot list`).
    pub kills: u32,
    pub deaths: u32,
    pub casts_started: u32,
    pub casts_refused: u32,
    pub looted_items: u32,
    pub last_cast_skill: u32,
    last_cast_ms: i64,
    /// Where the running approach walk goes (`None` = no walk).
    walk_to: Option<(f32, f32)>,
    /// Monsters that could not be reached, ignored until the given time.
    pub ignored: HashMap<u32, i64>,
    /// The last monster seen attacking the bot (the likely killer when it dies).
    pub last_attacker: Option<AttackerSeen>,
    /// The last place the bot stood calm and healthy.
    pub safe_pos: Option<(f32, f32)>,
    pub safe_pos_ms: i64,
    /// Where the running flee walk goes, and how many flee walks failed in a row.
    fleeing_to: Option<(f32, f32)>,
    flee_fails: u8,
    /// Since when the bot has had no line of sight to its target.
    pub no_sight: Option<(u32, i64)>,
    /// `(skill, target template)`: the skill can never hit that kind of target.
    pub never: HashSet<(u32, u32)>,
    /// Skills the current loadout cannot use: skill -> the loadout signature.
    pub dropped: HashMap<u32, u64>,
    /// Skills the server refused although every check said yes.
    pub refused_on: HashMap<u32, RefusalKey>,
    /// Health / mana fraction wanted before a fight starts.
    pub engage_min: f32,
    /// Health fraction below which heals / potions are used.
    pub heal_at: f32,
    /// Health / mana fraction below which a calm bot rests.
    pub rest_below: f32,
    /// `Bots.MaxTargetLevelGap` at this bot's last configure.
    pub max_target_level_gap: u8,
    /// The archetype / rotation this bot grows and fights with.
    pub archetype: Option<Arc<Archetype>>,
    pub rotation: Option<Arc<Rotation>>,
    pub progress: progress::ProgressState,
    /// Areas of monsters too strong for the bot (the danger index of the nav tests, P4's
    /// authored index fills the other half).
    pub danger_marks: Vec<DangerMark>,
    /// The corpse the last kill left, to be looted.
    pub loot_target: Option<u32>,
    /// Grind wandering: a rotating direction counter and when the next wander may start.
    wander: u32,
    next_wander_ms: i64,
    /// Death bookkeeping.
    pub dead_since: Option<i64>,
    respawn_sent_ms: i64,
    /// The bot's base think cadence out of combat (`Bots.BrainTickMs`).
    pub base_tick: Duration,
    /// The skill frames the server wrote since the last think (report counters).
    pub skill_started: u32,
    pub skill_fired: u32,
    pub skill_ended: u32,
}

impl Default for CombatState {
    fn default() -> Self {
        Self {
            mode: Mode::Idle,
            target: None,
            target_sent: 0,
            infos: HashMap::new(),
            resting: false,
            kills: 0,
            deaths: 0,
            casts_started: 0,
            casts_refused: 0,
            looted_items: 0,
            last_cast_skill: 0,
            last_cast_ms: 0,
            walk_to: None,
            ignored: HashMap::new(),
            last_attacker: None,
            safe_pos: None,
            safe_pos_ms: 0,
            fleeing_to: None,
            flee_fails: 0,
            no_sight: None,
            never: HashSet::new(),
            dropped: HashMap::new(),
            refused_on: HashMap::new(),
            engage_min: survival::ENGAGE_MIN,
            heal_at: HEAL_BELOW,
            rest_below: REST_BELOW,
            max_target_level_gap: 3,
            archetype: None,
            rotation: None,
            progress: Default::default(),
            danger_marks: Vec::new(),
            loot_target: None,
            wander: 0,
            next_wander_ms: 0,
            dead_since: None,
            respawn_sent_ms: 0,
            base_tick: Duration::from_millis(250),
            skill_started: 0,
            skill_fired: 0,
            skill_ended: 0,
        }
    }
}

impl CombatState {
    pub fn info(&mut self, server: &GameServer, skill_id: u32) -> Option<Arc<SkillInfo>> {
        self.infos
            .entry(skill_id)
            .or_insert_with(|| skills::build_info(server, skill_id).map(Arc::new))
            .clone()
    }

    /// Forgets the fight (death, target change).
    pub fn disengage(&mut self) {
        self.target = None;
        self.walk_to = None;
    }

    /// The health / mana fraction a resting bot rests up to.
    pub fn rest_until(&self) -> f32 {
        self.engage_min.max(REST_UNTIL)
    }

    /// A signature of everything a skill's usability depends on besides the target: worn
    /// equipment, learned skills, active skillsets and level.
    pub fn loadout_signature(&self, player: &PlayerRef) -> u64 {
        use std::hash::{Hash, Hasher};
        let c = player.data();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        c.unit.level.hash(&mut h);
        (c.ability1, c.ability2, c.ability3).hash(&mut h);
        let mut skills: Vec<u32> = c.skills.skills.iter().map(|s| s.id).collect();
        skills.sort_unstable();
        skills.hash(&mut h);
        if let Some(equipment) = c
            .inventory
            .get(aaemu_game::models::container::slot_type::EQUIPMENT)
        {
            let mut worn: Vec<(i32, u32)> = equipment
                .lock()
                .expect("container poisoned")
                .items
                .iter()
                .map(|i| (i.slot, i.template_id))
                .collect();
            worn.sort_unstable();
            worn.hash(&mut h);
        }
        h.finish()
    }

    /// What the server's own checks say about a skill failing a unit requirement: a fact about
    /// the kind of target, a fact about the loadout, or a passing state (asked again each time).
    pub fn learn_requirement(
        &mut self,
        skill_id: u32,
        target_template: u32,
        key: &'static str,
        loadout: u64,
    ) {
        match skills::requirement_kind(key) {
            skills::Requirement::TargetKind => {
                self.never.insert((skill_id, target_template));
            }
            skills::Requirement::Loadout => {
                self.dropped.insert(skill_id, loadout);
            }
            skills::Requirement::Passing => {}
        }
    }

    /// A live mark of a monster above the level gap.
    fn note_danger(&mut self, x: f32, y: f32, level: u8) {
        let mark = DangerMark {
            x,
            y,
            radius: crate::nav::danger::MARK_RADIUS_M,
            level,
        };
        if let Some(existing) = self.danger_marks.iter_mut().find(|m| m.contains(x, y)) {
            existing.level = existing.level.max(mark.level);
        } else {
            self.danger_marks.push(mark);
            if self.danger_marks.len() > 32 {
                self.danger_marks.remove(0);
            }
        }
    }

    /// Is `(x, y)` inside a danger mark the bot should avoid at its level?
    fn is_dangerous(&self, x: f32, y: f32, level: u8) -> bool {
        self.danger_marks
            .iter()
            .any(|m| m.is_live(level, self.max_target_level_gap) && m.contains(x, y))
    }
}

/// Configures a freshly adopted bot: its class data, the level gap and the think cadence.
pub fn configure(bot: &mut CombatState, archetype: Option<Arc<Archetype>>, rotation: Option<Arc<Rotation>>, max_gap: u8, tick_ms: u64) {
    bot.archetype = archetype;
    bot.rotation = rotation;
    bot.max_target_level_gap = max_gap;
    bot.base_tick = Duration::from_millis(tick_ms.clamp(20, 5_000));
}

/// One think of a live bot. Returns how long until the next think.
pub async fn think(bot: &Arc<BotSession>, server: &Arc<GameServer>) -> Duration {
    let mut state = bot.brain().lock().await;
    let now = aaemu_game::world::now_ms();

    // The frames of the server's answers since the last think (a client's socket read).
    let frames = bot.drain_replies().await;
    let replies = cast::CastReplies::of(&frames);
    state.skill_started += replies.started as u32;
    state.skill_fired += replies.fired as u32;
    state.skill_ended += replies.ended as u32;

    let Some(player) = bot.player().await else {
        return Duration::from_secs(1);
    };
    if !player.data().is_online {
        return Duration::from_secs(1);
    }

    // Dead: wait the client's own resurrection delay, then ask like a client.
    if player.data().unit.hp <= 0 {
        return handle_dead(bot, server, &mut state, &player, now).await;
    }
    state.dead_since = None;

    // Grow along the archetype's learn order.
    if let Some(archetype) = state.archetype.clone() {
        progress::maintain(server, bot, &archetype, &mut state.progress, now).await;
    }

    survival::note_safe_position(&player, &mut state, now);
    let resting = survival::update_rest(&player, &mut state);
    if matches!(state.mode, Mode::Idle) {
        return state.base_tick;
    }

    // What cannot be fought: run away (the danger index keeps the area).
    if let Some(threat) = survival::threat_to_flee(server, &player, &mut state, now) {
        flee(bot, server, &mut state, &player, &threat, now);
        return Duration::from_millis(300);
    }
    state.fleeing_to = None;
    state.flee_fails = 0;

    // Loot the last kill before pulling the next monster.
    if state.loot_target.is_some() && loot_step(bot, server, &mut state, &player).await {
        return Duration::from_millis(300);
    }

    // Heal / potion before the next action.
    if heal_if_hurt(bot, server, &mut state, &player, now).await {
        return Duration::from_millis(300);
    }
    if resting {
        return Duration::from_secs(2);
    }

    // The current target: keep it while alive; a kill is counted and its corpse looted.
    let mode = state.mode.clone();
    if let Some(obj_id) = state.target {
        match npc_hp(server, obj_id) {
            Some(hp) if hp > 0 => {}
            _ => {
                state.kills += 1;
                state.disengage();
                if loot::loot_count(server, obj_id).is_some_and(|n| n > 0) {
                    state.loot_target = Some(obj_id);
                }
                tracing::info!(
                    character = %bot.name,
                    target = obj_id,
                    kills = state.kills,
                    "Bots: target died"
                );
            }
        }
    }

    if state.target.is_none() {
        if let Some(view) = acquire_target(server, &player, &state, &mode, now) {
            state.target = Some(view.obj_id);
            state.walk_to = None;
            tracing::debug!(
                character = %bot.name,
                target = view.obj_id,
                template = view.template_id,
                level = view.level,
                "Bots: engaging"
            );
        }
    }

    let Some(obj_id) = state.target else {
        return no_target(bot, server, &mut state, &player, &mode, now).await;
    };
    let Some(target_obj) = server.world.object(obj_id) else {
        state.disengage();
        return Duration::from_millis(300);
    };
    if target_sent_changed(bot, server, &mut state, obj_id).await {
        // The target packet is one client action; the cast follows on the next think.
        return Duration::from_millis(100);
    }
    let (px, py, _pz) = me_pos(&player);
    let t = target_obj.transform().local();
    let dist = dist2(px, py, t.x, t.y);

    // The rotation first, the data scoring as the fallback.
    let chosen = rotation::choose_rotation(server, &player, &mut state, &target_obj, dist, now)
        .map(|(skill, self_target)| Choice::Cast { skill, self_target })
        .unwrap_or_else(|| skills::choose_data(server, &player, &mut state, &target_obj, dist, now));

    match chosen {
        Choice::Cast {
            skill,
            self_target,
        } => {
            if bot.walking() {
                // Let the walker finish its arrival radius before the cast changes the facing.
                bot.bump_nav_generation();
                return Duration::from_millis(300);
            }
            let template = target_obj.with_base_unit(|b| b.template_id());
            let target_id = if self_target { player.obj_id } else { obj_id };
            let outcome = cast::cast(
                bot,
                server,
                &mut state,
                skill,
                SkillCaster::Unit(player.obj_id),
                target_id,
                template,
                now,
            )
            .await;
            match outcome {
                CastOutcome::Started => Duration::from_millis(300),
                CastOutcome::Refused => Duration::from_millis(900),
            }
        }
        Choice::Approach(range) => {
            if approach(bot, server, &mut state, &player, obj_id, t.x, t.y, range) {
                Duration::from_millis(300)
            } else {
                state.ignored.insert(obj_id, now + IGNORE_MS);
                state.disengage();
                Duration::from_millis(300)
            }
        }
        Choice::Wait => {
            if dist > MELEE_RANGE_M + 2.0 {
                approach(
                    bot,
                    server,
                    &mut state,
                    &player,
                    obj_id,
                    t.x,
                    t.y,
                    MELEE_RANGE_M - 1.0,
                );
            }
            Duration::from_millis(300)
        }
        Choice::GiveUp => {
            tracing::info!(
                character = %bot.name,
                target = obj_id,
                "Bots: giving up a target without line of sight"
            );
            state.ignored.insert(obj_id, now + IGNORE_MS);
            state.disengage();
            Duration::from_millis(300)
        }
    }
}

/// The HP of a world object's unit (`None` when the object is gone).
fn npc_hp(server: &GameServer, obj_id: u32) -> Option<i32> {
    server
        .world
        .object(obj_id)
        .and_then(|o| o.with_unit(|u| u.unit().hp))
}

/// Tells the server about a new target once; `true` when the packet was sent.
async fn target_sent_changed(
    bot: &Arc<BotSession>,
    server: &GameServer,
    state: &mut CombatState,
    obj_id: u32,
) -> bool {
    if state.target_sent == obj_id {
        return false;
    }
    state.target_sent = obj_id;
    let _ = bot
        .dispatch(server, CSChangeTargetPacket { target_id: obj_id })
        .await;
    true
}

/// Recruits the next target: an attacker first (when it is within the gap), then the best pull.
fn acquire_target(
    server: &GameServer,
    player: &PlayerRef,
    state: &CombatState,
    mode: &Mode,
    now: i64,
) -> Option<NpcView> {
    let (wanted, radius, allow_tagged) = match mode {
        Mode::Idle => return None,
        Mode::Fight { wanted, radius } => (wanted.as_slice(), *radius, true),
        Mode::Grind { radius, center } => {
            let (px, py, _) = me_pos(player);
            if dist2(px, py, center.0, center.1) > *radius {
                // The bot was pulled out of its grind (or respawned): walk back first.
                return None;
            }
            (&[][..], *radius, false)
        }
    };
    let my_level = player.data().unit.level;
    if let Some(attacker) = target::find_attacker(server, player) {
        if attacker.level <= my_level.saturating_add(state.max_target_level_gap) {
            return Some(attacker);
        }
    }
    let rules = PullRules {
        ignored: Some(&state.ignored),
        caution: 0,
        deferred: None,
        allow_tagged,
    };
    target::find_target(
        server,
        player,
        wanted,
        &rules,
        radius,
        state.max_target_level_gap,
        now,
    )
}

/// Walks toward `(x, y)` until `range`; `false` when the walk could not even be started.
#[allow(clippy::too_many_arguments)]
fn approach(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
    obj_id: u32,
    x: f32,
    y: f32,
    range: f32,
) -> bool {
    let (px, py, pz) = me_pos(player);
    if dist2(px, py, x, y) <= range.max(1.0) && !bot.walking() {
        return true;
    }
    if bot.walking() {
        if let Some((wx, wy)) = state.walk_to {
            if dist2(wx, wy, x, y) <= 3.0 {
                return true;
            }
        }
    }
    let z = aaemu_game::systems::ai::nav::NavView::of_object(server, &WorldObject::from(player))
        .and_then(|v| v.terrain_height(x, y).or_else(|| v.height(x, y)))
        .unwrap_or(pz);
    state.walk_to = Some((x, y));
    let _ = bot.start_goto(server, Vec3::new(x, y, z), range.max(1.0));
    tracing::trace!(
        character = %bot.name,
        target = obj_id,
        x,
        y,
        range,
        "Bots: approaching"
    );
    true
}

/// Runs from `attacker` and records the danger mark (the nav danger index's mark).
fn flee(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
    attacker: &NpcView,
    now: i64,
) {
    let (px, py, pz) = me_pos(player);
    if state.fleeing_to.is_none() {
        state.disengage();
        state.note_danger(attacker.x, attacker.y, attacker.level);
        tracing::info!(
            character = %bot.name,
            attacker = attacker.obj_id,
            level = attacker.level,
            "Bots: fleeing a monster above the level gap"
        );
    }
    if bot.walking() {
        return;
    }
    let mut target = survival::flee_target(
        (px, py),
        (attacker.x, attacker.y),
        state.safe_pos,
        state.flee_fails,
    );
    let level = player.data().unit.level;
    for _ in 0..6 {
        if !state.is_dangerous(target.0, target.1, level) {
            break;
        }
        target = survival::flee_target(
            (px, py),
            (attacker.x, attacker.y),
            state.safe_pos,
            state.flee_fails.saturating_add(1),
        );
    }
    let z = aaemu_game::systems::ai::nav::NavView::of_object(server, &WorldObject::from(player))
        .and_then(|v| v.terrain_height(target.0, target.1).or_else(|| v.height(target.0, target.1)))
        .unwrap_or(pz);
    state.fleeing_to = Some(target);
    state.flee_fails = state.flee_fails.saturating_add(1);
    let _ = now;
    let _ = bot.start_goto(server, Vec3::new(target.0, target.1, z), 2.0);
}

/// Loots the pending corpse: open it in range, else walk to it. `true` while busy.
async fn loot_step(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
) -> bool {
    let Some(corpse) = state.loot_target else {
        return false;
    };
    let items = loot::loot_count(server, corpse).unwrap_or(0);
    if items == 0 || loot::corpse_position(server, corpse).is_none() {
        state.loot_target = None;
        return false;
    }
    if loot::in_loot_range(server, {
        let (x, y, _) = me_pos(player);
        (x, y)
    }, corpse) {
        loot::open_corpse(bot, server, state, corpse).await;
        state.loot_target = None;
        return false;
    }
    let (x, y) = loot::corpse_position(server, corpse).expect("checked above");
    if !approach(bot, server, state, player, corpse, x, y, LOOT_DISTANCE_M) && !bot.walking() {
        state.loot_target = None;
        return false;
    }
    true
}

/// Resting with nothing to do: a grind walks to a new spot inside its radius now and then.
async fn no_target(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
    mode: &Mode,
    now: i64,
) -> Duration {
    if state.resting {
        return Duration::from_secs(2);
    }
    let Mode::Grind { center, radius } = mode else {
        return state.base_tick;
    };
    let (px, py, pz) = me_pos(player);
    if dist2(px, py, center.0, center.1) > *radius {
        if bot.walking() && state.walk_to.is_some() {
            return Duration::from_millis(300);
        }
        let z = aaemu_game::systems::ai::nav::NavView::of_object(server, &WorldObject::from(player))
            .and_then(|v| v.terrain_height(center.0, center.1).or_else(|| v.height(center.0, center.1)))
            .unwrap_or(pz);
        state.walk_to = Some(*center);
        let _ = bot.start_goto(server, Vec3::new(center.0, center.1, z), 5.0);
        return Duration::from_millis(300);
    }
    if bot.walking() || now < state.next_wander_ms {
        return Duration::from_millis(300);
    }
    state.wander = state.wander.wrapping_add(1);
    state.next_wander_ms = now + WANDER_MS;
    let angle = (state.wander as f32 * 2.399_963_2) % (std::f32::consts::PI * 2.0);
    let reach = *radius * WANDER_FRACTION;
    let x = (center.0 + reach * angle.cos()).clamp(
        center.0 - *radius,
        center.0 + *radius,
    );
    let y = (center.1 + reach * angle.sin()).clamp(
        center.1 - *radius,
        center.1 + *radius,
    );
    let z = aaemu_game::systems::ai::nav::NavView::of_object(server, &WorldObject::from(player))
        .and_then(|v| v.terrain_height(x, y).or_else(|| v.height(x, y)))
        .unwrap_or(pz);
    state.walk_to = Some((x, y));
    let _ = bot.start_goto(server, Vec3::new(x, y, z), 3.0);
    tracing::trace!(character = %bot.name, x, y, "Bots: grinding to a new spot");
    Duration::from_millis(300)
}

/// The death policy: wait `RezWaitDuration`, then send `CSResurrectCharacterPacket` with the
/// player-legal choice (in place only when safe, else the temple / return point).
async fn handle_dead(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
    now: i64,
) -> Duration {
    if state.dead_since.is_none() {
        state.dead_since = Some(now);
        state.deaths += 1;
        state.disengage();
        state.loot_target = None;
        state.resting = false;
        tracing::info!(
            character = %bot.name,
            deaths = state.deaths,
            "Bots: died"
        );
    }
    if !survival::may_resurrect(&player.data(), now) {
        return Duration::from_secs(1);
    }
    if now - state.respawn_sent_ms < 2_000 {
        return Duration::from_millis(500);
    }
    let choice = survival::revive_choice(server, player, state, now);
    state.respawn_sent_ms = now;
    let packet = aaemu_game::systems::skills::resurrect::CSResurrectCharacterPacket {
        in_place: choice.in_place,
    };
    let _ = bot.dispatch(server, packet).await;
    tracing::info!(
        character = %bot.name,
        in_place = choice.in_place,
        reason = choice.reason,
        "Bots: asked to be resurrected"
    );
    Duration::from_secs(1)
}

/// Uses a heal skill or a healing potion when hurt below `heal_at`. `true` when it cast.
async fn heal_if_hurt(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    state: &mut CombatState,
    player: &PlayerRef,
    now: i64,
) -> bool {
    let (hp, ..) = survival::vitals(player);
    if hp >= state.heal_at || now - state.last_cast_ms < CAST_SPACING_MS {
        return false;
    }
    let cs = cast_state(player);
    if cs.casting {
        return false;
    }
    let me = WorldObject::from(player);
    let sig = state.loadout_signature(player);
    let mut options: Vec<(u32, Option<(u64, u32)>)> = Vec::new();
    for id in skills::candidate_ids(server, player) {
        if state.info(server, id).is_some_and(|i| i.heals) {
            options.push((id, None));
        }
    }
    for (item_id, template_id, use_skill) in skills::healing_items(server, state, player) {
        options.push((use_skill, Some((item_id, template_id))));
    }
    if options.is_empty() {
        return false;
    }
    for (id, item) in options {
        if state.dropped.get(&id) == Some(&sig) || state.never.contains(&(id, 0)) {
            continue;
        }
        let Some(template) = server.game_data.systems.skills.skill(id).cloned() else {
            continue;
        };
        if !skills::off_cooldown(&template, &cs, now) {
            continue;
        }
        if aaemu_game::systems::skills::skill::mana_cost(
            server,
            &aaemu_game::systems::skills::state::SkillRun::new(template.clone(), 1),
            &me,
        ) > cs.mp
        {
            continue;
        }
        match skills::usability(server, player, &template, &me) {
            skills::Usable::Yes => {}
            skills::Usable::Requirement(key) => {
                state.learn_requirement(id, 0, key, sig);
                continue;
            }
            _ => continue,
        }
        let outcome = match item {
            Some((item_id, template_id)) => {
                cast::use_item(bot, server, state, item_id, template_id, id, now).await
            }
            None => {
                cast::cast(
                    bot,
                    server,
                    state,
                    id,
                    SkillCaster::Unit(player.obj_id),
                    player.obj_id,
                    0,
                    now,
                )
                .await
            }
        };
        if outcome == CastOutcome::Started {
            tracing::debug!(character = %bot.name, skill = id, "Bots: healed");
            return true;
        }
    }
    false
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn a_fresh_state_sits_idle_and_rests_to_the_recovery_fraction() {
        let state = CombatState::default();
        assert_eq!(state.mode, Mode::Idle);
        assert!(state.rest_until() >= REST_UNTIL);
        assert_eq!(state.kills, 0);
        assert_eq!(state.base_tick, Duration::from_millis(250));
    }

    #[test]
    fn danger_marks_are_raised_not_duplicated() {
        let mut state = CombatState::default();
        state.note_danger(10.0, 10.0, 20);
        state.note_danger(12.0, 10.0, 25);
        assert_eq!(state.danger_marks.len(), 1);
        assert_eq!(state.danger_marks[0].level, 25);
        assert!(state.is_dangerous(11.0, 10.0, 5));
        assert!(!state.is_dangerous(11.0, 10.0, 30), "grown out of it");
    }
}

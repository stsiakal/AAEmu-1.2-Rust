//! The rotation rule evaluator: the `classes-12` rules as priorities on top of the server's own
//! cast checks, with the data scoring of [`super::skills`] as the fallback.
//!
//! A rule passes when its condition tree passes against the bot, its target and the world the
//! server exposes (buff tags, range, cooldown state, enemies around, HP / mana). Among the
//! passing rules the highest `relevance` band wins, then the rule order; inside a rule the first
//! action the server would take now is cast. The skill's own target type decides who it is cast
//! on: only hostile-target skills go to the monster (a damage+heal skill like Antithesis stays
//! hostile); self and friendly skills go on the bot, the only ally a solo bot has. `fillers`
//! are the research's default buttons, tried before the data scoring.
//!
//! Rules are draft data: a rule that the server's checks refuse is skipped (and the skill is
//! learned / backed off by the caller) like any other candidate.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::buffs;
use aaemu_game::world::{NpcRef, PlayerRef, WorldObject};

use super::skills::{cast_state, off_cooldown, skill_gate, Aim, SkillBlock, DEFAULT_ATTACKS};
use super::target::{dist2, enemies_within, me_pos};
use super::CombatState;
use crate::rotation::{Action, Condition};

/// A context cache for one rule evaluation (the enemy scan is the expensive part).
struct Context<'a> {
    server: &'a GameServer,
    player: &'a PlayerRef,
    target: &'a WorldObject,
    dist: f32,
    now: i64,
    learned: &'a HashSet<u32>,
    /// The think's one neighbourhood scan (the enemy counts of the rules).
    all: &'a [NpcRef],
    cs: super::skills::CastState,
    enemies: RefCell<HashMap<u32, usize>>,
}

impl Context<'_> {
    fn me(&self) -> WorldObject {
        WorldObject::from(self.player)
    }

    fn hp_pct(&self) -> f32 {
        let c = self.player.data();
        c.unit.hp as f32 / c.unit.max_hp.max(1) as f32 * 100.0
    }

    fn mana_pct(&self) -> f32 {
        let c = self.player.data();
        if c.unit.max_mp <= 0 {
            100.0
        } else {
            c.unit.mp as f32 / c.unit.max_mp as f32 * 100.0
        }
    }

    fn enemies_within(&self, radius: u32) -> usize {
        if let Some(n) = self.enemies.borrow().get(&radius) {
            return *n;
        }
        let n = enemies_within(self.server, self.all, self.player, radius as f32);
        self.enemies.borrow_mut().insert(radius, n);
        n
    }

    fn ability_active(&self, ability: u8) -> bool {
        let c = self.player.data();
        [c.ability1, c.ability2, c.ability3].contains(&ability)
    }

    fn matches(&self, condition: &Condition) -> bool {
        match condition {
            Condition::All { all } => all.iter().all(|c| self.matches(c)),
            Condition::Any { any } => any.iter().any(|c| self.matches(c)),
            Condition::Not { not } => !self.matches(not),
            Condition::TargetHasTag { target_has_tag } => {
                buffs::check_buffs_by_tag(self.server, self.target, *target_has_tag)
            }
            Condition::SelfHasTag { self_has_tag } => {
                buffs::check_buffs_by_tag(self.server, &self.me(), *self_has_tag)
            }
            Condition::CooldownReady { cooldown_ready } => {
                if !self.learned.contains(cooldown_ready)
                    && !DEFAULT_ATTACKS.contains(cooldown_ready)
                {
                    return false;
                }
                let Some(template) = self.server.game_data.systems.skills.skill(*cooldown_ready)
                else {
                    return false;
                };
                off_cooldown(template, &self.cs, self.now)
            }
            Condition::Range { range } => self.dist >= range.0 && self.dist <= range.1,
            Condition::EnemiesWithin { enemies_within } => {
                self.enemies_within(enemies_within.0) >= enemies_within.1 as usize
            }
            // Solo: the only ally is the bot itself, at distance zero.
            Condition::AllyHpBelow { ally_hp_below } => self.hp_pct() < ally_hp_below.0 as f32,
            Condition::SelfHpBelow { self_hp_below } => self.hp_pct() < *self_hp_below as f32,
            Condition::SelfManaBelow { self_mana_below } => {
                self.mana_pct() < *self_mana_below as f32
            }
            Condition::SelfMissingBuff { self_missing_buff } => {
                let me = self.me();
                !me.with_base_unit(|u| {
                    u.buffs()
                        .effects
                        .iter()
                        .any(|e| e.template.id == *self_missing_buff)
                })
            }
            Condition::TargetCasting { target_casting } => {
                let casting = self
                    .target
                    .with_unit(|u| u.unit().skill_state.skill_task.is_some())
                    .unwrap_or(false);
                casting == *target_casting
            }
            Condition::AllyDead { ally_dead } => !*ally_dead,
            Condition::OutOfCombat { out_of_combat } => {
                let in_battle = self.player.data().unit.is_in_battle;
                in_battle != *out_of_combat
            }
        }
    }
}

/// The first action of `actions` the server would take now, and whether it targets the bot.
///
/// The skill's own aim decides the target: a target skill (a damage skill, even one that also
/// heals, like Antithesis) goes to the monster; self / friendly skills go on the bot; a skill
/// with a placement or doodad target type is skipped (the bot cannot aim it). The same loadout
/// signature, refusal back-off and active-skillset filter as the data scoring apply, so the two
/// paths cannot disagree about a skill being usable.
fn first_castable(
    context: &mut Context<'_>,
    state: &mut CombatState,
    actions: &[Action],
) -> Option<(u32, bool)> {
    for action in actions {
        let Some(info) = state.info(context.server, action.skill) else {
            continue;
        };
        if info.aim == Aim::Unaimable {
            continue;
        }
        if !context.learned.contains(&action.skill) && !DEFAULT_ATTACKS.contains(&action.skill) {
            continue;
        }
        let sig = state.loadout_signature(context.player);
        if state
            .never
            .contains(&(action.skill, target_template(context.target)))
            || state.dropped.get(&action.skill).is_some_and(|s| *s == sig)
        {
            continue;
        }
        if super::cast::refused(
            state,
            context.player,
            action.skill,
            context.target.obj_id(),
            context.now,
        ) {
            continue;
        }
        let Some(template) = context
            .server
            .game_data
            .systems
            .skills
            .skill(action.skill)
            .cloned()
        else {
            continue;
        };
        if template.ability_id != 0 && !context.ability_active(template.ability_id) {
            continue;
        }
        let self_target = info.aim == Aim::SelfOnly;
        let me = WorldObject::from(context.player);
        let target = if self_target { &me } else { context.target };
        match skill_gate(
            context.server,
            context.player,
            &template,
            target,
            &context.cs,
            context.now,
        ) {
            Ok(()) => return Some((action.skill, self_target)),
            Err(SkillBlock::Requirement(key)) => {
                state.learn_requirement(action.skill, target_template(context.target), key, sig);
            }
            Err(_) => {}
        }
    }
    None
}

fn target_template(target: &WorldObject) -> u32 {
    target.with_base_unit(|b| b.template_id())
}

/// The best cast from the rotation, or `None` when no rule or filler yields one.
#[allow(clippy::too_many_arguments)]
pub fn choose_rotation(
    server: &GameServer,
    player: &PlayerRef,
    state: &mut CombatState,
    target: &WorldObject,
    dist: f32,
    now: i64,
    all: &[NpcRef],
) -> Option<(u32, bool)> {
    let rotation = state.rotation.clone()?;
    let learned = super::skills::learned(player);
    let mut context = Context {
        server,
        player,
        target,
        dist,
        now,
        learned: &learned,
        all,
        cs: cast_state(player),
        enemies: RefCell::new(HashMap::new()),
    };
    let mut order: Vec<&crate::rotation::Rule> = rotation.rules.iter().collect();
    order.sort_by_key(|r| std::cmp::Reverse(r.relevance));
    for rule in order {
        if !context.matches(&rule.when) {
            continue;
        }
        if let Some(cast) = first_castable(&mut context, state, &rule.then) {
            return Some(cast);
        }
    }
    first_castable(&mut context, state, &rotation.fillers)
}

/// The distance from the bot to `target` (x/y).
pub fn distance_to(player: &PlayerRef, target: &WorldObject) -> f32 {
    let (px, py, _) = me_pos(player);
    let t = target.transform().local();
    dist2(px, py, t.x, t.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::Rotation;

    fn rule(json: &str) -> crate::rotation::Rule {
        let rotation = Rotation::parse(&format!(
            r#"{{"archetype":"T","role":"melee_dps","rules":[{json}]}}"#
        ))
        .expect("rule parses");
        rotation.rules.into_iter().next().expect("one rule")
    }

    #[test]
    fn an_all_condition_requires_every_part() {
        let r = rule(
            r#"{"id":"a","when":{"all":[{"self_hp_below":60},{"range":[0,4]}]},"then":[{"skill":3}]}"#,
        );
        let Condition::All { all } = &r.when else {
            panic!("all");
        };
        assert_eq!(all.len(), 2);
    }
}

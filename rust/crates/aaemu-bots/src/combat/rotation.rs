//! The rotation rule evaluator: the `classes-12` rules as priorities on top of the server's own
//! cast checks, with the data scoring of [`super::skills`] as the fallback.
//!
//! A rule passes when its condition tree passes against the bot, its target and the world the
//! server exposes (buff tags, range, cooldown state, enemies around, HP / mana). Among the
//! passing rules the highest `relevance` band wins, then the rule order; inside a rule the first
//! action the server would take now is cast. A heal rule (`ally_hp_below`, `self_hp_below` with
//! a healing skill) casts on the bot; damage and control rules cast on the target. `fillers`
//! are the research's default buttons, tried before the data scoring.
//!
//! Rules are draft data: a rule that the server's checks refuse is skipped (and the skill is
//! learned / backed off by the caller) like any other candidate.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::buffs;
use aaemu_game::world::{PlayerRef, WorldObject};

use super::skills::{cast_state, off_cooldown, skill_gate, SkillBlock, SkillKind, DEFAULT_ATTACKS};
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
        let n = enemies_within(self.server, self.player, radius as f32);
        self.enemies.borrow_mut().insert(radius, n);
        n
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
                if !self.learned.contains(cooldown_ready) && !DEFAULT_ATTACKS.contains(cooldown_ready)
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

/// Does the rule's condition tree want a heal on the bot rather than a hostile action?
fn rule_heals_self(condition: &Condition) -> bool {
    match condition {
        Condition::AllyHpBelow { .. } | Condition::SelfHpBelow { .. } => true,
        Condition::All { all } => all.iter().any(rule_heals_self),
        Condition::Any { any } => any.iter().any(rule_heals_self),
        Condition::Not { not } => rule_heals_self(not),
        _ => false,
    }
}

/// The first action of `actions` the server would take now, and whether it targets the bot.
fn first_castable(
    context: &mut Context<'_>,
    state: &mut CombatState,
    actions: &[Action],
    heal_self: bool,
) -> Option<(u32, bool)> {
    for action in actions {
        let Some(info) = state.info(context.server, action.skill) else {
            continue;
        };
        if !context.learned.contains(&action.skill) && !DEFAULT_ATTACKS.contains(&action.skill) {
            continue;
        }
        let self_target = if info.heals && heal_self {
            true
        } else {
            matches!(info.kind, SkillKind::SelfBuff | SkillKind::Support)
        };
        if state
            .never
            .contains(&(action.skill, target_template(context.target)))
            || state.dropped.contains_key(&action.skill)
        {
            continue;
        }
        let (bx, by, _) = me_pos(context.player);
        let cell = ((bx / 2.0).floor() as i32, (by / 2.0).floor() as i32);
        let sig = state.loadout_signature(context.player);
        if state.refused_on.get(&action.skill).is_some_and(|r| {
            r.matches(context.target.obj_id(), cell, sig, context.now)
        }) {
            continue;
        }
        let me = WorldObject::from(context.player);
        let target = if self_target {
            &me
        } else {
            context.target
        };
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
                state.learn_requirement(
                    action.skill,
                    target_template(context.target),
                    key,
                    sig,
                );
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
pub fn choose_rotation(
    server: &GameServer,
    player: &PlayerRef,
    state: &mut CombatState,
    target: &WorldObject,
    dist: f32,
    now: i64,
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
        cs: cast_state(player),
        enemies: RefCell::new(HashMap::new()),
    };
    let mut order: Vec<&crate::rotation::Rule> = rotation.rules.iter().collect();
    order.sort_by_key(|r| std::cmp::Reverse(r.relevance));
    for rule in order {
        if !context.matches(&rule.when) {
            continue;
        }
        let heal_self = rule_heals_self(&rule.when);
        if let Some(cast) = first_castable(&mut context, state, &rule.then, heal_self) {
            return Some(cast);
        }
    }
    first_castable(&mut context, state, &rotation.fillers, true)
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

    #[test]
    fn heal_rules_are_detected_through_all_and_not() {
        let r = rule(
            r#"{"id":"h","when":{"all":[{"ally_hp_below":[90,30]},{"not":{"out_of_combat":true}}]},"then":[{"skill":10534}]}"#,
        );
        assert!(rule_heals_self(&r.when));
        let d = rule(r#"{"id":"d","when":{"target_has_tag":107},"then":[{"skill":18132}]}"#);
        assert!(!rule_heals_self(&d.when));
    }
}

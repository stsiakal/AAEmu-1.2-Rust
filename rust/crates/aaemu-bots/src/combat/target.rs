//! Target selection of the bot combat: hostile monsters in search range, the one that attacks
//! the bot (self-defence), and sane pulls for grinding.
//!
//! This is the port of `aaemu-game/src/systems/bots/combat.rs`'s selection half. The bot is a
//! player, so every relation question goes to the server's own `relation::can_attack`; the
//! selection only ranks what the server would let it hit. A pull prefers the outermost monster
//! of a group (fewest hostile neighbours) and then the nearest, skips monsters tagged by
//! somebody else, monsters more than `Bots.MaxTargetLevelGap` levels above the bot and monsters
//! it learned to leave alone (unreachable, too strong on a death review).

use std::collections::HashMap;

#[cfg(test)]
use aaemu_game::models::npc::Npc;
use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::geometry::Target;
use aaemu_game::systems::skills::relation;
use aaemu_game::world::{NpcRef, PlayerRef, WorldObject};

/// Monsters are looked for within this distance (metres, x/y) when the mode does not say.
pub const SEARCH_RADIUS_M: f32 = 90.0;
/// A pull is refused when more than this many other hostile monsters stand within
/// [`PACK_RADIUS_M`] of the target.
pub const PACK_LIMIT: usize = 1;
pub const PACK_RADIUS_M: f32 = 14.0;
/// A monster that is already attacking the bot within this distance is fought.
pub const ATTACKER_RADIUS_M: f32 = 40.0;
/// Hostile monsters within this distance of the corpse make an in-place revive unsafe.
pub const HOSTILE_NEAR_CORPSE_M: f32 = 35.0;

/// A snapshot of a monster (no object lock is held after it is taken).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NpcView {
    pub obj_id: u32,
    pub template_id: u32,
    pub level: u8,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub hp: i32,
    /// The player holding tagging rights (`0` = none).
    pub tagger: u32,
    /// The monster's current aggro target object id (`0` = none).
    pub aggro_target: u32,
    /// Is the monster casting / channelling a skill right now?
    pub casting: bool,
}

pub fn view_of(npc: &NpcRef) -> NpcView {
    let n = npc.data();
    let t = n.unit.transform;
    NpcView {
        obj_id: npc.obj_id,
        template_id: n.unit.template_id,
        level: n.unit.level,
        x: t.x,
        y: t.y,
        z: t.z,
        hp: n.unit.hp,
        tagger: n.tagger_obj_id,
        aggro_target: n.current_aggro_target,
        casting: n.unit.skill_state.skill_task.is_some(),
    }
}

pub fn dist2(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
}

pub fn me_pos(player: &PlayerRef) -> (f32, f32, f32) {
    let t = player.data().unit.transform;
    (t.x, t.y, t.z)
}

/// The monsters of the bot's neighbourhood within `radius` metres, taken once per think and
/// reused for every target / threat / crowd question.
///
/// `World::around` walks only the grid's neighbouring regions instead of every object of the
/// world instance (`all_npcs` cloned the whole instance for each query), and the distance
/// filter runs here, before any relation check ([`attackable`]) is asked.
pub fn scan_npcs(server: &GameServer, player: &PlayerRef, radius: f32) -> Vec<NpcRef> {
    let me = WorldObject::from(player);
    let (px, py, _) = me_pos(player);
    let Some(world) = server.world.world_of(&me) else {
        return Vec::new();
    };
    world.around(&me, |o| {
        let npc = o.as_npc()?;
        let t = npc.data().unit.transform;
        ((t.x - px).powi(2) + (t.y - py).powi(2) <= radius * radius).then(|| npc.clone())
    })
}

/// Does the server's relation let the bot attack this monster?
pub fn attackable(server: &GameServer, player: &PlayerRef, npc: &NpcRef) -> bool {
    #[cfg(test)]
    RELATION_CHECKS.with(|c| c.set(c.get() + 1));
    let me = Target::Obj(WorldObject::from(player));
    let other = Target::Obj(WorldObject::from(npc.clone()));
    relation::can_attack(server, &me, &other)
}

#[cfg(test)]
thread_local! {
    /// Relation checks asked on this test thread (a far monster must not reach [`attackable`]).
    static RELATION_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn relation_checks() -> usize {
    RELATION_CHECKS.with(|c| c.get())
}

/// What the bot has learned about whom to pull.
#[derive(Debug, Default)]
pub struct PullRules<'a> {
    /// Monsters that could not be reached, ignored until the given time.
    pub ignored: Option<&'a HashMap<u32, i64>>,
    /// Death-review caution about packs (0 = normal).
    pub caution: u8,
    /// Templates the bot is too weak for until it reaches the level.
    pub deferred: Option<&'a HashMap<u32, u8>>,
    /// Whether a monster tagged by another player may still be attacked (an explicit
    /// `/bot fight` target: tagging gives loot rights, it does not make the monster immortal).
    pub allow_tagged: bool,
}

/// The other attackable monsters within `radius` of `v` (a crowd / pack count). The distance
/// filter runs before the relation check.
pub fn hostiles_around(
    server: &GameServer,
    player: &PlayerRef,
    all: &[NpcRef],
    v: &NpcView,
    radius: f32,
) -> usize {
    all.iter()
        .filter(|other| other.obj_id != v.obj_id)
        .filter(|other| {
            let n = other.data();
            n.unit.hp > 0 && dist2(n.unit.transform.x, n.unit.transform.y, v.x, v.y) <= radius
        })
        .filter(|other| attackable(server, player, other))
        .count()
}

/// Is the monster a sane pull for a solo bot? Alive, hostile, not tagged by somebody else (when
/// the rules say so), at most `max_gap` levels above the bot, and not part of a pack. Returns
/// the number of hostile neighbours that decided the pull (the ranking key), computed once.
fn pullable_crowd(
    server: &GameServer,
    player: &PlayerRef,
    all: &[NpcRef],
    npc: &NpcRef,
    rules: &PullRules,
    max_gap: u8,
) -> Option<usize> {
    let v = view_of(npc);
    let my_level = player.data().unit.level;
    if v.hp <= 0 || v.level > my_level.saturating_add(max_gap) {
        return None;
    }
    if rules
        .deferred
        .and_then(|d| d.get(&v.template_id))
        .is_some_and(|need| my_level < *need)
    {
        return None;
    }
    if !rules.allow_tagged && v.tagger != 0 && v.tagger != player.obj_id {
        return None;
    }
    if !attackable(server, player, npc) {
        return None;
    }
    let (limit, radius) = if rules.caution == 0 {
        (PACK_LIMIT, PACK_RADIUS_M)
    } else {
        (0, PACK_RADIUS_M + 6.0 * f32::from(rules.caution.min(4)))
    };
    let crowd = hostiles_around(server, player, all, &v, radius);
    (crowd <= limit).then_some(crowd)
}

/// The best pull among `wanted` templates (empty = any hostile monster) inside `search_radius`
/// of `center`: the one with the fewest monsters around it (the outermost of a group first),
/// then the nearest to `center`. A grind measures from its centre, so a bot pulled out of its
/// radius still does not pull the world around itself.
#[allow(clippy::too_many_arguments)]
pub fn find_target(
    server: &GameServer,
    player: &PlayerRef,
    all: &[NpcRef],
    wanted: &[u32],
    rules: &PullRules,
    center: (f32, f32),
    search_radius: f32,
    max_gap: u8,
    now: i64,
) -> Option<NpcView> {
    let mut best: Option<((usize, f32), NpcView)> = None;
    for npc in all {
        let v = view_of(npc);
        if !wanted.is_empty() && !wanted.contains(&v.template_id) {
            continue;
        }
        if rules
            .ignored
            .and_then(|i| i.get(&v.obj_id))
            .is_some_and(|until| *until > now)
        {
            continue;
        }
        let d = dist2(center.0, center.1, v.x, v.y);
        if d > search_radius {
            continue;
        }
        let Some(crowd) = pullable_crowd(server, player, all, npc, rules, max_gap) else {
            continue;
        };
        let key = (crowd, d);
        let better = best
            .as_ref()
            .is_none_or(|(b, _)| key.0 < b.0 || (key.0 == b.0 && key.1 < b.1));
        if better {
            best = Some((key, v));
        }
    }
    best.map(|(_, v)| v)
}

/// A hostile monster that is already attacking the bot (self-defence).
pub fn find_attacker(all: &[NpcRef], player: &PlayerRef) -> Option<NpcView> {
    let me = player.obj_id;
    let (px, py, _) = me_pos(player);
    all.iter()
        .filter(|npc| {
            let n = npc.data();
            n.unit.hp > 0
                && (n.current_aggro_target == me
                    || n.unit.skill_state.aggro_table.contains_key(&me))
        })
        .map(view_of)
        .filter(|v| dist2(px, py, v.x, v.y) <= ATTACKER_RADIUS_M)
        .min_by(|a, b| dist2(px, py, a.x, a.y).total_cmp(&dist2(px, py, b.x, b.y)))
}

/// How many hostile monsters attack the bot right now (within [`ATTACKER_RADIUS_M`]).
pub fn attackers_on_me(all: &[NpcRef], player: &PlayerRef) -> usize {
    let me = player.obj_id;
    let (px, py, _) = me_pos(player);
    all.iter()
        .filter(|npc| {
            let n = npc.data();
            n.unit.hp > 0
                && (n.current_aggro_target == me
                    || n.unit.skill_state.aggro_table.contains_key(&me))
                && dist2(px, py, n.unit.transform.x, n.unit.transform.y) <= ATTACKER_RADIUS_M
        })
        .count()
}

/// The alive, attackable monsters within `radius` metres of the bot that are not trivially weak
/// (level within two of the bot or above): what a sensible player would not stand next to at 1%
/// health. Distance first, relation only for what is close enough.
pub fn hostiles_near(
    server: &GameServer,
    all: &[NpcRef],
    player: &PlayerRef,
    radius: f32,
) -> Vec<NpcView> {
    let (px, py, _) = me_pos(player);
    let my_level = player.data().unit.level;
    all.iter()
        .filter_map(|npc| {
            let v = view_of(npc);
            (v.hp > 0 && v.level.saturating_add(2) >= my_level && dist2(px, py, v.x, v.y) <= radius)
                .then_some((npc, v))
        })
        .filter(|(npc, _)| attackable(server, player, npc))
        .map(|(_, v)| v)
        .collect()
}

/// How many attackable monsters stand within `radius` metres of the bot (`enemies_within`).
pub fn enemies_within(
    server: &GameServer,
    all: &[NpcRef],
    player: &PlayerRef,
    radius: f32,
) -> usize {
    let (px, py, _) = me_pos(player);
    all.iter()
        .filter_map(|npc| {
            let v = view_of(npc);
            (v.hp > 0 && dist2(px, py, v.x, v.y) <= radius).then_some((npc, v))
        })
        .filter(|(npc, _)| attackable(server, player, npc))
        .count()
}

/// The equipped main-hand item template, for the starter-weapon check of the tests.
pub fn mainhand_template(player: &PlayerRef) -> Option<u32> {
    let c = player.data();
    let equipment = c
        .inventory
        .get(aaemu_game::models::container::slot_type::EQUIPMENT)?;
    let e = equipment.lock().expect("equipment poisoned");
    e.items
        .iter()
        .find(|i| i.slot == aaemu_game::models::container::equip_slot::MAINHAND)
        .map(|i| i.template_id)
}

/// Spawns a test monster at `(x, y)` in instance 0 (unit-test fixture).
#[cfg(test)]
pub(crate) fn spawn_monster(
    server: &GameServer,
    obj_id: u32,
    template: u32,
    level: u8,
    hp: i32,
    x: f32,
    y: f32,
) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.level = level;
    n.unit.hp = hp;
    n.unit.max_hp = hp;
    n.unit.name = format!("Monster{obj_id}");
    n.unit.transform.set_xyz(x, y, 100.0);
    let npc = aaemu_game::world::Entity::new(obj_id, n);
    let world = server.world.world(0).expect("instance 0");
    world.spawn(&npc, &server.view_ctx());
    npc
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aaemu_game::test_support::{spawn_player, test_character, test_server, TestClient};

    use super::*;

    fn fixture() -> (Arc<GameServer>, PlayerRef) {
        let server = Arc::new(test_server());
        server.set_self();
        let client = TestClient::new(&server, 1, 1);
        let mut c = test_character(1, "Bot", 100.0, 100.0);
        c.unit.level = 5;
        let player = spawn_player(&server, &client, c, 0x100);
        (server, player)
    }

    fn pull(
        server: &GameServer,
        player: &PlayerRef,
        scan: &[NpcRef],
        radius: f32,
        now: i64,
    ) -> Option<NpcView> {
        find_target(
            server,
            player,
            scan,
            &[],
            &PullRules::default(),
            (100.0, 100.0),
            radius,
            3,
            now,
        )
    }

    #[tokio::test]
    async fn the_outermost_then_nearest_monster_wins_the_pull() {
        let (server, player) = fixture();
        // A pair, a lone monster and a farther lone monster.
        spawn_monster(&server, 0x201, 1, 2, 50, 110.0, 100.0);
        spawn_monster(&server, 0x202, 1, 2, 50, 112.0, 100.0);
        spawn_monster(&server, 0x203, 1, 2, 50, 100.0, 118.0);
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let target = pull(&server, &player, &scan, SEARCH_RADIUS_M, 0).expect("a target");
        assert_eq!(target.obj_id, 0x203, "the lone monster beats the pair");
    }

    #[tokio::test]
    async fn monsters_above_the_level_gap_and_tagged_ones_are_skipped() {
        let (server, player) = fixture();
        spawn_monster(&server, 0x201, 1, 20, 50, 100.0, 130.0);
        let tagged = spawn_monster(&server, 0x202, 2, 2, 50, 100.0, 145.0);
        tagged.data().tagger_obj_id = 0x999;
        spawn_monster(&server, 0x203, 3, 2, 50, 107.0, 100.0);
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let target = pull(&server, &player, &scan, SEARCH_RADIUS_M, 0).expect("a target");
        assert_eq!(target.obj_id, 0x203, "only the pullable monster is left");
    }

    #[tokio::test]
    async fn an_explicit_fight_may_attack_a_tagged_monster() {
        let (server, player) = fixture();
        let tagged = spawn_monster(&server, 0x202, 2, 2, 50, 106.0, 100.0);
        tagged.data().tagger_obj_id = 0x999;
        let rules = PullRules {
            allow_tagged: true,
            ..Default::default()
        };
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let target = find_target(
            &server,
            &player,
            &scan,
            &[2],
            &rules,
            (100.0, 100.0),
            SEARCH_RADIUS_M,
            3,
            0,
        )
        .expect("a target");
        assert_eq!(target.obj_id, 0x202);
    }

    #[tokio::test]
    async fn an_attacker_is_found_even_when_it_is_farther_than_the_pull_radius() {
        let (server, player) = fixture();
        let mob = spawn_monster(&server, 0x201, 1, 2, 50, 130.0, 100.0);
        mob.data().current_aggro_target = player.obj_id;
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let attacker = find_attacker(&scan, &player).expect("the attacker");
        assert_eq!(attacker.obj_id, 0x201);
        assert!(pull(&server, &player, &scan, 20.0, 0).is_none());
    }

    #[tokio::test]
    async fn ignored_and_deferred_monsters_are_not_pulled() {
        let (server, player) = fixture();
        spawn_monster(&server, 0x201, 9, 2, 50, 105.0, 100.0);
        let mut ignored = HashMap::new();
        ignored.insert(0x201u32, 1_000i64);
        let mut deferred = HashMap::new();
        deferred.insert(9u32, 10u8);
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let target = find_target(
            &server,
            &player,
            &scan,
            &[],
            &PullRules {
                ignored: Some(&ignored),
                deferred: Some(&deferred),
                ..Default::default()
            },
            (100.0, 100.0),
            SEARCH_RADIUS_M,
            3,
            0,
        );
        assert!(target.is_none());
    }

    /// Many monsters stand in the neighbouring regions but outside the pull radius: they are
    /// filtered by distance before the relation check, so the server is asked about the near one
    /// only (the scan is `World::around`, not an `all_npcs` walk of the whole instance).
    #[tokio::test]
    async fn far_monsters_are_filtered_before_the_relation_check() {
        let (server, player) = fixture();
        for i in 0..200u32 {
            let x = 200.0 + (i % 10) as f32 * 4.0;
            let y = 150.0 + (i / 10) as f32 * 4.0;
            spawn_monster(&server, 0x1000 + i, 7, 2, 50, x, y);
        }
        spawn_monster(&server, 0x900, 3, 2, 50, 104.0, 100.0);
        let before = relation_checks();
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        assert_eq!(scan.len(), 1, "only the near monster is in the scan");
        let target = pull(&server, &player, &scan, SEARCH_RADIUS_M, 0).expect("the near target");
        assert_eq!(target.obj_id, 0x900);
        let checks = relation_checks() - before;
        assert!(
            checks <= 4,
            "200 far monsters reached the relation check: {checks} calls"
        );
    }

    /// In a grind the pull radius is measured from the grind centre: a monster right next to a
    /// bot that a fight pulled out of the circle is not a leg of the grind.
    #[tokio::test]
    async fn a_grind_pull_is_measured_from_the_centre() {
        let (server, player) = fixture();
        // The bot stands at (130, 100) (pulled out of the radius-20 grind at (100, 100)); the
        // monster is 5 m away from the bot but 25 m from the centre.
        aaemu_game::world::WorldObject::from(&player)
            .with_transform(|t| t.set_xyz(130.0, 100.0, 100.0));
        spawn_monster(&server, 0x201, 1, 2, 50, 125.0, 100.0);
        let scan = scan_npcs(&server, &player, SEARCH_RADIUS_M);
        let target = pull(&server, &player, &scan, 20.0, 0);
        assert!(target.is_none(), "{target:?} outside the grind centre");
    }
}

//! Flow tests of the bot combat on `test_server`: targets, casts, kills, sequence behaviour and
//! survival, all through the real packet handlers.
//!
//! Every fixture is a real [`BotSession`](crate::host::BotSession) on a socketless connection;
//! the only shortcuts are the test skill templates (fixed damage, like the game's own flow
//! tests) and setting HP / time directly to stage a death.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aaemu_game::data::items::{ItemTemplate, LootPackDroppingNpc};
use aaemu_game::data::GameData;
use aaemu_game::models::char_data::Skill;
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::server::GameServer;
use aaemu_game::systems::economy::loot_data::{Loot, LootPack};
use aaemu_game::systems::skills::data::templates::BuffTemplate;
use aaemu_game::systems::skills::data::SkillTemplate;
use aaemu_game::systems::skills::types::SkillCaster;
use aaemu_game::test_support::{
    install_flat_heightmap, install_test_terrain, test_character, test_server_with,
};
use aaemu_game::world::PlayerRef;

use crate::combat::cast::{self, CastOutcome};
use crate::combat::target::spawn_monster;
use crate::combat::{loot, Mode};
use crate::host::BotSession;
use crate::life;
use crate::rotation::Rotation;

const KILL_SKILL: u32 = 60_001;
const SLOW_SKILL: u32 = 60_002;
const INACTIVE_SKILL: u32 = 60_003;
/// Denied by the [`REFUSAL_BUFF`] while it is active (the server's buff-skill requirement).
const REFUSED_SKILL: u32 = 60_004;
const REFUSED_HEAL: u32 = 60_005;
const REFUSAL_BUFF: u32 = 90_001;
const MONSTER: u32 = 900;
const MONSTER_B: u32 = 901;
const DROP_ITEM: u32 = 7_001;
const LOOT_PACK: u32 = 77;

fn add_damage_skill(data: &mut GameData, id: u32, damage: i32, cast_ms: i32, ability_id: u8) {
    use aaemu_game::systems::skills::data::effects::{DamageEffect, EffectKind, EffectTemplate};
    use aaemu_game::systems::skills::data::templates::SkillEffect;

    let effect_id = 900_000 + id;
    let effect = DamageEffect {
        id: effect_id,
        damage_type: 1,
        fixed_min: damage,
        fixed_max: damage,
        multiplier: 1.0,
        use_fixed_damage: true,
        dps_multiplier: 1.0,
        ..Default::default()
    };
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(effect)),
        effect_id,
    );
    let mut template = SkillTemplate {
        id,
        ability_id,
        target_type: 4,
        max_range: 25,
        casting_time: cast_ms,
        ..Default::default()
    };
    template.effects.push(SkillEffect {
        effect_id,
        template: Some((EffectKind::DamageEffect, effect_id)),
        start_level: 0,
        end_level: 255,
        chance: 100,
        application_method: 1,
        ..Default::default()
    });
    data.systems.skills.add_skill(template);
}

fn add_heal_skill(data: &mut GameData, id: u32, heal: i32) {
    use aaemu_game::systems::skills::data::effects::{EffectKind, EffectTemplate, HealEffect};

    let effect_id = 910_000 + id;
    let effect = HealEffect {
        id: effect_id,
        use_fixed_heal: true,
        fixed_min: heal,
        fixed_max: heal,
        use_level_heal: false,
        level_md: 0.0,
        level_va_start: 0,
        level_va_end: 0,
        percent: false,
        use_charged_buff: false,
        charged_buff_id: 0,
        charged_mul: 0.0,
        slave_applicable: false,
        ignore_heal_aggro: false,
        dps_multiplier: 0.0,
        actability_group_id: 0,
        actability_step: 0,
        actability_mul: 0.0,
        actability_add: 0.0,
    };
    data.systems.skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(effect),
        effect_id,
    );
    let mut template = SkillTemplate {
        id,
        target_type: 0,
        max_range: 0,
        casting_time: 0,
        ..Default::default()
    };
    use aaemu_game::systems::skills::data::templates::SkillEffect;
    template.effects.push(SkillEffect {
        effect_id,
        template: Some((EffectKind::HealEffect, effect_id)),
        start_level: 0,
        end_level: 255,
        chance: 100,
        application_method: 1,
        ..Default::default()
    });
    data.systems.skills.add_skill(template);
}

/// The test world: a flat surface, the combat skills, a default melee attack and an always-drop
/// loot pack on the monster template.
fn combat_server() -> Arc<GameServer> {
    let mut data = GameData::empty();
    add_damage_skill(&mut data, KILL_SKILL, 5_000, 0, 0);
    add_damage_skill(&mut data, SLOW_SKILL, 5_000, 800, 0);
    add_damage_skill(&mut data, INACTIVE_SKILL, 5_000, 0, 5);
    add_damage_skill(&mut data, 3, 50, 0, 0);
    add_damage_skill(&mut data, REFUSED_SKILL, 5_000, 0, 0);
    add_heal_skill(&mut data, REFUSED_HEAL, 5_000);
    data.systems.skills.add_buff(BuffTemplate {
        id: REFUSAL_BUFF,
        duration: 600_000,
        ..Default::default()
    });
    data.systems.skills.buff_skill_requirements.push(
        aaemu_game::systems::skills::buff_reqs::BuffSkillRequirement {
            id: 1,
            buff_id: REFUSAL_BUFF,
            default_result: true,
            skill_ids: [REFUSED_SKILL, REFUSED_HEAL].into(),
            ..Default::default()
        },
    );
    data.skills
        .default_skills
        .push(aaemu_game::data::skills::DefaultSkill {
            skill_id: 3,
            slot: 0,
            add_to_slot: true,
        });
    data.items
        .templates
        .insert(DROP_ITEM, ItemTemplate::plain(DROP_ITEM, 20));
    data.items.loot_pack_dropping_npc.insert(
        MONSTER,
        vec![LootPackDroppingNpc {
            id: 1,
            npc_id: MONSTER,
            loot_pack_id: LOOT_PACK,
            default_pack: false,
        }],
    );
    let entry = Loot {
        id: 1,
        group: 0,
        item_id: DROP_ITEM,
        drop_rate: 10_000_000,
        min_amount: 1,
        max_amount: 1,
        loot_pack_id: LOOT_PACK,
        grade_id: 0,
        always_drop: true,
    };
    data.systems.economy.loot.packs.insert(
        LOOT_PACK,
        LootPack {
            id: LOOT_PACK,
            group_count: 0,
            loots: vec![entry],
            groups: Vec::new(),
            actability_groups: Vec::new(),
            loots_by_group_no: vec![(0, vec![entry])],
        },
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    aaemu_game::systems::skills::bind(&server);
    install_flat_heightmap(&server, 100.0);
    install_test_terrain(&server, 100.0);
    server
}

async fn bot(
    server: &Arc<GameServer>,
    id: u32,
    name: &str,
    x: f32,
    y: f32,
    skills: &[u32],
) -> Arc<BotSession> {
    let conn = SocketlessConnection::new(server, 1_000_000 + id);
    let mut character = test_character(id, name, x, y);
    character.unit.level = 10;
    character.rez_wait_duration = 0;
    for skill in skills {
        character.skills.skills.push(Skill {
            id: *skill,
            level: 1,
        });
    }
    let bot = life::enter_loaded(server, conn, character).expect("bot enters");
    // The fixture skips the DB stat load; a live character has an HP pool before fighting.
    if let Some(player) = bot.player().await {
        let mut c = player.data();
        c.unit.hp = 1_000;
        c.unit.max_hp = 1_000;
        c.unit.mp = 500;
        c.unit.max_mp = 500;
    }
    bot.configure(None, 3, 50).await;
    bot
}

fn player_of(bot: &Arc<BotSession>) -> PlayerRef {
    futures::executor::block_on(bot.player()).expect("a live bot")
}

/// Gives the bot the buff a buff-skill requirement denies [`REFUSED_SKILL`] / [`REFUSED_HEAL`]
/// with: the server refuses those casts while every bot-side check passes.
fn apply_refusal_buff(server: &Arc<GameServer>, player: &PlayerRef) {
    use aaemu_game::systems::skills::buffs;
    let template = BuffTemplate {
        id: REFUSAL_BUFF,
        duration: 600_000,
        ..Default::default()
    };
    let me = aaemu_game::world::WorldObject::from(player);
    let buff = buffs::new_buff(
        server,
        &template,
        Some(&me),
        &SkillCaster::Unit(player.obj_id),
        None,
        aaemu_game::world::now_ms(),
    );
    // Put the effect on the unit directly: the fixture has no stat formulas, and a buff event
    // would recalculate (and zero) the test character's max hp.
    player.data().unit.buffs.effects.push(buff);
}

async fn run_for(server: &Arc<GameServer>, bot: &Arc<BotSession>, duration: Duration) {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if !bot.tick(server).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until(
    server: &Arc<GameServer>,
    bot: &Arc<BotSession>,
    timeout: Duration,
    mut done: impl FnMut() -> bool,
) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if done() {
            return true;
        }
        if !bot.tick(server).await {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    done()
}

fn monster_hp(server: &GameServer, obj_id: u32) -> i32 {
    server
        .world
        .object(obj_id)
        .and_then(|o| o.with_unit(|u| u.unit().hp))
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_kills_a_monster_through_the_real_skill_packets_and_loots_it() {
    let server = combat_server();
    let bot = bot(&server, 2, "Fighter", 100.0, 100.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 500, 102.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: vec![MONSTER],
        radius: 20.0,
    })
    .await;

    let killed = wait_until(&server, &bot, Duration::from_secs(10), || {
        monster_hp(&server, 0x400) <= 0
    })
    .await;
    assert!(killed, "the monster died");
    // The test server has no economy/npc hook wiring (that is the startup path); generate the
    // corpse's loot the way the npc death bridge does, before the bot sees the death.
    let owner = server.world.object(0x400).expect("corpse");
    let killer = aaemu_game::world::WorldObject::from(player_of(&bot));
    aaemu_game::systems::economy::looting::LootManager::generate_loot(
        &server,
        &owner,
        Some(&killer),
    );
    let counted = wait_until(&server, &bot, Duration::from_secs(2), || {
        bot.brain().try_lock().ok().is_some_and(|s| s.kills == 1)
    })
    .await;
    assert!(counted, "the kill was counted");
    {
        let state = bot.brain().lock().await;
        assert!(
            state.casts_started >= 1,
            "the cast went through the handler"
        );
    }
    // The kill leaves a corpse with loot; the next thinks open it like a client.
    let looted = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.looted_items > 0)
    })
    .await;
    assert!(looted, "the bot opened the corpse's loot bag");
    assert_eq!(
        server.systems.economy.loot.item_count(0x400),
        0,
        "the drop was taken"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_dying_mid_cast_is_replaced_by_the_next_one() {
    let server = combat_server();
    let bot = bot(&server, 2, "Caster", 100.0, 100.0, &[SLOW_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 102.0, 100.0);
    spawn_monster(&server, 0x401, MONSTER_B, 2, 5_000, 104.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;

    // Wait for the slow cast at the first monster, then kill it mid-cast.
    let started = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x400) && s.casts_started >= 1)
    })
    .await;
    assert!(started, "the bot cast at the first monster");
    let player = player_of(&bot);
    assert!(
        player.data().unit.skill_state.skill_task.is_some(),
        "the cast is still running"
    );
    {
        let npc = server.world.object(0x400).expect("corpse");
        npc.with_unit(|u| u.unit_mut().hp = 0);
    }

    let switched = wait_until(&server, &bot, Duration::from_secs(8), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x401))
    })
    .await;
    assert!(switched, "the bot picked the second monster");
    let state = bot.brain().lock().await;
    assert_eq!(state.kills, 1, "the dead monster was counted");
    assert_eq!(state.target, Some(0x401));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_bot_resurrects_like_a_client_and_resumes_the_hunt() {
    let server = combat_server();
    let bot = bot(&server, 2, "Fighter", 100.0, 100.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 5, 50_000, 118.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: vec![MONSTER],
        radius: 60.0,
    })
    .await;
    // One think to acquire the target.
    run_for(&server, &bot, Duration::from_millis(500)).await;

    let player = player_of(&bot);
    {
        let mut c = player.data();
        c.unit.hp = 0;
        c.dead_time = aaemu_game::world::now_ms() / 1000;
        c.rez_wait_duration = 0;
        c.skill_char.resurrect_offer = false;
    }
    let alive = wait_until(&server, &bot, Duration::from_secs(5), || {
        player.data().unit.hp > 0
    })
    .await;
    assert!(
        alive,
        "the bot resurrected itself through CSResurrectCharacterPacket"
    );
    {
        let state = bot.brain().lock().await;
        assert_eq!(state.deaths, 1);
    }
    let resumed = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x400))
    })
    .await;
    assert!(resumed, "the bot hunts the same monster again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_cast_is_backed_off_and_not_retried_in_a_loop() {
    let server = combat_server();
    // A buff the bot carries denies this skill server-side (a buff-skill requirement the quiet
    // precheck does not apply). The bot must see the refusal and back the skill off.
    let bot = bot(
        &server,
        2,
        "Refused",
        100.0,
        100.0,
        &[REFUSED_SKILL, KILL_SKILL],
    )
    .await;
    apply_refusal_buff(&server, &bot.player().await.expect("a live bot"));
    spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 102.0, 100.0);
    {
        let rotation = Rotation::parse(
            r#"{"archetype":"Test","role":"melee_dps","rules":[],
                "fillers":[{"skill":60004,"name":"Denied"},{"skill":60001,"name":"Hit"}]}"#,
        )
        .expect("rotation");
        bot.brain().lock().await.rotation = Some(Arc::new(rotation));
    }
    bot.set_mode(Mode::Fight {
        wanted: vec![MONSTER],
        radius: 20.0,
    })
    .await;

    run_for(&server, &bot, Duration::from_millis(2_000)).await;
    {
        let state = bot.brain().lock().await;
        assert!(
            (1..=2).contains(&state.casts_refused),
            "one refusal per back-off window, got {}",
            state.casts_refused
        );
        assert!(
            state.refused_on.contains_key(&REFUSED_SKILL),
            "the skill is backed off for this target and spot"
        );
    }
    // The 1.5 s back-off cannot keep up with the 50 ms think: ten seconds of trying stay at
    // the back-off rate instead of one refusal per think.
    run_for(&server, &bot, Duration::from_millis(8_000)).await;
    let state = bot.brain().lock().await;
    assert!(
        state.casts_refused <= 7,
        "refusals are rate-limited, got {}",
        state.casts_refused
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_bots_fight_the_same_monster() {
    let server = combat_server();
    let a = bot(&server, 2, "FighterA", 100.0, 100.0, &[KILL_SKILL]).await;
    let b = bot(&server, 3, "FighterB", 100.0, 102.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 20_000, 101.0, 101.0);
    for fighter in [&a, &b] {
        fighter
            .set_mode(Mode::Fight {
                wanted: vec![MONSTER],
                radius: 20.0,
            })
            .await;
    }
    let both_done = || {
        let a_state = a.brain().try_lock().ok();
        let b_state = b.brain().try_lock().ok();
        a_state.is_some_and(|s| s.casts_started > 0 && s.target == Some(0x400))
            && b_state.is_some_and(|s| s.casts_started > 0 && s.target == Some(0x400))
    };
    let end = Instant::now() + Duration::from_secs(8);
    let mut both = both_done();
    while !both && Instant::now() < end {
        let _ = a.tick(&server).await;
        let _ = b.tick(&server).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        both = both_done();
    }
    assert!(both, "both bots target and cast at the same monster");
    assert!(
        monster_hp(&server, 0x400) < 20_000,
        "the monster took damage from both"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grind_pulls_a_monster_in_its_radius_and_kills_it() {
    let server = combat_server();
    let bot = bot(&server, 2, "Grinder", 100.0, 100.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 500, 112.0, 100.0);
    bot.set_mode(Mode::Grind {
        center: (100.0, 100.0),
        radius: 30.0,
    })
    .await;
    let killed = wait_until(&server, &bot, Duration::from_secs(10), || {
        monster_hp(&server, 0x400) <= 0
    })
    .await;
    assert!(killed, "the grinder walked to and killed a monster");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_without_learned_skills_auto_attacks_its_target_down() {
    let server = combat_server();
    let bot = bot(&server, 2, "Newbie", 100.0, 100.0, &[]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 100, 101.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: vec![MONSTER],
        radius: 20.0,
    })
    .await;
    let killed = wait_until(&server, &bot, Duration::from_secs(10), || {
        monster_hp(&server, 0x400) <= 0
    })
    .await;
    assert!(killed, "the default auto-attack killed the monster");
}

/// Every walk of this test times out at once (a real failed `goto`): a target beyond the
/// skill's reach is never given up before this fix and its plan replanned every 300 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_target_is_ignored_and_the_next_one_attacked() {
    let server = combat_server();
    let bot = bot(&server, 2, "Blocker", 100.0, 100.0, &[KILL_SKILL]).await;
    bot.set_walk_deadline_cap(Duration::from_millis(1));
    let now = aaemu_game::world::now_ms();
    spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 140.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 60.0,
    })
    .await;

    let ignored = wait_until(&server, &bot, Duration::from_secs(8), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.ignored.get(&0x400).is_some_and(|t| *t > now))
    })
    .await;
    assert!(ignored, "the unreachable target was left alone");
    assert_eq!(monster_hp(&server, 0x400), 5_000, "it was never reached");

    spawn_monster(&server, 0x401, MONSTER_B, 2, 500, 101.0, 100.0);
    let killed = wait_until(&server, &bot, Duration::from_secs(8), || {
        monster_hp(&server, 0x401) <= 0
    })
    .await;
    assert!(killed, "the next target was attacked");
}

/// A corpse the walker cannot reach is given up after one failed walk, not walked to forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_corpse_is_skipped() {
    let server = combat_server();
    let bot = bot(&server, 2, "Looter", 100.0, 100.0, &[KILL_SKILL]).await;
    bot.set_walk_deadline_cap(Duration::from_millis(1));
    spawn_monster(&server, 0x400, MONSTER, 2, 0, 140.0, 100.0);
    let owner = server.world.object(0x400).expect("corpse");
    let killer = aaemu_game::world::WorldObject::from(player_of(&bot));
    aaemu_game::systems::economy::looting::LootManager::generate_loot(
        &server,
        &owner,
        Some(&killer),
    );
    assert!(
        loot::loot_count(&server, 0x400).is_some_and(|n| n > 0),
        "the corpse has loot"
    );
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;
    bot.brain().lock().await.loot_target = Some(0x400);

    let skipped = wait_until(&server, &bot, Duration::from_secs(8), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.loot_target.is_none())
    })
    .await;
    assert!(skipped, "the unreachable corpse was given up");
    let state = bot.brain().lock().await;
    assert_eq!(state.looted_items, 0, "nothing was looted from afar");
}

/// A resting bot that a monster starts attacking breaks the rest and fights back on the next
/// think (it used to keep resting, and the early return skipped target selection).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resting_bot_hit_by_a_monster_fights_back() {
    let server = combat_server();
    let bot = bot(&server, 2, "Sleeper", 100.0, 100.0, &[KILL_SKILL]).await;
    let player = player_of(&bot);
    {
        let mut c = player.data();
        c.unit.hp = 200; // below REST_BELOW, out of battle: the bot rests
    }
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;
    run_for(&server, &bot, Duration::from_millis(300)).await;
    assert!(
        bot.brain().lock().await.resting,
        "the hurt bot started resting"
    );

    let mob = spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 104.0, 100.0);
    mob.data().current_aggro_target = player.obj_id;
    let fought = wait_until(&server, &bot, Duration::from_secs(8), || {
        monster_hp(&server, 0x400) < 5_000
    })
    .await;
    assert!(fought, "the resting bot fought back");
    assert!(!bot.brain().lock().await.resting, "the rest was broken");
}

/// A grind answers an attacker even when the bot was pulled outside its radius (the radius is
/// only a pull filter, never a reason to ignore something hitting the bot).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_outside_its_grind_radius_answers_an_attacker() {
    let server = combat_server();
    let bot = bot(&server, 2, "Pulled", 140.0, 100.0, &[KILL_SKILL]).await;
    let player = player_of(&bot);
    let mob = spawn_monster(&server, 0x400, MONSTER, 2, 500, 144.0, 100.0);
    mob.data().current_aggro_target = player.obj_id;
    bot.set_mode(Mode::Grind {
        center: (100.0, 100.0),
        radius: 20.0,
    })
    .await;

    let killed = wait_until(&server, &bot, Duration::from_secs(8), || {
        monster_hp(&server, 0x400) <= 0
    })
    .await;
    assert!(
        killed,
        "the bot answered the attacker outside its grind radius"
    );
}

/// A refused heal is backed off like any cast and no second `CSStartSkill` leaves in the same
/// think (the rotation used to cast right after the refused heal).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_heal_is_backed_off_and_only_one_cast_leaves_per_think() {
    let server = combat_server();
    let bot = bot(
        &server,
        2,
        "Healer",
        100.0,
        100.0,
        &[REFUSED_HEAL, KILL_SKILL],
    )
    .await;
    let player = bot.player().await.expect("a live bot");
    {
        let mut c = player.data();
        c.unit.hp = 300; // below HEAL_BELOW: the heal is tried first
    }
    apply_refusal_buff(&server, &player);
    spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 101.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;

    bot.tick(&server).await;
    {
        let state = bot.brain().lock().await;
        assert_eq!(state.casts_refused, 1, "the heal was refused once");
        assert!(
            state.refused_on.contains_key(&REFUSED_HEAL),
            "the heal is backed off"
        );
        assert_eq!(
            state.casts_started, 0,
            "the rotation cast in the same think as the refused heal"
        );
    }
    // Inside the back-off window the heal is not resent; the fight still goes on.
    run_for(&server, &bot, Duration::from_millis(600)).await;
    let state = bot.brain().lock().await;
    assert_eq!(state.casts_refused, 1, "the refused heal was not retried");
    assert!(
        state.max_casts_per_think <= 1,
        "one think sent {} casts",
        state.max_casts_per_think
    );
}

/// A removed bot sends nothing (no combat packet reaches the handler) and its fight state is
/// gone with the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removed_bot_sends_nothing_and_clears_its_combat_state() {
    let server = combat_server();
    let bot = bot(&server, 2, "Removed", 100.0, 100.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 50_000, 101.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;
    let fighting = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x400) && s.casts_started > 0)
    })
    .await;
    assert!(fighting, "the bot was fighting");

    bot.cancel_navigation();
    bot.logout(&server).await;
    bot.disconnect(&server).await;
    let sent = bot.packets_dispatched_settled().await;

    let player = player_of(&bot);
    let mut state = bot.brain().lock().await;
    assert_eq!(state.target, None);
    assert_eq!(state.mode, Mode::Idle);
    assert!(state.refused_on.is_empty(), "refusals outlived the session");
    assert!(state.ignored.is_empty(), "ignores outlived the session");
    assert_eq!(state.loot_target, None);
    assert!(!state.resting);
    let outcome = cast::cast(
        &bot,
        &server,
        &mut state,
        KILL_SKILL,
        SkillCaster::Unit(player.obj_id),
        0x400,
        MONSTER,
        aaemu_game::world::now_ms(),
    )
    .await;
    assert_eq!(
        outcome,
        CastOutcome::Refused,
        "an offline bot's cast is refused locally"
    );
    drop(state);
    assert_eq!(
        bot.packets_dispatched(),
        sent,
        "a packet left the removed bot"
    );
}

/// A new `/bot fight` or `/bot grind` command drops the old fight state with its target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_command_clears_the_old_target() {
    let server = combat_server();
    let bot = bot(&server, 2, "Redirected", 100.0, 100.0, &[KILL_SKILL]).await;
    spawn_monster(&server, 0x400, MONSTER, 2, 50_000, 101.0, 100.0);
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;
    let targeted = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x400))
    })
    .await;
    assert!(targeted, "the bot targeted the monster");
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 10.0,
    })
    .await;
    let state = bot.brain().lock().await;
    assert_eq!(state.target, None, "the old target outlived the command");
}

/// A monster that keeps attacking from a spot the bot cannot walk to must not be re-acquired
/// after it was ignored: the bot retreats instead of standing still under its hits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_attacker_is_not_reacquired_and_the_bot_retreats() {
    let server = combat_server();
    let bot = bot(&server, 2, "Bait", 100.0, 100.0, &[KILL_SKILL]).await;
    let player = player_of(&bot);
    bot.set_walk_deadline_cap(Duration::from_millis(1));
    let mob = spawn_monster(&server, 0x400, MONSTER, 2, 50_000, 135.0, 100.0);
    mob.data().current_aggro_target = player.obj_id;
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 60.0,
    })
    .await;

    let ignored = wait_until(&server, &bot, Duration::from_secs(8), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.is_ignored(0x400, aaemu_game::world::now_ms()))
    })
    .await;
    assert!(ignored, "the unreachable attacker was never ignored");

    let retreated = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target != Some(0x400) && s.fleeing_to.is_some())
    })
    .await;
    assert!(
        retreated,
        "the bot re-acquired or stood under the unreachable attacker"
    );
    let state = bot.brain().lock().await;
    assert_ne!(state.target, Some(0x400));
    assert!(state.is_ignored(0x400, aaemu_game::world::now_ms()));
}

/// With an ignored attacker that is nearer than a live one, the bot engages the live attacker
/// instead of the ignored one (the attacker path applies the ignore window too).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ignored_attacker_does_not_shadow_a_live_one() {
    let server = combat_server();
    let bot = bot(&server, 2, "Split", 100.0, 100.0, &[KILL_SKILL]).await;
    let player = player_of(&bot);
    let ignored = spawn_monster(&server, 0x400, MONSTER, 2, 50_000, 102.0, 100.0);
    ignored.data().current_aggro_target = player.obj_id;
    let live = spawn_monster(&server, 0x401, MONSTER_B, 2, 500, 104.0, 100.0);
    live.data().current_aggro_target = player.obj_id;
    bot.set_mode(Mode::Fight {
        wanted: Vec::new(),
        radius: 20.0,
    })
    .await;
    bot.brain()
        .lock()
        .await
        .ignored
        .insert(0x400, aaemu_game::world::now_ms() + 60_000);

    let targeted = wait_until(&server, &bot, Duration::from_secs(5), || {
        bot.brain()
            .try_lock()
            .ok()
            .is_some_and(|s| s.target == Some(0x401))
    })
    .await;
    assert!(
        targeted,
        "the live attacker was shadowed by the ignored one"
    );
}

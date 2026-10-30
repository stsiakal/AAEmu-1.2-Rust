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
use aaemu_game::systems::skills::data::SkillTemplate;
use aaemu_game::test_support::{
    install_flat_heightmap, install_test_terrain, test_character, test_server_with,
};
use aaemu_game::world::PlayerRef;

use crate::combat::target::spawn_monster;
use crate::combat::Mode;
use crate::host::BotSession;
use crate::life;
use crate::rotation::Rotation;

const KILL_SKILL: u32 = 60_001;
const SLOW_SKILL: u32 = 60_002;
const INACTIVE_SKILL: u32 = 60_003;
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

/// The test world: a flat surface, the combat skills, a default melee attack and an always-drop
/// loot pack on the monster template.
fn combat_server() -> Arc<GameServer> {
    let mut data = GameData::empty();
    add_damage_skill(&mut data, KILL_SKILL, 5_000, 0, 0);
    add_damage_skill(&mut data, SLOW_SKILL, 5_000, 800, 0);
    add_damage_skill(&mut data, INACTIVE_SKILL, 5_000, 0, 5);
    add_damage_skill(&mut data, 3, 50, 0, 0);
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
    // The skill's tree is not active: the server refuses it as an inactive ability, while the
    // quiet precheck passes. The bot must see the refusal and back the skill off.
    let bot = bot(
        &server,
        2,
        "Refused",
        100.0,
        100.0,
        &[INACTIVE_SKILL, KILL_SKILL],
    )
    .await;
    spawn_monster(&server, 0x400, MONSTER, 2, 5_000, 102.0, 100.0);
    {
        let rotation = Rotation::parse(
            r#"{"archetype":"Test","role":"melee_dps","rules":[],
                "fillers":[{"skill":60003,"name":"Inactive"},{"skill":60001,"name":"Hit"}]}"#,
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
            state.refused_on.contains_key(&INACTIVE_SKILL),
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

//! Quest item-use objectives on the real 1.2 data: the quests the playerbots failed with
//! "skill refused" / "item missing" / "does not work".
//!
//! Every test drives the client path: it accepts the quest from its real giver (so the Supply
//! component hands out the quest item), spawns the real target unit or doodad, and sends the
//! `CSStartSkillPacket` item cast the 1.2 client sends. The server-side answer is pinned:
//!
//! * 2394 "Searching for Answers" (Detection Orb 17594, skill 16881, NPC group 200 negation):
//!   three uses on a Field of Honor Fighter (7824) count 3. Casting it at the caster itself is
//!   refused with `skill_urk_target_npc_group` - the playerbot's cast-at-self failure.
//! * 84 "A Broken Man" (Wilson's Flask doodad 1352 grants item 1843, throw skill 12471 next to
//!   the doodad): the fake use grants the flask and the throw counts 1. The bot never looted
//!   the flask ("quest item missing").
//! * 1856 "Taking Flight" (Whitewing Recovery Potion 5215, skill 13325, NPC 7278 with buff tag
//!   323): the use counts 1.
//! * 2396 "A Worthy Opponent" (Competition Blade 17470, skill 14311, NPC 7818): the first use
//!   counts 1; a second use on the same now-duelling NPC is refused with `SkillReqFail` (1.2
//!   `skill_reqs` 78, buff tag 566) - the bot contested one shared NPC.
//! * 2488 "A Mother's Tale" (Humphrey's Letter 17585, skill 18001, NPC 10636): the use counts 1.
//! * 2328 "Into the Deep" (Dormant Flame 14087, skill 17758, NPC 4227): the use is accepted and
//!   summons the flame doodad 1638 at the Mistfiend; the hunt objective then completes on the
//!   kill. The bot could not reach the Mistfiends (navmesh).
//!
//! Gated by `AAEMU_COMPACT_DB` (path to `compact.sqlite3`):
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game quest_item_use -- --ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::client_files::ClientFileManager;
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::character::Character;
use crate::systems::economy::inventory::Inv;
use crate::systems::quests::model::{kind, status};
use crate::systems::skills::state::{SkillRef, SkillRun};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::unit_state::UNIT_STATE_FORCED_BUFFS;
use crate::world::{NpcRef, PlayerRef, WorldObject};
use crate::GameServer;

use super::handlers::{start_skill, CSStartSkillPacket};

/// `main_world` is 31 x 31 cells of 1024 m; the quest targets span the whole continent.
const CELLS: i32 = 32;
const ZONE: u32 = 1;
const PLAYER_OBJ: u32 = 0x100;
const GIVER_OBJ: u32 = 0x200;
const TARGET_OBJ: u32 = 0x300;

/// The tables the casts need, loaded from the real database.
fn real_data() -> Option<GameData> {
    let path = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[], &app);
    let localization = crate::data::Localization::load(&conn, "en_us").unwrap();
    let mut data = GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    data.factions = crate::data::FactionData::load(&conn, &localization).unwrap();
    data.skills = crate::data::skills::SkillData::load(&conn).unwrap();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).unwrap();
    data.systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .unwrap();
    Some(data)
}

/// A server on the real tables with the subsystem hooks installed like `systems::start_all`.
fn real_server(mut data: GameData) -> Arc<GameServer> {
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(
            0,
            "main_world",
            CELLS,
            CELLS,
            ZONE,
        )],
        Vec::new(),
    )
    .expect("world");
    for id in UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.app_path = app;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let pool = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, pool, data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::ai::hooks::install(&server);
    server
}

struct Fx {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
}

impl Fx {
    /// Spawns a real NPC template at `(x, y)` in the test world.
    fn spawn_npc(&self, obj_id: u32, template_id: u32, x: f32, y: f32, z: f32) -> NpcRef {
        let npc = crate::systems::npcs::npc::manager::create(&self.server, obj_id, template_id)
            .expect("target npc template");
        npc.data().unit.transform.set_xyz(x, y, z);
        npc.data().unit.transform.zone_id = ZONE;
        npc.data().unit.hp = 5000;
        npc.data().unit.max_hp = 5000;
        self.server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &self.server.view_ctx());
        npc
    }
}

/// A character at `(x, y, z)` that accepts `quest` from the real giver `giver` standing next to
/// it, then the real target NPC `target` spawned at the same spot. `setup` shapes the character
/// for the quest's start requirements (race, faction, level, completed chain quests).
fn fixture(
    quest: u32,
    giver: u32,
    target: u32,
    setup: impl FnOnce(&mut Character),
    pos: (f32, f32, f32),
) -> Option<(Fx, NpcRef)> {
    let data = real_data()?;
    let server = real_server(data);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = test_character(1, "Quester", pos.0, pos.1);
    c.unit.transform.set_xyz(pos.0, pos.1, pos.2);
    c.unit.transform.zone_id = ZONE;
    c.unit.level = 30;
    c.unit.hp = 5000;
    c.unit.max_hp = 5000;
    c.unit.mp = 5000;
    c.unit.max_mp = 5000;
    c.skill_char.ignore_skill_cooldowns = true;
    setup(&mut c);
    // Reserve the object ids the fixture hands out explicitly so a summoned doodad gets a
    // fresh one instead of colliding with the player.
    server.ids.object.initialize_with(&[
        PLAYER_OBJ, GIVER_OBJ, TARGET_OBJ, 0x301, 0x302, 0x400, 0x401,
    ]);
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    client.conn.active_char = Some(player.clone());
    server.items.add_container(Inv::of(&player).bag());

    let giver_ref = crate::systems::npcs::npc::manager::create(&server, GIVER_OBJ, giver)
        .expect("giver npc template");
    giver_ref
        .data()
        .unit
        .transform
        .set_xyz(pos.0 + 1.0, pos.1, pos.2);
    giver_ref.data().unit.transform.zone_id = ZONE;
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&giver_ref, &server.view_ctx());
    let started =
        crate::systems::quests::events::add_quest_from_npc(&server, &player, quest, GIVER_OBJ);
    assert_eq!(started, Some(true), "quest {quest} must start");
    server.systems.quests.run_queued_evaluations(&server);
    assert_eq!(
        quest_step(&player, quest),
        kind::PROGRESS,
        "quest {quest} must reach Progress"
    );

    let mut fx = Fx {
        server,
        client,
        player,
    };
    let npc = fx.spawn_npc(TARGET_OBJ, target, pos.0, pos.1 + 1.0, pos.2);
    fx.client.drain();
    Some((fx, npc))
}

fn quest_step(player: &PlayerRef, quest: u32) -> u8 {
    player
        .data()
        .quests
        .active
        .iter()
        .find(|q| q.template_id == quest)
        .map_or(kind::INVALID, |q| q.step)
}

fn objective_count(player: &PlayerRef, quest: u32, index: usize) -> i32 {
    player
        .data()
        .quests
        .active
        .iter()
        .find(|q| q.template_id == quest)
        .expect("active quest")
        .objectives[index]
}

fn quest_status(player: &PlayerRef, quest: u32) -> u8 {
    player
        .data()
        .quests
        .active
        .iter()
        .find(|q| q.template_id == quest)
        .expect("active quest")
        .status
}

/// Captures the formatted log lines of the current thread (fields included), like the quest
/// item-starter tests: a refused cast logs its `skill_urk_*` reason at WARN.
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn warnings(f: impl FnOnce()) -> Vec<String> {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    text.lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The bag item id of a quest-material stack (`items_by_template` returns ids and the count).
fn bag_item(fx: &Fx, template_id: u32) -> Option<u64> {
    let bag = Inv::of(&fx.player).bag();
    let bag = bag.lock().unwrap();
    let (ids, count) = bag.items_by_template(template_id, -1);
    (count > 0).then(|| ids[0])
}

/// Applies a real buff template to a unit (the OnSpawn `np_skills` buffs of the quest targets).
fn apply_buff(fx: &Fx, target: &NpcRef, buff_id: u32) {
    let template = fx
        .server
        .game_data
        .systems
        .skills
        .buff(buff_id)
        .unwrap_or_else(|| panic!("buff {buff_id} template"))
        .clone();
    let obj = WorldObject::from(target);
    super::buffs::apply_template(
        &fx.server,
        &template,
        Some(&obj),
        &SkillCaster::Unit(target.obj_id),
        &obj,
        None,
    );
    crate::systems::dispatch_buff_events(&fx.server, &obj);
    assert!(
        obj.with_base_unit(|b| b.buffs().check_buff(buff_id)),
        "buff {buff_id} must be on NPC {}",
        target.obj_id
    );
}

impl Fx {
    /// The client's item cast (`CSStartSkillPacket`, `SkillCaster::Item`) at `target_obj`.
    fn cast_item(&mut self, skill_id: u32, item_id: u64, item_template_id: u32, target_obj: u32) {
        self.player.data().unit.skill_last_used_ms = 0;
        self.player.data().unit.global_cooldown_ms = 0;
        start_skill(
            &self.server,
            &mut self.client.conn,
            CSStartSkillPacket {
                skill_id,
                caster: SkillCaster::Item {
                    obj_id: self.player.obj_id,
                    item_id,
                    item_template_id,
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(target_obj),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    }

    /// The plain unit cast the 1.2 client sends for a doodad use (FakeUse / gather skills).
    fn cast_doodad(&mut self, skill_id: u32, doodad_obj: u32) {
        self.player.data().unit.skill_last_used_ms = 0;
        self.player.data().unit.global_cooldown_ms = 0;
        start_skill(
            &self.server,
            &mut self.client.conn,
            CSStartSkillPacket {
                skill_id,
                caster: SkillCaster::Unit(self.player.obj_id),
                target: SkillCastTarget::Doodad(doodad_obj),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    }

    fn settle(&self) {
        self.server
            .systems
            .quests
            .run_queued_evaluations(&self.server);
    }
}

/// The real skill template as a fresh run (for the direct `use_skill` refusals).
fn skill_run(server: &GameServer, skill_id: u32) -> SkillRef {
    SkillRun::new(
        server
            .game_data
            .systems
            .skills
            .skill(skill_id)
            .unwrap_or_else(|| panic!("skill {skill_id} template"))
            .clone(),
        1,
    )
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_2394_detection_orb_counts_three_uses_and_refuses_the_caster_as_target() {
    // Start component 10287: race 4 (Elf) and quest 2392 completed. Supply component 10288
    // hands out the orb (item 17594, one use per cast).
    let Some((mut fx, npc)) = fixture(
        2394,
        511,
        7824,
        |c| {
            c.race = 4;
            c.unit.faction_id = 102;
            crate::systems::quests::engine::set_completed_quest_flag(
                &mut c.quests.completed,
                2392,
                true,
            );
        },
        (10_000.0, 10_000.0, 200.0),
    ) else {
        return;
    };
    let orb = bag_item(&fx, 17594).expect("the Supply component must hand out the Detection Orb");

    // The NPC puts buff 3004 "Suspect" on itself when it spawns (`np_skills` 17426, OnSpawn);
    // skill 16881 requires it and its 0x22 requirement excludes the NPC group 200. The orb's
    // DispelEffect carries buff tag 573 (buff 3004) with weight 0, so every use removes the
    // Suspect buff: like the 1.2 client, the player moves to another suspect.
    apply_buff(&fx, &npc, 3004);
    let second = fx.spawn_npc(0x301, 7823, 10_000.0, 10_002.0, 200.0);
    apply_buff(&fx, &second, 3004);
    let third = fx.spawn_npc(0x302, 7824, 10_002.0, 10_000.0, 200.0);
    apply_buff(&fx, &third, 3004);

    // The playerbot cast the orb with itself as the target: refused with the group requirement.
    let me = WorldObject::from(&fx.player);
    let mut self_target = SkillCastTarget::Unit(fx.player.obj_id);
    let (result, _) = super::skill::use_skill(
        &fx.server,
        &skill_run(&fx.server, 16881),
        &me,
        SkillCaster::Item {
            obj_id: fx.player.obj_id,
            item_id: orb,
            item_template_id: 17594,
            type1: 0,
            type2: 0,
        },
        &mut self_target,
        None,
        false,
    );
    assert_eq!(
        result,
        super::unit_reqs::skill_result_for_key("skill_urk_target_npc_group"),
        "the 1.2 rule refuses a non-NPC (the caster itself) as the orb target"
    );

    for (use_no, suspect) in [&npc, &second, &third].into_iter().enumerate() {
        fx.cast_item(16881, orb, 17594, suspect.obj_id);
        // The cast fires before the end of its animation; let the effect task run.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        fx.settle();
        assert_eq!(
            objective_count(&fx.player, 2394, 0),
            use_no as i32 + 1,
            "use {} of the Detection Orb must count",
            use_no + 1
        );
    }
    assert_eq!(quest_status(&fx.player, 2394), status::READY);
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_84_flask_is_looted_from_the_doodad_and_thrown_at_its_spot() {
    // Start component 208 needs level 14. The progress step obtains item 1843 from the Wilson's
    // Flask doodad 1352 (FakeUse skill 12806 -> DoodadFuncLootItem) and then throws it (12471,
    // whose 0x14 requirement wants the same doodad within 10 m).
    let Some((mut fx, _npc)) = fixture(
        84,
        757,
        1352,
        |c| {
            c.unit.level = 14;
        },
        (12_000.0, 13_000.0, 165.0),
    ) else {
        return;
    };
    let world = fx.server.world.world(0).unwrap();
    let doodad = crate::systems::npcs::doodad::manager::create(
        &fx.server,
        &world,
        0x400,
        1352,
        Some(&WorldObject::from(&fx.player)),
        true,
    )
    .expect("doodad 1352 template");
    doodad.data().transform.set_xyz(12_000.0, 13_001.0, 165.0);
    doodad.data().transform.zone_id = ZONE;
    crate::systems::npcs::doodad::manager::spawn(&fx.server, &world, &doodad);

    // The client casts the doodad's fake use; the LootItem func grants the flask.
    fx.cast_doodad(12806, doodad.obj_id);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    fx.settle();
    let flask = bag_item(&fx, 1843).expect("the Wilson's Flask doodad must grant item 1843");

    // A looted flask doodad leaves its phase; the throw needs a flask doodad within 10 m
    // (the quest area has six of them), so stand next to a second one.
    let other = crate::systems::npcs::doodad::manager::create(
        &fx.server,
        &world,
        0x401,
        1352,
        Some(&WorldObject::from(&fx.player)),
        true,
    )
    .expect("second doodad 1352 template");
    other.data().transform.set_xyz(12_001.0, 13_001.0, 165.0);
    other.data().transform.zone_id = ZONE;
    crate::systems::npcs::doodad::manager::spawn(&fx.server, &world, &other);

    // The throw is a position cast in range of that doodad.
    let lines = warnings(|| fx.cast_item(12471, flask, 1843, fx.player.obj_id));
    // 0.5 s cast + the attack animation delay before the effects run.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    fx.settle();
    assert_eq!(
        objective_count(&fx.player, 84, 0),
        1,
        "the throw must count, refusal log: {lines:?}"
    );
    assert_eq!(quest_status(&fx.player, 84), status::READY);
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_1856_healing_potion_works_on_the_whitewing_astra() {
    // Start component 9045: level 22, mother faction 149, quest 834 completed. Supply component
    // 9046 hands out the potion (5215 x3); skill 13325 wants NPC 7278 with buff tag 323, which
    // the NPC's OnSpawn skill 13396 puts on itself (buff 1441).
    let Some((mut fx, npc)) = fixture(
        1856,
        5076,
        7278,
        |c| {
            c.race = 6;
            c.unit.faction_id = 108;
            c.unit.level = 22;
            crate::systems::quests::engine::set_completed_quest_flag(
                &mut c.quests.completed,
                834,
                true,
            );
        },
        (22_400.0, 9_970.0, 520.0),
    ) else {
        return;
    };
    let potion = bag_item(&fx, 5215).expect("the Supply component must hand out the potion");
    apply_buff(&fx, &npc, 1441);

    fx.cast_item(13325, potion, 5215, npc.obj_id);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    fx.settle();
    assert_eq!(
        objective_count(&fx.player, 1856, 0),
        1,
        "the potion use must count"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_2396_first_blade_use_counts_and_a_duel_target_is_refused() {
    // Start component 10298: race 4 (Elf), quest 2388 completed. Supply component 17418 hands
    // out the Competition Blade (17470); skill 14311 wants NPC 7818.
    let Some((mut fx, npc)) = fixture(
        2396,
        7817,
        7818,
        |c| {
            c.race = 4;
            c.unit.faction_id = 102;
            crate::systems::quests::engine::set_completed_quest_flag(
                &mut c.quests.completed,
                2388,
                true,
            );
        },
        (10_300.0, 15_400.0, 238.0),
    ) else {
        return;
    };
    let blade = bag_item(&fx, 17470).expect("the Supply component must hand out the blade");

    // A knight that already started a duel (buff 1834, tag 566) is refused by the 1.2
    // `skill_reqs` 78; the bot contested one shared NPC with the other bots.
    let duelling = fx.spawn_npc(0x301, 7818, 10_300.0, 15_402.0, 238.0);
    apply_buff(&fx, &duelling, 1834);
    let me = WorldObject::from(&fx.player);
    let mut target = SkillCastTarget::Unit(duelling.obj_id);
    let (result, _) = super::skill::use_skill(
        &fx.server,
        &skill_run(&fx.server, 14311),
        &me,
        SkillCaster::Item {
            obj_id: fx.player.obj_id,
            item_id: blade,
            item_template_id: 17470,
            type1: 0,
            type2: 0,
        },
        &mut target,
        None,
        false,
    );
    assert_eq!(
        result,
        super::types::skill_result::SKILL_REQ_FAIL,
        "a target that already started a duel is refused by the 1.2 skill requirement"
    );

    fx.cast_item(14311, blade, 17470, npc.obj_id);
    tokio::time::sleep(Duration::from_millis(3000)).await;
    fx.settle();
    assert_eq!(
        objective_count(&fx.player, 2396, 0),
        1,
        "the first blade use must count"
    );
    assert!(
        WorldObject::from(&npc).with_base_unit(|b| b.buffs().check_buff(1834)),
        "the blade must start the duel on the target"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_2488_letter_is_read_to_emma() {
    // Start component 10750: race 1 (Nuian), quest 3573 completed. Supply component 18204 hands
    // out the letter (17585); skill 18001 (target type Friendly) wants NPC 10636.
    let Some((mut fx, npc)) = fixture(
        2488,
        7595,
        10636,
        |c| {
            c.race = 1;
            c.unit.faction_id = 101;
            crate::systems::quests::engine::set_completed_quest_flag(
                &mut c.quests.completed,
                3573,
                true,
            );
        },
        (12_700.0, 16_260.0, 392.0),
    ) else {
        return;
    };
    let letter = bag_item(&fx, 17585).expect("the Supply component must hand out the letter");

    fx.cast_item(18001, letter, 17585, npc.obj_id);
    tokio::time::sleep(Duration::from_millis(3100)).await;
    fx.settle();
    assert_eq!(
        objective_count(&fx.player, 2488, 0),
        1,
        "reading the letter must count"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_2328_dormant_flame_wakes_the_mistfiend_and_the_hunt_completes() {
    // Start component 10068: race 6 (Ferre), quest 2326 completed. Supply component 10069 hands
    // out the Dormant Flame (14087); skill 17758 (target type Hostile) wants NPC 4227 and its
    // InteractionEffect summons doodad 1638 at the target.
    let Some((mut fx, npc)) = fixture(
        2328,
        4269,
        4227,
        |c| {
            c.race = 6;
            c.unit.faction_id = 113;
            crate::systems::quests::engine::set_completed_quest_flag(
                &mut c.quests.completed,
                2326,
                true,
            );
        },
        (22_930.0, 9_430.0, 556.0),
    ) else {
        return;
    };
    let flame = bag_item(&fx, 14087).expect("the Supply component must hand out the Dormant Flame");

    assert!(
        fx.server
            .game_data
            .systems
            .npcs
            .doodad
            .template(1638)
            .is_some(),
        "doodad 1638 template must load"
    );
    let doodads_before = fx.server.world.world(0).unwrap().all_doodads().len();
    let lines = warnings(|| fx.cast_item(17758, flame, 14087, npc.obj_id));
    // 1.5 s cast + the skill's 1.3 s effect delay before the interaction summons the doodad.
    tokio::time::sleep(Duration::from_millis(4000)).await;
    fx.settle();
    let buffed = WorldObject::from(&npc).with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .map(|e| e.template.id)
            .collect::<Vec<_>>()
    });
    assert!(
        fx.server
            .world
            .world(0)
            .unwrap()
            .all_doodads()
            .iter()
            .any(|d| d.data().template_id == 1638),
        "the flame must summon doodad 1638 at the Mistfiend (doodads {doodads_before} -> {}, npc buffs {buffed:?}, refusal log: {lines:?})",
        fx.server.world.world(0).unwrap().all_doodads().len()
    );
    assert_eq!(
        objective_count(&fx.player, 2328, 0),
        0,
        "the objective is the Mistfiend kill, not the flame"
    );

    // The hunt objective completes on the kill (the bot never reached the Mistfiends).
    crate::systems::quests::events::on_npc_killed(&fx.server, &fx.player, &npc);
    fx.settle();
    assert_eq!(
        objective_count(&fx.player, 2328, 0),
        1,
        "the Mistfiend kill must count"
    );
    assert_eq!(quest_status(&fx.player, 2328), status::READY);
}

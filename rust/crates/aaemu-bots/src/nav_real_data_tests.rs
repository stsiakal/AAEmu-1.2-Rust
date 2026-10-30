//! Gated tests against the real 1.2 client data and `compact.sqlite3`:
//!
//! ```text
//! AAEMU_GAME_PAK=D:\AA1.2\game_pak \
//! AAEMU_COMPACT_DB=C:\...\aaemu-game\app\Data\compact.sqlite3 \
//!   cargo test -p aaemu-bots real_nav -- --ignored --nocapture
//! ```
//!
//! A few known routes in the Nuian start area (the old bots' real-data fixture) and the
//! transfer-road graph built from the server's loaded `transfer_path.xml` roads.

use std::path::Path;
use std::sync::Arc;

use aaemu_game::data::client_files::ClientFileManager;
use aaemu_game::data::GameData;
use aaemu_game::systems::ai::geodata;
use aaemu_game::systems::npcs::heightmap::{HeightmapConfig, HeightmapWorlds};
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::GameServer;

use crate::nav::{self, GameNav, RoadGraph};

const NUIAN_START: (f32, f32, f32) = (15578.04, 15382.12, 126.48);
/// Quest 330's starter (template 3597, authored in the Nuian start area).
const QUEST_330_NPC: (f32, f32, f32) = (15562.45, 15354.32, 127.99);

fn real_server() -> Option<Arc<GameServer>> {
    let pak = std::env::var("AAEMU_GAME_PAK").ok()?;
    let compact = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let app = Path::new(&compact).parent()?.parent()?.to_path_buf();
    let files = Arc::new(ClientFileManager::from_sources(
        std::slice::from_ref(&pak),
        &app,
    ));
    let data = GameData::load(&app, &[pak], "en_us").expect("game data");
    let hm = Arc::new(HeightmapWorlds::new(
        files.clone(),
        &data.worlds,
        HeightmapConfig {
            heightmaps_enable: true,
            geo_data_mode: true,
            pre_load_terrain: false,
        },
    ));
    let mut config = aaemu_game::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.app_path = app;
    config.world.geo_data_mode = true;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let db = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, db, data));
    server.set_self();
    server.systems.npcs.heightmaps.install(hm);
    geodata::start(&server);
    Some(server)
}

fn at(p: (f32, f32, f32)) -> Vec3 {
    Vec3::new(p.0, p.1, p.2)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_GAME_PAK and AAEMU_COMPACT_DB"]
async fn real_nav_short_route_in_the_nuian_start() {
    let Some(server) = real_server() else { return };
    let nav = GameNav::of_instance(&server, 0, None).expect("main world instance");
    let from = at(NUIAN_START);
    let to = at(QUEST_330_NPC);
    let report = nav::plan_explain(&nav, from, to);
    println!(
        "Nuian start -> quest 330 starter: planner {} waypoints {:?} notes {:?}",
        report.planner(),
        report.plan.as_ref().map(|p| p.points.len()),
        report.notes
    );
    let plan = report
        .plan
        .unwrap_or_else(|| panic!("no route: {:?}", report.notes));
    assert!(!plan.points.is_empty());
    let step = nav::next_step(&nav, from, plan.points[0]);
    assert!(step.is_some(), "the first step is blocked: {plan:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_GAME_PAK and AAEMU_COMPACT_DB"]
async fn real_nav_long_route_over_three_hundred_metres() {
    let Some(server) = real_server() else { return };
    let nav = GameNav::of_instance(&server, 0, None).expect("main world instance");
    let from = at(NUIAN_START);
    // South-west along the starter valley: beyond the 250 m navmesh range, so the height grid
    // (or the transfer roads) must produce the route.
    let to = Vec3::new(15320.0, 15240.0, 126.0);
    let report = nav::plan_explain(&nav, from, to);
    println!(
        "300 m route: planner {} waypoints {:?} notes {:?}",
        report.planner(),
        report.plan.as_ref().map(|p| p.points.len()),
        report.notes
    );
    let plan = report
        .plan
        .unwrap_or_else(|| panic!("no long route: {:?}", report.notes));
    assert!(plan.points.len() >= 2);
    assert_eq!(*plan.points.last().unwrap(), to);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs AAEMU_GAME_PAK and AAEMU_COMPACT_DB"]
async fn real_transfer_roads_build_a_connected_graph() {
    let Some(server) = real_server() else { return };
    let template = server.world.world(0).expect("world 0").template.id;
    let Some(graph) = RoadGraph::from_server(&server, template) else {
        println!("no transfer roads loaded for template {template}");
        return;
    };
    println!(
        "transfer road graph: {} nodes, {} edges",
        graph.len(),
        graph.edges()
    );
    assert!(graph.len() >= 2);
    assert!(graph.edges() >= 1, "the loaded roads give no walkable edge");
    let (a, b) = graph.any_edge().expect("an edge");
    let route = graph.route(a, b).expect("an edge must route");
    assert_eq!(*route.first().unwrap(), a);
    assert_eq!(*route.last().unwrap(), b);
}

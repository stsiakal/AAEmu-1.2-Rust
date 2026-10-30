//! Navigation flow tests on `test_server`: route planning and the real `CSMoveUnitPacket`
//! movement path with a synthetic heightmap and a flat collision terrain.
//!
//! The P1 lifecycle test stays in `flow_tests.rs`; these exercise `goto` and the walker,
//! including two bots planning and walking at the same time.

use std::sync::Arc;
use std::time::Duration;

use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::opcodes::sc;
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::test_support::{
    install_flat_heightmap, install_test_terrain, spawn_player, test_character, test_server,
    TestClient,
};

use crate::life;
use crate::nav::movement::NavOutcome;

const SURFACE: f32 = 100.0;

fn nav_server() -> Arc<aaemu_game::GameServer> {
    let server = Arc::new(test_server());
    server.set_self();
    install_flat_heightmap(&server, SURFACE);
    install_test_terrain(&server, SURFACE);
    server
}

async fn bot_at(
    server: &Arc<aaemu_game::GameServer>,
    id: u32,
    name: &str,
    x: f32,
    y: f32,
) -> Arc<crate::host::BotSession> {
    let conn = SocketlessConnection::new(server, 1_000_000 + id);
    let bot = life::enter_loaded(server, conn, test_character(id, name, x, y)).expect("bot enters");
    // The fixture skips the DB stat load; a live character has an HP pool before moving.
    if let Some(player) = bot.player().await {
        let mut c = player.data();
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
        c.unit.mp = 500;
        c.unit.max_mp = 500;
    }
    bot
}

fn position(server: &aaemu_game::GameServer, character_id: u32) -> (f32, f32, f32) {
    let p = server
        .world
        .character_by_id(character_id)
        .expect("bot in the world");
    let t = p.data().unit.transform;
    (t.x, t.y, t.z)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bot_walks_fifty_metres_through_the_real_move_handler() {
    let server = nav_server();
    // A nearby player sees the movement packets the server broadcasts.
    let mut observer = TestClient::new(&server, 1, 1000);
    let observer_obj = server.ids.object.get_next_id().expect("object id");
    spawn_player(
        &server,
        &observer,
        test_character(1, "Observer", 90.0, 100.0),
        observer_obj,
    );
    observer.drain();

    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(25),
        bot.goto(&server, Vec3::new(150.0, 100.0, SURFACE), 1.0),
    )
    .await
    .expect("the walk finished in time");
    assert_eq!(outcome, NavOutcome::Arrived);
    let (x, y, _) = position(&server, 2);
    assert!(
        (x - 150.0).abs() <= 1.0 && (y - 100.0).abs() <= 1.0,
        "{x} {y}"
    );
    let seen = observer.drain_ops();
    assert!(
        seen.iter()
            .filter(|op| **op == sc::SCOneUnitMovementPacket)
            .count()
            >= 20,
        "the observer saw the bot move ({} frames)",
        seen.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_bots_plan_and_walk_at_the_same_time() {
    let server = nav_server();
    let a = bot_at(&server, 2, "WalkerA", 100.0, 100.0).await;
    let b = bot_at(&server, 3, "WalkerB", 100.0, 120.0).await;
    let (a_outcome, b_outcome) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(25),
            a.goto(&server, Vec3::new(130.0, 100.0, SURFACE), 1.0)
        ),
        tokio::time::timeout(
            Duration::from_secs(25),
            b.goto(&server, Vec3::new(130.0, 120.0, SURFACE), 1.0)
        ),
    );
    assert_eq!(a_outcome.expect("bot A in time"), NavOutcome::Arrived);
    assert_eq!(b_outcome.expect("bot B in time"), NavOutcome::Arrived);
    let (ax, ay, _) = position(&server, 2);
    let (bx, by, _) = position(&server, 3);
    assert!(
        (ax - 130.0).abs() <= 1.0 && (ay - 100.0).abs() <= 1.0,
        "{ax} {ay}"
    );
    assert!(
        (bx - 130.0).abs() <= 1.0 && (by - 120.0).abs() <= 1.0,
        "{bx} {by}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_goto_cancels_the_first() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let first = bot.start_goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = bot.start_goto(&server, Vec3::new(110.0, 100.0, SURFACE), 1.0);
    assert_eq!(second.wait().await, NavOutcome::Arrived);
    let first = first.wait().await;
    assert!(
        matches!(
            first,
            NavOutcome::Failed(crate::nav::movement::NavFailure::Interrupted(_))
        ),
        "{first:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_target_fails_with_a_reason() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    // Far outside the synthetic heightmap: no height data, no plan.
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        bot.goto(&server, Vec3::new(40_000.0, 40_000.0, SURFACE), 1.0),
    )
    .await
    .expect("the failure came back");
    assert!(
        matches!(
            outcome,
            NavOutcome::Failed(crate::nav::movement::NavFailure::Unreachable(_))
        ),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_bot_sends_no_movement_after_the_removal() {
    let server = nav_server();
    let mut observer = TestClient::new(&server, 1, 1000);
    let observer_obj = server.ids.object.get_next_id().expect("object id");
    spawn_player(
        &server,
        &observer,
        test_character(1, "Observer", 90.0, 100.0),
        observer_obj,
    );
    observer.drain();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let goto = bot.start_goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    bot.logout(&server).await;
    observer.drain();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = observer.drain();
    assert!(
        !after
            .iter()
            .any(|(op, _)| *op == sc::SCOneUnitMovementPacket),
        "the removed bot still sent {} frames",
        after.len()
    );
    let outcome = goto.wait().await;
    assert!(
        matches!(
            outcome,
            NavOutcome::Failed(crate::nav::movement::NavFailure::Interrupted(_))
        ),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_bot_ends_its_walk() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let goto = bot.start_goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Some(player) = bot.player().await {
        player.data().unit.hp = 0;
    }
    let outcome = tokio::time::timeout(Duration::from_secs(5), goto.wait())
        .await
        .expect("the walk ended after the death");
    assert!(
        matches!(
            outcome,
            NavOutcome::Failed(crate::nav::movement::NavFailure::Interrupted(
                "the bot died"
            ))
        ),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_teleported_bot_stops_its_walk() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let goto = bot.start_goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Some(player) = bot.player().await {
        aaemu_game::world::WorldObject::from(player)
            .with_transform(|t| t.set_xyz(200.0, 100.0, SURFACE));
    }
    let outcome = tokio::time::timeout(Duration::from_secs(5), goto.wait())
        .await
        .expect("the walk ended after the teleport");
    assert!(
        matches!(
            outcome,
            NavOutcome::Failed(crate::nav::movement::NavFailure::Interrupted(
                "the bot was teleported"
            ))
        ),
        "{outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_walk_that_outlasts_its_deadline_times_out() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    // 300 m take about a minute: the capped deadline ends the walk part of the way.
    bot.set_walk_deadline_cap(Duration::from_millis(900));
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        bot.goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0),
    )
    .await
    .expect("the walk ended");
    assert_eq!(
        outcome,
        NavOutcome::Failed(crate::nav::movement::NavFailure::Timeout)
    );
    let (x, _, _) = position(&server, 2);
    assert!(
        x > 101.0 && x < 390.0,
        "the bot walked part of the way: {x}"
    );
}

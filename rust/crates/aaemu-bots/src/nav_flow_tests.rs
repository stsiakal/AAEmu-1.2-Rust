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

use aaemu_game::world::PlayerRef;

use crate::life;
use crate::nav::movement::{Goto, NavFailure, NavOutcome};

const SURFACE: f32 = 100.0;

fn nav_server() -> Arc<aaemu_game::GameServer> {
    nav_server_at(SURFACE)
}

/// A flat world at `surface` metres (the test world's sea level is z = 100: a landing there is a
/// water landing, which the zone's fall check treats differently).
fn nav_server_at(surface: f32) -> Arc<aaemu_game::GameServer> {
    let server = Arc::new(test_server());
    server.set_self();
    install_flat_heightmap(&server, surface);
    install_test_terrain(&server, surface);
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
    assert_eq!(
        first,
        NavOutcome::Failed(crate::nav::movement::NavFailure::Interrupted(
            "replaced by a newer goto"
        ))
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

/// A walk parked between choosing its first step and sending it: the window the blocking step
/// task leaves open, in which a cancel, a newer goto or a logout can come in.
async fn walk_parked_mid_step(
    server: &Arc<aaemu_game::GameServer>,
    bot: &Arc<crate::host::BotSession>,
) -> (Goto, Arc<crate::host::StepPause>) {
    let pause = bot.pause_next_step();
    let goto = bot.start_goto(server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::timeout(Duration::from_secs(10), pause.reached.notified())
        .await
        .expect("the walk chose its first step");
    (goto, pause)
}

/// `BotHost::remove` cancels the navigation, logs the bot out and disconnects it. A walk that
/// is mid-step at that moment must not send its step afterwards: the packets are counted on the
/// bot's connection (the removed bot is out of the world, so an observer sees nothing either way).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_bot_sends_no_movement_after_the_removal() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let (goto, pause) = walk_parked_mid_step(&server, &bot).await;
    let sent = bot.packets_dispatched();
    bot.cancel_navigation();
    bot.logout(&server).await;
    bot.disconnect(&server).await;
    pause.resume.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), goto.wait())
        .await
        .expect("the walk ended after the removal");
    assert_eq!(
        outcome,
        NavOutcome::Failed(NavFailure::Interrupted("navigation cancelled"))
    );
    assert_eq!(
        bot.packets_dispatched(),
        sent,
        "the cancelled walk sent a movement packet"
    );
}

/// A newer goto replaces the walk while it is mid-step: the replaced walk sends nothing (the
/// second goto is unreachable, so every packet counted would be the first walk's).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_goto_mid_step_sends_nothing_for_the_replaced_walk() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let (first, pause) = walk_parked_mid_step(&server, &bot).await;
    let sent = bot.packets_dispatched();
    let second = bot.start_goto(&server, Vec3::new(40_000.0, 40_000.0, SURFACE), 1.0);
    pause.resume.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(5), first.wait())
        .await
        .expect("the replaced walk ended");
    assert_eq!(
        outcome,
        NavOutcome::Failed(NavFailure::Interrupted("replaced by a newer goto"))
    );
    let second = tokio::time::timeout(Duration::from_secs(10), second.wait())
        .await
        .expect("the second goto failed fast");
    assert!(
        matches!(second, NavOutcome::Failed(NavFailure::Unreachable(_))),
        "{second:?}"
    );
    assert_eq!(
        bot.packets_dispatched(),
        sent,
        "the replaced walk sent a movement packet"
    );
}

/// A logout (the server's own logout timer: no cancel) while the walk is mid-step, a death and
/// an instance change end it before the step is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bot_that_goes_offline_dies_or_changes_instance_mid_step_sends_nothing() {
    type Change = fn(&Arc<aaemu_game::GameServer>, &Arc<crate::host::BotSession>, &PlayerRef);
    let cases: [(&str, Change); 3] = [
        ("the bot is offline", |_, _, player| {
            player.data().is_online = false;
        }),
        ("the bot died", |_, _, player| {
            player.data().unit.hp = 0;
        }),
        ("the bot changed instance", |_, _, player| {
            player.data().unit.transform.instance_id = 7;
        }),
    ];
    for (n, (reason, change)) in cases.into_iter().enumerate() {
        let server = nav_server();
        let bot = bot_at(&server, 2 + n as u32, "Walker", 100.0, 100.0).await;
        let player = bot.player().await.expect("player");
        let (goto, pause) = walk_parked_mid_step(&server, &bot).await;
        let sent = bot.packets_dispatched();
        change(&server, &bot, &player);
        pause.resume.notify_one();
        let outcome = tokio::time::timeout(Duration::from_secs(5), goto.wait())
            .await
            .expect("the walk ended");
        assert_eq!(
            outcome,
            NavOutcome::Failed(NavFailure::Interrupted(reason)),
            "{reason}"
        );
        assert_eq!(
            bot.packets_dispatched(),
            sent,
            "{reason}: a packet was sent"
        );
    }
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

/// A bot that stands 9 m above dry ground walks off: the drop is a controlled, safe fall (below
/// the 20 m/s damage-free fall speed), not a teleport, even though that one step moves the bot
/// further than the teleport threshold; it arrives and takes no fall damage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nine_metre_safe_drop_does_not_end_the_walk() {
    const GROUND: f32 = 120.0;
    let server = nav_server_at(GROUND);
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let player = bot.player().await.expect("player");
    aaemu_game::world::WorldObject::from(player.clone())
        .with_transform(|t| t.set_xyz(100.0, 100.0, GROUND + 9.0));
    let outcome = tokio::time::timeout(
        Duration::from_secs(25),
        bot.goto(&server, Vec3::new(130.0, 100.0, GROUND), 1.0),
    )
    .await
    .expect("the walk finished in time");
    assert_eq!(outcome, NavOutcome::Arrived);
    let (x, y, z) = position(&server, 2);
    assert!(
        (x - 130.0).abs() <= 1.0 && (y - 100.0).abs() <= 1.0,
        "{x} {y}"
    );
    assert!(
        (z - GROUND).abs() < 0.5,
        "the bot stands on the ground: {z}"
    );
    assert_eq!(
        player.data().unit.hp,
        1000,
        "a 9 m drop is below the damage-free fall speed"
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

/// `BotHost::remove` cancels the navigation before it logs the bot out: the walk ends as
/// cancelled (not as "replaced by a newer goto") and no movement packet follows. The packets
/// are counted on the bot's connection once a dispatch in flight at the cancel has finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_navigation_ends_the_walk_before_the_next_packet() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let goto = bot.start_goto(&server, Vec3::new(400.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(bot.packets_dispatched() > 0, "the bot was walking");
    bot.cancel_navigation();
    let sent = bot.packets_dispatched_settled().await;
    let outcome = tokio::time::timeout(Duration::from_secs(3), goto.wait())
        .await
        .expect("the walk ended after the cancel");
    assert_eq!(
        outcome,
        NavOutcome::Failed(NavFailure::Interrupted("navigation cancelled"))
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        bot.packets_dispatched(),
        sent,
        "the cancelled bot still sent packets"
    );
}

/// A slow (a `MoveSpeedMul` / `SwimSpeedMul` percent bonus) shortens the bot's steps and the
/// speeds it reports, like a real client's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slowed_bot_walks_at_the_units_actual_speed() {
    use aaemu_game::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};

    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let player = bot.player().await.expect("player");
    let (run, swim) = crate::nav::movement::speeds(&player);
    assert!((run - crate::nav::RUN_SPEED_MPS).abs() < 0.01, "{run}");
    assert!((swim - crate::nav::SWIM_SPEED_MPS).abs() < 0.01, "{swim}");
    {
        let mut c = player.data();
        for attribute in [UnitAttribute::MoveSpeedMul, UnitAttribute::SwimSpeedMul] {
            c.unit.bonuses.add_bonus(
                900,
                Bonus {
                    template: BonusTemplate {
                        attribute,
                        modifier_type: UnitModifierType::Percent,
                        value: -50,
                        linear_level_bonus: 0,
                    },
                    value: -50,
                },
            );
        }
    }
    let (slow_run, slow_swim) = crate::nav::movement::speeds(&player);
    assert!((slow_run - run * 0.5).abs() < 0.01, "{slow_run}");
    assert!((slow_swim - swim * 0.5).abs() < 0.01, "{slow_swim}");
    // End to end: one second of walking covers about half the normal 5.4 m.
    let goto = bot.start_goto(&server, Vec3::new(300.0, 100.0, SURFACE), 1.0);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (x1, _, _) = position(&server, 2);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let (x2, _, _) = position(&server, 2);
    bot.cancel_navigation();
    let _ = goto.wait().await;
    let covered = x2 - x1;
    assert!(
        (2.0..3.6).contains(&covered),
        "a 50% slow covers about 2.7 m per second, not {covered:.2}"
    );
}

/// The movement packets' client time never goes backwards (the verifier's reason 0) and runs at
/// wall-clock rate, also across an idle stretch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_movement_clock_is_monotonic_and_runs_at_wall_clock_rate() {
    let server = nav_server();
    let bot = bot_at(&server, 2, "Walker", 100.0, 100.0).await;
    let first = bot.next_move_time_ms();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = bot.next_move_time_ms();
    assert!(
        second >= first + 240 && second <= first + 600,
        "{first} -> {second}"
    );
    assert!(bot.next_move_time_ms() >= second);
}

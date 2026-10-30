//! Unit tests for the walk state machine, the recovery ladder and the planners; the
//! server-side flow tests live in `flow_tests.rs`.

use std::sync::Arc;

use aaemu_game::systems::skills::controllers::Vec3;

use super::*;
use crate::nav::{
    self, grid_route, plan, Avoid, NavWorld, Obstacle, StepAction, GOAL_TOLERANCE_M, LADDER,
    RUNG_WINDOW_MS, STUCK_WINDOW_MS,
};

/// Flat ground at z = 100.
struct Flat;

impl NavWorld for Flat {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
}

fn at(x: f32, y: f32) -> Vec3 {
    Vec3::new(x, y, 100.0)
}

/// Drives a [`RouteWalker`] with no server: steps are applied as if the server accepted them,
/// and a `Replan` request is answered with the height-grid planner (the repath rung with the
/// avoid circles, a plain replan without them).
fn drive(
    walker: &mut RouteWalker,
    world: &Arc<dyn NavWorld + Send + Sync>,
    mut pos: Vec3,
    target: Vec3,
    ticks: usize,
) -> NavOutcome {
    let mut now = 0i64;
    for _ in 0..ticks {
        now += 100;
        match walker.next(pos, now) {
            WalkAction::Arrived => return NavOutcome::Arrived,
            WalkAction::Failed(failure) => return NavOutcome::Failed(failure),
            WalkAction::Wait => {}
            WalkAction::Step(step) => {
                // A run/swim step must be a legal client step, never a teleport.
                let d = ((step.to.x - pos.x).powi(2) + (step.to.y - pos.y).powi(2)).sqrt();
                assert!(
                    d <= 0.55 || !matches!(step.action, StepAction::Run),
                    "{step:?}"
                );
                pos = step.to;
            }
            WalkAction::Replan { avoid, .. } => {
                let plan = match avoid {
                    Some(spot) => grid_route(
                        &Avoid {
                            world: world.clone(),
                            spots: vec![spot],
                            radius: 3.0,
                        },
                        pos,
                        target,
                        2.0,
                        100.0,
                    ),
                    None => grid_route(world.as_ref(), pos, target, 2.0, 100.0),
                };
                walker.set_plan(plan.unwrap_or_else(|| vec![target]), avoid.is_some());
            }
        }
    }
    NavOutcome::Failed(NavFailure::Interrupted("test ticks ran out"))
}

#[test]
fn a_flat_route_is_followed_to_the_target() {
    let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(Flat);
    let target = at(120.0, 100.0);
    let mut walker = RouteWalker::new(world.clone(), vec![target], target, 0.5);
    let outcome = drive(&mut walker, &world, at(100.0, 100.0), target, 200);
    assert_eq!(outcome, NavOutcome::Arrived);
}

#[test]
fn a_server_refused_leg_replans_and_the_route_still_arrives() {
    let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(Flat);
    let target = at(120.0, 100.0);
    let mut walker = RouteWalker::new(world.clone(), vec![target], target, 0.5);
    let mut now = 0i64;
    let mut action = None;
    // Four blocked steps at the same spot: the first rung is the repath.
    for _ in 0..4 {
        now += 100;
        action = walker.refused(at(100.0, 100.0), now);
    }
    assert!(
        matches!(action, Some(WalkAction::Replan { avoid: Some(_), .. })),
        "{action:?}"
    );
    // The caller replans around the blocked spot; the new route goes north first.
    walker.set_plan(vec![at(100.0, 112.0), at(120.0, 112.0), target], true);
    let outcome = drive(&mut walker, &world, at(100.0, 100.0), target, 300);
    assert_eq!(outcome, NavOutcome::Arrived);
    assert_eq!(walker.rungs, vec!["recovery:repath"]);
}

/// Every step whose middle is inside the pocket is blocked: the bot starts wedged and only the
/// retrace rung (which ignores the obstacle sweeps) gets it out of the pocket.
struct Pocket;

impl NavWorld for Pocket {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        let mid = ((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
        ((mid.0 - 100.0).powi(2) + (mid.1 - 100.0).powi(2) < 16.0).then_some(Obstacle::High(3.0))
    }
}

#[test]
fn stuck_goes_through_the_recovery_ladder_to_success() {
    let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(Pocket);
    let target = at(140.0, 100.0);
    let mut walker = RouteWalker::new(world.clone(), vec![target], target, 0.5);
    // 30 s: 3 blocked steps, the failed repath, then the 5 s rung windows.
    let outcome = drive(&mut walker, &world, at(100.0, 100.0), target, 900);
    assert_eq!(outcome, NavOutcome::Arrived);
    assert!(
        walker.rungs.contains(&"recovery:repath"),
        "{:?}",
        walker.rungs
    );
    assert!(
        walker.rungs.contains(&"recovery:backtrack"),
        "{:?}",
        walker.rungs
    );
}

struct Wall(Option<(f32, f32)>);

impl NavWorld for Wall {
    fn height(&self, x: f32, _y: f32) -> Option<f32> {
        Some(100.0 + x * 0.04)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        let crosses = |range: (f32, f32)| {
            (from.x < range.0 && to.x >= range.0) || (from.x > range.1 && to.x <= range.1)
        };
        self.0.filter(|r| crosses(*r)).map(|_| Obstacle::High(3.0))
    }
}

#[test]
fn the_grid_planner_goes_around_a_wall_or_refuses() {
    let passable = Wall(None);
    let route = plan(&passable, at(100.0, 100.0), at(140.0, 100.0)).expect("open ground");
    assert_eq!(route.source, "heightmap");
    let walled = Wall(Some((120.0, 121.0)));
    assert!(
        plan(&walled, at(100.0, 100.0), at(140.0, 100.0)).is_none(),
        "a wall across the whole search box must not be crossed"
    );
}

#[test]
fn a_jumpable_fence_is_crossed_but_a_high_wall_is_not() {
    struct Fence {
        wall: bool,
    }
    impl NavWorld for Fence {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
            let crosses = (from.x < 105.0 && to.x >= 105.0) || (from.x > 106.0 && to.x <= 106.0);
            if !crosses {
                return None;
            }
            Some(if self.wall {
                Obstacle::High(3.0)
            } else {
                Obstacle::Low(1.1)
            })
        }
    }
    let fence = Fence { wall: false };
    assert!(plan(&fence, at(100.0, 100.0), at(112.0, 100.0)).is_some());
    let wall = Fence { wall: true };
    assert!(plan(&wall, at(100.0, 100.0), at(112.0, 100.0)).is_none());
}

#[test]
fn a_jumpable_obstacle_produces_a_jump_step() {
    struct Fence;
    impl NavWorld for Fence {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
            ((from.x < 105.0 && to.x >= 105.0) || (from.x > 106.0 && to.x <= 106.0))
                .then_some(Obstacle::Low(1.1))
        }
    }
    let step = nav::next_step(&Fence, at(104.5, 100.0), at(107.0, 100.0)).expect("jump step");
    assert!(
        matches!(step.action, StepAction::Jump(h) if (h - 1.1).abs() < 0.01),
        "{step:?}"
    );
}

#[test]
fn drops_are_limited_to_the_safe_fall_height() {
    struct Cliff;
    impl NavWorld for Cliff {
        fn height(&self, x: f32, _y: f32) -> Option<f32> {
            Some(if x >= 104.0 { 92.0 } else { 100.0 })
        }
    }
    let world = Cliff;
    assert!(plan(&world, at(100.0, 100.0), at(112.0, 100.0)).is_some());
    let step = nav::next_step(
        &world,
        Vec3::new(103.8, 100.0, 100.0),
        Vec3::new(105.0, 100.0, 92.0),
    )
    .expect("drop step");
    assert_eq!(step.action, StepAction::Drop(8.0));

    struct Deep;
    impl NavWorld for Deep {
        fn height(&self, x: f32, _y: f32) -> Option<f32> {
            Some(if x >= 104.0 { 80.0 } else { 100.0 })
        }
    }
    assert!(plan(&Deep, at(100.0, 100.0), at(112.0, 100.0)).is_none());
}

#[test]
fn water_is_crossed_by_swimming() {
    struct Lake;
    impl NavWorld for Lake {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn water_depth(&self, x: f32, _y: f32, _ground: f32) -> f32 {
            if (103.0..=109.0).contains(&x) {
                3.0
            } else {
                0.0
            }
        }
    }
    let world = Lake;
    assert!(plan(&world, at(100.0, 100.0), at(112.0, 100.0)).is_some());
    let step = nav::next_step(&world, at(104.0, 100.0), at(105.0, 100.0)).expect("swim step");
    assert_eq!(step.action, StepAction::Swim);
    assert!((step.to.z - 101.7).abs() < 0.01, "{step:?}");
}

#[test]
fn a_goal_inside_a_collider_is_approached() {
    struct GoalInCollider;
    impl NavWorld for GoalInCollider {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn obstacle(&self, _from: Vec3, to: Vec3) -> Option<Obstacle> {
            (((to.x - 130.0).powi(2) + (to.y - 100.0).powi(2)).sqrt() < 2.2)
                .then_some(Obstacle::High(3.0))
        }
    }
    let goal = at(130.0, 100.0);
    let route = plan(&GoalInCollider, at(100.0, 100.0), goal).expect("approach");
    let last = *route.points.last().unwrap();
    let d = ((last.x - goal.x).powi(2) + (last.y - goal.y).powi(2)).sqrt();
    assert!(d <= GOAL_TOLERANCE_M + 0.1 && d > 2.0, "{last:?}");
}

#[test]
fn a_long_route_over_a_ridge_goes_through_the_pass() {
    struct RidgeWorld {
        gap: Option<(f32, f32)>,
    }
    impl NavWorld for RidgeWorld {
        fn height(&self, x: f32, y: f32) -> Option<f32> {
            let base = 100.0 + x * 0.08;
            let in_ridge = (1500.0..=1530.0).contains(&x);
            let in_gap = self.gap.is_some_and(|(a, b)| y >= a && y <= b);
            let bump = if in_ridge && !in_gap {
                let edge = (x - 1500.0).min(1530.0 - x).clamp(0.0, 6.0);
                90.0 * edge / 6.0
            } else {
                0.0
            };
            Some(base + bump)
        }
    }
    let world = RidgeWorld {
        gap: Some((300.0, 380.0)),
    };
    let from = Vec3::new(0.0, 0.0, 100.0);
    let to = Vec3::new(3000.0, 0.0, 340.0);
    let plan = plan(&world, from, to).unwrap_or_else(|| panic!("no plan"));
    assert_eq!(plan.source, "heightmap");
    assert_eq!(*plan.points.last().unwrap(), to);
}

#[test]
fn stuck_ladder_needs_a_window_per_rung() {
    let mut tracker = nav::StuckTracker::default();
    let pos = (100.0, 100.0);
    let mut now = 0i64;
    let mut labels = Vec::new();
    for _ in 0..1_200 {
        now += 100;
        if tracker.blocked(pos, now) {
            if let Some(label) = tracker.rung(pos, now) {
                labels.push((now, label));
            }
        }
    }
    let names: Vec<&str> = labels.iter().map(|(_, l)| *l).collect();
    assert_eq!(names, LADDER);
    assert!(labels[0].0 <= 400, "{labels:?}");
    for pair in labels.windows(2) {
        assert!(pair[1].0 - pair[0].0 >= RUNG_WINDOW_MS, "{labels:?}");
    }
}

#[test]
fn a_rung_that_gets_the_bot_moving_restarts_the_ladder() {
    let mut tracker = nav::StuckTracker::default();
    let mut now = 0i64;
    let mut pos = (100.0, 100.0);
    for _ in 0..4 {
        now += 100;
        tracker.blocked(pos, now);
    }
    assert_eq!(tracker.rung(pos, now), Some("recovery:repath"));
    pos = (106.0, 100.0);
    let mut next = None;
    for _ in 0..5 {
        now += 100;
        if tracker.blocked(pos, now) {
            next = tracker.rung(pos, now);
            break;
        }
    }
    assert_eq!(next, Some("recovery:repath"));
}

#[test]
fn the_no_progress_window_also_hands_out_a_rung() {
    let mut tracker = nav::StuckTracker::default();
    assert!(!tracker.observe((100.0, 100.0), 0));
    assert!(!tracker.observe((101.0, 100.0), STUCK_WINDOW_MS - 1));
    assert!(tracker.observe((101.0, 100.0), STUCK_WINDOW_MS + 1));
}

/// A world with a transfer road along y = 0 from x = 0 to x = 1000.
struct RoadWorld;

impl NavWorld for RoadWorld {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }

    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        (((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt() <= nav::NAV_DIRECT_MAX_M)
            .then_some(vec![to])
    }

    fn road_route(&self, _from: Vec3, _to: Vec3) -> Option<Vec<Vec3>> {
        Some(vec![at(10.0, 0.0), at(500.0, 0.0), at(990.0, 0.0)])
    }
}

#[test]
fn a_far_route_uses_the_road_graph_with_nav_legs() {
    let world = RoadWorld;
    let from = at(10.0, 5.0);
    let to = at(990.0, 5.0);
    let report = nav::plan_explain(&world, from, to);
    let plan = report
        .plan
        .unwrap_or_else(|| panic!("no plan: {:?}", report.notes));
    assert_eq!(plan.source, "roads");
    assert_eq!(plan.points.first(), Some(&at(10.0, 0.0)));
    assert_eq!(plan.points.last(), Some(&to));
}

/// Two plans at once (two bots, two blocking workers) both finish and return a route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_plans_run_concurrently() {
    let a = tokio::task::spawn_blocking(|| {
        let world = Flat;
        nav::plan(&world, at(0.0, 0.0), at(600.0, 40.0)).is_some()
    });
    let b = tokio::task::spawn_blocking(|| {
        let world = Flat;
        nav::plan(&world, at(0.0, 0.0), at(0.0, 600.0)).is_some()
    });
    let (a, b) = tokio::join!(a, b);
    assert!(a.expect("plan a"), "the first plan returned no route");
    assert!(b.expect("plan b"), "the second plan returned no route");
}

#[test]
fn a_swim_step_is_as_long_as_the_units_swim_speed() {
    struct Lake;
    impl NavWorld for Lake {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn water_depth(&self, x: f32, _y: f32, _ground: f32) -> f32 {
            if x >= 101.0 {
                3.0
            } else {
                0.0
            }
        }
    }
    let from = Vec3::new(101.0, 100.0, 100.0);
    let step =
        nav::next_step_with_speeds(&Lake, from, at(120.0, 100.0), 5.4, 1.5).expect("swim step");
    assert_eq!(step.action, StepAction::Swim);
    let horizontal = ((step.to.x - from.x).powi(2) + (step.to.y - from.y).powi(2)).sqrt();
    assert!((horizontal - 0.15).abs() < 0.01, "{step:?}");
    let run = nav::next_step_with_speeds(&Lake, at(100.0, 100.0), at(120.0, 100.0), 5.4, 1.5)
        .expect("run step");
    assert!((run.to.x - 100.54).abs() < 0.01, "{run:?}");
}

struct Ledge;
impl NavWorld for Ledge {
    fn height(&self, x: f32, _y: f32) -> Option<f32> {
        Some(if x >= 105.0 { 101.0 } else { 100.0 })
    }
}

#[test]
fn a_jump_onto_a_ledge_never_samples_under_the_terrain() {
    let from = Vec3::new(104.8, 100.0, 100.0);
    let step =
        nav::next_step_with_speeds(&Ledge, from, at(107.0, 100.0), 5.4, 3.0).expect("jump step");
    assert!(matches!(step.action, StepAction::Jump(_)), "{step:?}");
    assert!(
        (step.to.z - 101.0).abs() < 0.01,
        "lands on the ledge: {step:?}"
    );
    for i in 0..=64 {
        let t = i as f32 / 64.0 * nav::JUMP_AIR_TIME_S;
        let p = nav::jump_arc(from, step.to, t);
        let ground = Ledge.height(p.x, p.y).unwrap();
        assert!(
            p.z >= ground - 0.1,
            "sample {t:.3}s at ({:.2}, {:.2}) z={:.2} is under ground {ground:.2}",
            p.x,
            p.y,
            p.z
        );
    }
}

#[test]
fn a_jump_toward_a_cliff_edge_does_not_land_in_the_air() {
    struct Deep;
    impl NavWorld for Deep {
        fn height(&self, x: f32, _y: f32) -> Option<f32> {
            Some(if x >= 105.0 { 80.0 } else { 100.0 })
        }
    }
    assert!(
        nav::next_step_with_speeds(
            &Deep,
            Vec3::new(104.8, 100.0, 100.0),
            at(107.0, 100.0),
            5.4,
            3.0
        )
        .is_none(),
        "a 20 m drop must not become a jump"
    );
    struct Shallow;
    impl NavWorld for Shallow {
        fn height(&self, x: f32, _y: f32) -> Option<f32> {
            Some(if x >= 105.0 { 97.0 } else { 100.0 })
        }
    }
    let step = nav::next_step_with_speeds(
        &Shallow,
        Vec3::new(104.8, 100.0, 100.0),
        at(107.0, 100.0),
        5.4,
        3.0,
    )
    .expect("safe drop step");
    assert!(
        matches!(step.action, StepAction::Drop(h) if (h - 3.0).abs() < 0.01),
        "{step:?}"
    );
    assert!((step.to.z - 97.0).abs() < 0.01, "lands on the lower ground");
}

#[test]
fn a_jump_blocked_by_a_wall_is_rejected_by_the_arc_sweep() {
    struct Fence;
    impl NavWorld for Fence {
        fn height(&self, _x: f32, _y: f32) -> Option<f32> {
            Some(100.0)
        }
        fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
            ((from.x < 105.0 && to.x >= 105.0) || (from.x > 106.0 && to.x <= 106.0))
                .then_some(Obstacle::Low(1.1))
        }
        fn jump_clear(&self, _from: Vec3, _to: Vec3) -> Option<bool> {
            Some(false)
        }
    }
    assert!(
        nav::next_step(&Fence, at(104.5, 100.0), at(107.0, 100.0)).is_none(),
        "a blocked arc must not produce a jump"
    );
}

#[test]
fn drop_resets_keep_the_total_and_hit_the_rung_cap() {
    let mut tracker = nav::StuckTracker::default();
    let mut now = 0i64;
    let pos = (100.0, 100.0);
    let mut labels = Vec::new();
    for _ in 0..200 {
        for _ in 0..4 {
            now += 100;
            if tracker.blocked(pos, now) {
                if let Some(label) = tracker.rung(pos, now) {
                    labels.push(label);
                }
            }
        }
        tracker.reset();
    }
    let abandon = labels
        .iter()
        .position(|label| *label == "recovery:abandon")
        .unwrap_or_else(|| panic!("the total rung cap must survive drop resets: {labels:?}"));
    assert!(
        abandon <= nav::MAX_TOTAL_RUNGS as usize,
        "abandon came after {abandon} rungs: {labels:?}"
    );
}

#[test]
fn a_long_walk_gets_a_route_scaled_deadline() {
    let now = tokio::time::Instant::now();
    let short = super::walk_deadline(&[], at(0.0, 0.0), 5.4);
    assert!(short > now && short <= now + std::time::Duration::from_secs(31));
    let long = super::walk_deadline(&[at(0.0, 0.0), at(100_000.0, 0.0)], at(0.0, 0.0), 5.4);
    assert!(long <= now + std::time::Duration::from_secs(1_801));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_plans_respect_the_plan_gate() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let gate = Arc::new(nav::PlanGate::new(4));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let gate = gate.clone();
        let active = active.clone();
        let peak = peak.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = gate.acquire().await;
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for task in tasks {
        task.await.expect("a plan task panicked");
    }
    assert!(
        peak.load(Ordering::SeqCst) <= 4,
        "the gate admitted {} plans",
        peak.load(Ordering::SeqCst)
    );
}

struct BlockingWorld {
    started: Arc<std::sync::atomic::AtomicBool>,
    release: Arc<std::sync::atomic::AtomicBool>,
}
impl NavWorld for BlockingWorld {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        use std::sync::atomic::Ordering;
        self.started.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        Some(100.0)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_plan_stops_within_the_budget() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let flag = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(BlockingWorld {
        started: started.clone(),
        release: release.clone(),
    });
    let handle = tokio::spawn(nav::plan_guarded(
        world,
        at(0.0, 0.0),
        at(600.0, 40.0),
        Arc::new(nav::PlanGate::new(1)),
        nav::PlanCancel::from_flag(flag.clone()),
    ));
    while !started.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    flag.store(true, Ordering::SeqCst);
    release.store(true, Ordering::SeqCst);
    let report = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("the cancelled plan stopped in time")
        .expect("the plan task panicked")
        .expect("the plan gate returned no report");
    assert!(report.plan.is_none(), "{report:?}");
}

struct BlockedRoadWorld;
impl NavWorld for BlockedRoadWorld {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        (((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt() <= nav::NAV_DIRECT_MAX_M)
            .then_some(vec![to])
    }
    fn road_route(&self, _from: Vec3, _to: Vec3) -> Option<Vec<Vec3>> {
        Some(vec![at(10.0, 0.0), at(500.0, 0.0), at(990.0, 0.0)])
    }
    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        ((from.x < 500.0 && to.x >= 500.0) || (from.x > 500.0 && to.x <= 500.0))
            .then_some(Obstacle::High(3.0))
    }
}

#[test]
fn a_road_route_through_a_wall_is_rejected() {
    let report = nav::plan_explain(&BlockedRoadWorld, at(10.0, 5.0), at(990.0, 5.0));
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("roads: segment rejected")),
        "{:?}",
        report.notes
    );
    assert_ne!(report.planner(), "roads");
}

#[test]
fn a_road_further_than_the_access_limit_is_not_used() {
    let graph = nav::RoadGraph::from_roads([vec![at(0.0, 0.0), at(100.0, 0.0)]]);
    assert!(graph.route(at(0.0, 200.0), at(100.0, 0.0)).is_none());
}

/// Two roads between the same ends: the straight one (y = 0) crosses a wall at x = 500, the
/// longer one runs along y = 50 and is clean.
struct TwoRoads {
    graph: nav::RoadGraph,
}

impl TwoRoads {
    fn new() -> Self {
        let line = |from: (f32, f32), to: (f32, f32)| -> Vec<Vec3> {
            let n = (((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt() / 50.0).ceil();
            (0..=n as usize)
                .map(|i| {
                    let t = i as f32 / n;
                    at(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
                })
                .collect()
        };
        let straight = line((10.0, 0.0), (990.0, 0.0));
        let mut detour = line((10.0, 0.0), (10.0, 50.0));
        detour.extend(line((10.0, 50.0), (990.0, 50.0)).into_iter().skip(1));
        detour.extend(line((990.0, 50.0), (990.0, 0.0)).into_iter().skip(1));
        Self {
            graph: nav::RoadGraph::from_roads([straight, detour]),
        }
    }
}

impl NavWorld for TwoRoads {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.graph.route(from, to)
    }
    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut nav::PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.graph.route_avoiding(from, to, banned, budget)
    }
    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        let crosses = (from.x < 500.0) != (to.x < 500.0);
        (crosses && from.y.abs() < 25.0 && to.y.abs() < 25.0).then_some(Obstacle::High(3.0))
    }
}

#[test]
fn a_rejected_road_edge_is_dropped_and_the_clean_road_is_used() {
    let world = TwoRoads::new();
    let report = nav::plan_explain(&world, at(10.0, 5.0), at(990.0, 5.0));
    let plan = report
        .plan
        .unwrap_or_else(|| panic!("no plan: {:?}", report.notes));
    assert_eq!(plan.source, "roads", "{:?}", report.notes);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("roads: segment rejected")),
        "the walled edge was never tried: {:?}",
        report.notes
    );
    // Nothing on the plan crosses the wall: the walk went by the y = 50 road.
    assert!(
        plan.points.iter().any(|p| (p.y - 50.0).abs() < 1.0),
        "{:?}",
        plan.points
    );
}

#[test]
fn the_plan_budget_reads_the_clock_after_a_large_charge() {
    // A grid prefill charges hundreds of thousands of cells in one call: a count that is not
    // a multiple of the clock batch must still read the clock.
    let mut budget = nav::PlanBudget::with_limits(usize::MAX, 1);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(!budget.charge(1_000), "the elapsed deadline was not seen");
    assert!(budget.stopped());
    let mut budget = nav::PlanBudget::with_limits(usize::MAX, 60_000);
    for _ in 0..200 {
        assert!(budget.charge(37));
    }
}

/// Bilinear heightmap at 100 m, physics terrain 0.2 m above it (a twisted cell).
struct Twisted;
impl NavWorld for Twisted {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
    fn terrain_height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.2)
    }
}

#[test]
fn a_step_ends_on_the_physics_terrain_not_the_bilinear_height() {
    let step = nav::next_step(&Twisted, at(0.0, 0.0), at(10.0, 0.0)).expect("a run step");
    assert_eq!(step.action, StepAction::Run);
    assert!(
        (step.to.z - 100.2).abs() < 1e-4,
        "the step must stand on the terrain the verifier compares to: {step:?}"
    );
}

/// A flat world with a wall at x = 300: a goal behind it is unreachable, so the fine grid floods
/// the near side until the plan budget stops it. `active` counts the plans running at once
/// (first query -> drop).
struct WalledWorld {
    started: std::sync::atomic::AtomicBool,
    active: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl WalledWorld {
    fn note_start(&self) {
        use std::sync::atomic::Ordering;
        if !self.started.swap(true, Ordering::SeqCst) {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
        }
    }
}

impl Drop for WalledWorld {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.started.load(Ordering::SeqCst) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl NavWorld for WalledWorld {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        self.note_start();
        Some(100.0)
    }
    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        ((from.x < 300.0) != (to.x < 300.0)).then_some(Obstacle::High(3.0))
    }
}

/// 50 real plans at once: the gate admits four, every unreachable plan ends at its work budget,
/// and the whole batch finishes within a few budgets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_real_plans_respect_the_gate_and_end_at_their_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let gate = Arc::new(nav::PlanGate::new(4));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(WalledWorld {
            started: std::sync::atomic::AtomicBool::new(false),
            active: active.clone(),
            peak: peak.clone(),
        });
        tasks.push(tokio::spawn(nav::plan_guarded_with_limits(
            world,
            at(250.0, 0.0),
            at(400.0, 0.0),
            gate.clone(),
            nav::PlanCancel::never(),
            20_000,
            2_000,
        )));
    }
    for task in tasks {
        let report = task
            .await
            .expect("a plan task panicked")
            .expect("the plan was not cancelled");
        assert!(report.plan.is_none(), "{report:?}");
        assert!(
            report.failure().is_some_and(|why| why.contains("budget")),
            "an unreachable plan must end at its budget: {:?}",
            report.failure()
        );
    }
    assert!(
        peak.load(Ordering::SeqCst) <= 4,
        "the gate admitted {} plans",
        peak.load(Ordering::SeqCst)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "50 budgeted plans took {:?}",
        started.elapsed()
    );
    assert_eq!(gate.available(), 4, "every permit came back");
}

/// A fence ahead of every step; the arc sweep calls a jump clear when it is at most `max_clear`
/// metres long (a long running jump is still rising where it passes the fence, a short one
/// has cleared it).
struct FenceAhead {
    max_clear: f32,
}

impl NavWorld for FenceAhead {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
    fn obstacle(&self, _from: Vec3, _to: Vec3) -> Option<Obstacle> {
        Some(Obstacle::Low(0.9))
    }
    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        Some(((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt() <= self.max_clear)
    }
}

#[test]
fn a_blocked_running_jump_falls_back_to_a_shorter_one() {
    let from = Vec3::new(100.0, 100.0, 100.0);
    let step = nav::next_step(&FenceAhead { max_clear: 2.0 }, from, at(110.0, 100.0))
        .expect("a shorter jump clears");
    assert!(matches!(step.action, StepAction::Jump(_)), "{step:?}");
    let len = step.to.x - from.x;
    assert!(
        (len - nav::JUMP_DISTANCE_M * 0.45).abs() < 0.05,
        "the longest clear candidate is 45% of the running jump: {len:.2} m"
    );
}

#[test]
fn a_clear_obstacle_gets_the_running_jump_and_a_blocked_one_none() {
    let from = Vec3::new(100.0, 100.0, 100.0);
    let step = nav::next_step(&FenceAhead { max_clear: 50.0 }, from, at(110.0, 100.0))
        .expect("the running jump clears");
    assert!(
        (step.to.x - from.x - nav::JUMP_DISTANCE_M).abs() < 0.05,
        "{step:?}"
    );
    assert!(
        nav::next_step(&FenceAhead { max_clear: 0.5 }, from, at(110.0, 100.0)).is_none(),
        "no jump length clears: the bot stays blocked (and the ladder takes over)"
    );
}

/// A low fence at x = 105 and ground that rises by `rise` metres behind it (the landing of
/// every jump over the fence is `rise` above the take-off).
struct FenceThenLedge {
    rise: f32,
}

impl NavWorld for FenceThenLedge {
    fn height(&self, x: f32, _y: f32) -> Option<f32> {
        Some(if x >= 105.0 { 100.0 + self.rise } else { 100.0 })
    }
    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        (from.x < 105.0 && to.x >= 105.0).then_some(Obstacle::Low(0.9))
    }
}

#[test]
fn a_jump_over_a_fence_never_lands_above_the_jump_height() {
    let from = Vec3::new(104.5, 100.0, 100.0);
    // A ledge within a jump's height behind the fence is landed on ...
    let step = nav::next_step(&FenceThenLedge { rise: 1.2 }, from, at(110.0, 100.0))
        .expect("a ledge within the jump height is jumped onto");
    assert!(matches!(step.action, StepAction::Jump(_)), "{step:?}");
    assert!((step.to.z - 101.2).abs() < 0.01, "{step:?}");
    // ... one above it is not, whatever the jump length: the old check only looked down.
    for rise in [1.4, 3.0, 5.0] {
        assert!(
            nav::next_step(&FenceThenLedge { rise }, from, at(110.0, 100.0)).is_none(),
            "a {rise} m higher landing is beyond a jump of {} m",
            nav::JUMP_HEIGHT_M
        );
    }
}

#[test]
fn no_jump_path_lands_above_the_jump_height() {
    // The direct path (the next step rises over the step height): a 2 m rise is no jump.
    struct Wall;
    impl NavWorld for Wall {
        fn height(&self, x: f32, _y: f32) -> Option<f32> {
            Some(if x >= 105.0 { 102.0 } else { 100.0 })
        }
    }
    assert!(
        nav::next_step(&Wall, Vec3::new(104.8, 100.0, 100.0), at(110.0, 100.0)).is_none(),
        "a 2 m rise ahead is not jumped"
    );
    // The steered path tries other headings (a plain run step along one is fine) but never
    // jumps onto the high ledge either.
    let mut bias = 1i8;
    let steered = nav::next_step_steered(
        &FenceThenLedge { rise: 4.0 },
        Vec3::new(104.5, 100.0, 100.0),
        at(110.0, 100.0),
        &mut bias,
        5.4,
        3.0,
    );
    assert!(
        !matches!(
            steered,
            Some(nav::MoveStep {
                action: StepAction::Jump(_),
                ..
            })
        ),
        "{steered:?}"
    );
}

#[test]
fn a_jump_reports_its_real_horizontal_speed() {
    // A 1.57 m jump takes the full air time: its per-tick displacement is a tenth of the
    // flight's, not the whole 1.57 m (the packet velocity is the real, slow, horizontal speed).
    let state = super::JumpState {
        from: Vec3::new(0.0, 0.0, 0.0),
        to: Vec3::new(1.57, 0.0, 0.0),
        elapsed: 0.0,
        fall_vel: 0,
    };
    let (dx, dy) = state.step_delta();
    let speed = (dx * dx + dy * dy).sqrt() / (nav::STEP_MS as f32 / 1000.0);
    assert!(
        (speed - 1.57 / nav::JUMP_AIR_TIME_S).abs() < 0.01,
        "{speed}"
    );
}

/// A wall that counts the sweeps asked about it.
struct CountingWall {
    queries: std::sync::atomic::AtomicUsize,
}

impl NavWorld for CountingWall {
    fn height(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(100.0)
    }
    fn obstacle(&self, _from: Vec3, _to: Vec3) -> Option<Obstacle> {
        self.queries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Obstacle::High(3.0))
    }
}

#[test]
fn a_blocked_walker_does_not_repeat_its_probes_every_tick() {
    use std::sync::atomic::Ordering;

    let wall = Arc::new(CountingWall {
        queries: std::sync::atomic::AtomicUsize::new(0),
    });
    let world: Arc<dyn NavWorld + Send + Sync> = wall.clone();
    let mut walker = RouteWalker::new(world, vec![at(110.0, 0.0)], at(110.0, 0.0), 1.0);
    let pos = at(100.0, 0.0);
    assert_eq!(walker.next(pos, 100), WalkAction::Wait);
    let first = wall.queries.load(Ordering::SeqCst);
    assert!(first > 0, "the first blocked tick probes");
    assert_eq!(walker.next(pos, 200), WalkAction::Wait);
    assert_eq!(
        wall.queries.load(Ordering::SeqCst),
        first,
        "the second tick at the same spot reuses the refusal"
    );
    // The stuck tracker still counts every tick: the fourth blocked tick hands out the first
    // rung (the repath), cached refusal or not.
    assert_eq!(walker.next(pos, 300), WalkAction::Wait);
    assert!(
        matches!(walker.next(pos, 400), WalkAction::Replan { .. }),
        "the ladder is not delayed by the cache"
    );
    // After the cache expires (and a rung moved on) the probes run again.
    let before = wall.queries.load(Ordering::SeqCst);
    let _ = walker.next(pos, 1_000);
    assert!(wall.queries.load(Ordering::SeqCst) > before);
}

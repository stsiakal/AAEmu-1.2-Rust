//! Unit tests for the walk state machine, the recovery ladder and the planners; the
//! server-side flow tests live in `flow_tests.rs`.

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
    walker: &mut RouteWalker<'_>,
    world: &dyn NavWorld,
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
                            world,
                            spots: vec![spot],
                            radius: 3.0,
                        },
                        pos,
                        target,
                        2.0,
                        100.0,
                    ),
                    None => grid_route(world, pos, target, 2.0, 100.0),
                };
                walker.set_plan(plan.unwrap_or_else(|| vec![target]), avoid.is_some());
            }
        }
    }
    NavOutcome::Failed(NavFailure::Interrupted("test ticks ran out"))
}

#[test]
fn a_flat_route_is_followed_to_the_target() {
    let world = Flat;
    let target = at(120.0, 100.0);
    let mut walker = RouteWalker::new(&world, vec![target], target, 0.5);
    let outcome = drive(&mut walker, &world, at(100.0, 100.0), target, 200);
    assert_eq!(outcome, NavOutcome::Arrived);
}

#[test]
fn a_server_refused_leg_replans_and_the_route_still_arrives() {
    let world = Flat;
    let target = at(120.0, 100.0);
    let mut walker = RouteWalker::new(&world, vec![target], target, 0.5);
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
    let world = Pocket;
    let target = at(140.0, 100.0);
    let mut walker = RouteWalker::new(&world, vec![target], target, 0.5);
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

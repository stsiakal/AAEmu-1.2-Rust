//! Player-like navigation for the bots: terrain / navmesh planning, jumping, ledge drops,
//! swimming and the stuck-recovery ladder.
//!
//! Numbers (cited):
//! * run 5.4 m/s, capsule radius 0.35 m / height 1.8 m / step 0.5 m and a 50° climb limit:
//!   `D:/aaemu-client/AABevy1.2/docs/aa-client-movement.md` ("Speeds and jumping" / "Collision";
//!   the same row the testclient walks). The old bots used the same values in
//!   `aaemu-game/src/systems/bots/nav.rs`.
//! * jump height 1.3 m with gravity 25 => `v = sqrt(2gh) = 8.06 m/s`, air time 0.645 s and
//!   `5.4 * 0.645 = 3.48 m` horizontal (`systems/skills/controllers.rs`' `game_jump_height` /
//!   `physics_living_gravity` values).
//! * swimming starts at 1.3 m of water, swim speed 3.0 m/s.
//! * a fall is damage-free below `pl_fallDamage_SpeedSafe` 20 m/s, i.e. `h < v²/(2g) = 10 m`
//!   with the zone's `g = 20` (`systems::ai::collision::fall`); the bot keeps a 0.5 m margin.
//!
//! The planner is the port of the old `aaemu-game/src/systems/bots/nav.rs`: navmesh for short
//! routes, a coarse height grid A* and navmesh legs for long ones, and the transfer-road graph
//! for distances beyond the `.bai` range. Every search runs on a
//! [`PlanBudget`] (bounded node expansions and wall clock), because planning happens in
//! `spawn_blocking` and must not monopolise a worker.

pub mod danger;
pub mod movement;
pub mod roads;

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use aaemu_game::server::GameServer;
use aaemu_game::systems::ai::nav::NavView;
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::world::{PlayerRef, WorldObject};

pub use self::roads::{RoadGraph, RoadGraphCache};

pub const RUN_SPEED_MPS: f32 = 5.4;
pub const STEP_MS: u64 = 100;
pub const SWIM_SPEED_MPS: f32 = 3.0;
pub const CAPSULE_RADIUS: f32 = 0.35;
pub const CAPSULE_HEIGHT: f32 = 1.8;
pub const STEP_HEIGHT_M: f32 = 0.5;
pub const MAX_CLIMB_DEG: f32 = 50.0;
pub const JUMP_HEIGHT_M: f32 = 1.3;
pub const JUMP_GRAVITY: f32 = 25.0;
pub const JUMP_SPEED_MPS: f32 = 8.06;
pub const JUMP_AIR_TIME_S: f32 = 2.0 * JUMP_SPEED_MPS / JUMP_GRAVITY;
pub const JUMP_DISTANCE_M: f32 = RUN_SPEED_MPS * JUMP_AIR_TIME_S;
/// Damage-free drop: `sqrt(2*20*h) < 20` => `h < 10 m` (keep a margin).
pub const MAX_SAFE_DROP_M: f32 = 9.5;
pub const SWIM_DEPTH_M: f32 = 1.3;
/// One navmesh search is bounded by the server A*; the testclient used 450 m, the old bots
/// 250 m. Long routes go through the coarse grid or the transfer roads.
pub const NAV_DIRECT_MAX_M: f32 = 250.0;
pub const NAV_LEG_M: f32 = 250.0;
pub const STUCK_WINDOW_MS: i64 = 30_000;
pub const STUCK_PROGRESS_M: f32 = 5.0;
pub const BLOCKED_STEPS: u32 = 3;
/// Every recovery rung gets this long to move the bot before the next rung is tried.
pub const RUNG_WINDOW_MS: i64 = 5_000;
/// Progress (metres from where the rung was applied) that counts as the rung having worked.
pub const RUNG_PROGRESS_M: f32 = 2.0;
/// A goal the search cannot enter is approached to within this many metres (less than an
/// interaction reach, more than a wall away).
pub const GOAL_TOLERANCE_M: f32 = 3.5;
/// How far (in grid cells) a start / goal that is off the height data is snapped.
const SNAP_CELLS: i32 = 8;
pub const STOP_DISTANCE: f32 = 0.35;

/// Work units one plan may spend (the height grid prefill, A* expansions and collision queries
/// all count). The height grid A* is the expensive planner.
pub const PLAN_MAX_NODES: usize = 1_000_000;
/// Wall clock one plan may spend, in milliseconds.
pub const PLAN_MAX_MS: u64 = 5_000;
/// Cells one height grid may allocate (`heights`, water cache, g / parent / closed): about 7 MB
/// per plan at 400k cells, so four concurrent plans stay small.
pub const MAX_GRID_CELLS: usize = 400_000;
/// Work units between two reads of the plan clock (and yields of the planner thread).
const CLOCK_BATCH: usize = 64;
/// Times the road planner drops a rejected road edge and asks the road A* again.
const MAX_ROAD_RETRIES: usize = 3;
/// Concurrent plans allowed by default (configurable through `Bots.PlanConcurrency`).
pub const DEFAULT_PLAN_CONCURRENCY: usize = 4;

/// Cancels the plan of a walk that a newer `goto` replaced (or a bot that left the world).
#[derive(Debug, Clone, Default)]
pub struct PlanCancel {
    flag: Option<Arc<AtomicBool>>,
    generation: Option<(Arc<AtomicU64>, u64)>,
}

impl PlanCancel {
    /// A token that never cancels.
    pub fn never() -> Self {
        Self::default()
    }

    /// A token from a plain flag (tests).
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self {
            flag: Some(flag),
            generation: None,
        }
    }

    /// A token that is cancelled once `counter` no longer equals `expected`.
    pub fn generation(counter: Arc<AtomicU64>, expected: u64) -> Self {
        Self {
            flag: None,
            generation: Some((counter, expected)),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        if self
            .flag
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
        {
            return true;
        }
        self.generation
            .as_ref()
            .is_some_and(|(counter, expected)| counter.load(Ordering::Relaxed) != *expected)
    }

    /// Waits (bounded polling) until the token is cancelled.
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

/// The work limit of one plan: a bounded work count, a deadline and a cancellation token.
/// Every A* and every collision query of [`plan_explain`] shares one budget, so a route with
/// several attempted planners cannot stall a blocking worker indefinitely.
#[derive(Debug)]
pub struct PlanBudget {
    max_nodes: usize,
    expanded: usize,
    /// Work units since the clock was last read (the clock is read every [`CLOCK_BATCH`]).
    since_clock: usize,
    deadline: std::time::Instant,
    max_ms: u64,
    stopped: bool,
    cancel: PlanCancel,
}

impl PlanBudget {
    pub fn new() -> Self {
        Self::with_limits(PLAN_MAX_NODES, PLAN_MAX_MS)
    }

    pub fn with_limits(max_nodes: usize, max_ms: u64) -> Self {
        Self {
            max_nodes,
            expanded: 0,
            since_clock: 0,
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(max_ms),
            max_ms,
            stopped: false,
            cancel: PlanCancel::never(),
        }
    }

    /// The same budget with a cancellation token.
    pub fn with_cancel(mut self, cancel: PlanCancel) -> Self {
        self.cancel = cancel;
        self
    }

    /// Counts one unit of work (an expanded node, a grid cell, a collision query); `false`
    /// when the plan must stop.
    pub fn tick(&mut self) -> bool {
        self.charge(1)
    }

    /// Counts `units` units of work at once (the height grid prefill); `false` when the plan
    /// must stop.
    pub fn charge(&mut self, units: usize) -> bool {
        if self.stopped {
            return false;
        }
        if self.cancel.is_cancelled() {
            self.stopped = true;
            return false;
        }
        self.expanded = self.expanded.saturating_add(units);
        if self.expanded > self.max_nodes {
            self.stopped = true;
            return false;
        }
        // The clock is read once per `CLOCK_BATCH` units of work however the units arrive (a
        // single `charge(n)` of a whole grid counts as many batches): the node cap above is
        // exact and the wall-clock limit is hard to within one batch of queries. The yield
        // between batches releases the collision lock to the players' and NPCs' own queries.
        self.since_clock = self.since_clock.saturating_add(units);
        if self.since_clock >= CLOCK_BATCH {
            self.since_clock = 0;
            std::thread::yield_now();
            if self.deadline.elapsed().as_millis() as u64 > self.max_ms {
                self.stopped = true;
                return false;
            }
        }
        true
    }

    /// Checks cancellation / the deadline without spending work.
    pub fn poll(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        if self.cancel.is_cancelled() || self.deadline.elapsed().as_millis() as u64 > self.max_ms {
            self.stopped = true;
            return false;
        }
        true
    }

    pub fn expanded(&self) -> usize {
        self.expanded
    }

    /// Did a search hit the work, time or cancellation limit?
    pub fn stopped(&self) -> bool {
        self.stopped
    }
}

impl Default for PlanBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// The semaphore that bounds concurrent plans across the server. A plan is a blocking worker
/// doing terrain / collision queries; without the gate a `/bot goto` burst could occupy every
/// blocking thread and starve the collision lock.
#[derive(Debug)]
pub struct PlanGate {
    sem: Arc<tokio::sync::Semaphore>,
    max: usize,
}

impl PlanGate {
    pub fn new(max: usize) -> Self {
        let max = max.max(1);
        Self {
            sem: Arc::new(tokio::sync::Semaphore::new(max)),
            max,
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("the plan gate is never closed")
    }
}

/// The server-wide gate used by bots whose host did not install its configured one (tests,
/// direct `BotSession` use).
pub fn global_plan_gate() -> Arc<PlanGate> {
    static GATE: OnceLock<Arc<PlanGate>> = OnceLock::new();
    GATE.get_or_init(|| Arc::new(PlanGate::new(DEFAULT_PLAN_CONCURRENCY)))
        .clone()
}

/// A static obstacle between two points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Obstacle {
    /// Top is within the jump height (fence, log, low rock).
    Low(f32),
    /// Too high to jump or walk.
    High(f32),
}

/// What the planner and the step executor need from the world.
pub trait NavWorld: Send + Sync {
    fn height(&self, x: f32, y: f32) -> Option<f32>;
    /// Water depth over the ground at `(x, y)` (`0` = dry).
    fn water_depth(&self, _x: f32, _y: f32, ground: f32) -> f32 {
        let _ = ground;
        0.0
    }
    /// A smoothed navmesh route (start excluded, goal last).
    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        let _ = (from, to);
        None
    }
    /// A long-range route over the server's transfer roads: the road nodes from the one
    /// nearest `from` to the one nearest `to` (both included). The first and last leg between
    /// the bot and the road are planned with the normal planners.
    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        let _ = (from, to);
        None
    }
    /// [`NavWorld::road_route`] charged against the plan budget (the road A* is bounded).
    fn road_route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        if !budget.tick() {
            return None;
        }
        self.road_route(from, to)
    }
    /// [`NavWorld::road_route_budget`] without the `banned` road edges (pairs of road node
    /// positions, either direction): the planner drops the edges its terrain check rejected
    /// and asks again. Worlds that cannot ban an edge answer the unbanned query only.
    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        if banned.is_empty() {
            self.road_route_budget(from, to, budget)
        } else {
            None
        }
    }
    /// The static obstacle crossed by the straight segment, if any.
    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        let _ = (from, to);
        None
    }
    /// Is the whole jump arc `from` -> `to` free of static geometry? `None` when the world has
    /// no collision service (the caller then accepts the jump, as the pure tests do).
    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        let _ = (from, to);
        None
    }
    /// The authored world zone at a position (`None` = outside the world data).
    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        let _ = (x, y);
        None
    }
}

/// A planned route: the waypoints after the start, the last one the target.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub points: Vec<Vec3>,
    pub source: &'static str,
}

/// A world view that refuses to cross small circles (the recovery ladder's first rung and the
/// spots a bot got stuck at earlier). A segment that starts inside a circle may leave it, so a
/// bot standing in the very spot it avoids can still walk out.
pub struct Avoid {
    pub world: Arc<dyn NavWorld + Send + Sync>,
    pub spots: Vec<(f32, f32)>,
    pub radius: f32,
}

impl NavWorld for Avoid {
    fn height(&self, x: f32, y: f32) -> Option<f32> {
        self.world.height(x, y)
    }

    fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        self.world.water_depth(x, y, ground)
    }

    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.world.nav_route(from, to)
    }

    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.world.road_route(from, to)
    }

    fn road_route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.world.road_route_budget(from, to, budget)
    }

    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.world.road_route_avoiding(from, to, banned, budget)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        for spot in &self.spots {
            if !segment_near((from.x, from.y), (to.x, to.y), *spot, self.radius) {
                continue;
            }
            let d = |p: Vec3| ((p.x - spot.0).powi(2) + (p.y - spot.1).powi(2)).sqrt();
            let leaving = d(from) < self.radius && d(to) >= d(from);
            if !leaving {
                return Some(Obstacle::High(self.radius));
            }
        }
        self.world.obstacle(from, to)
    }

    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        self.world.jump_clear(from, to)
    }

    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.world.zone_at(x, y)
    }
}

/// A world view without static obstacles: the walker retraces ground it already stood on (the
/// breadcrumb trail) to get out of a pocket the capsule sweeps call closed on every side.
pub struct Retrace<'a>(pub &'a dyn NavWorld);

impl NavWorld for Retrace<'_> {
    fn height(&self, x: f32, y: f32) -> Option<f32> {
        self.0.height(x, y)
    }

    fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        self.0.water_depth(x, y, ground)
    }

    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.0.road_route(from, to)
    }

    fn road_route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.0.road_route_budget(from, to, budget)
    }

    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.0.road_route_avoiding(from, to, banned, budget)
    }

    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        self.0.jump_clear(from, to)
    }

    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.0.zone_at(x, y)
    }
}

fn segment_near(a: (f32, f32), b: (f32, f32), p: (f32, f32), radius: f32) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let x = a.0 + dx * t;
    let y = a.1 + dy * t;
    ((x - p.0).powi(2) + (y - p.1).powi(2)).sqrt() < radius
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepAction {
    Run,
    Swim,
    /// Jump over an obstacle of `height` metres.
    Jump(f32),
    /// Step down a ledge of `height` metres (<= safe fall).
    Drop(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveStep {
    pub to: Vec3,
    pub action: StepAction,
}

/// A plan plus why the planners that were tried and rejected did not produce it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanReport {
    pub plan: Option<Plan>,
    /// One entry per planner attempt, e.g. `navmesh: no route`, `heightmap: no route (start
    /// off the height data)`.
    pub notes: Vec<String>,
}

impl PlanReport {
    /// The planner that produced the route, or `none`.
    pub fn planner(&self) -> &'static str {
        self.plan.as_ref().map_or("none", |p| p.source)
    }

    pub fn failure(&self) -> Option<String> {
        if self.plan.is_some() {
            None
        } else {
            Some(self.notes.join("; "))
        }
    }
}

/// Plans `from` -> `to`.
pub fn plan(world: &dyn NavWorld, from: Vec3, to: Vec3) -> Option<Plan> {
    plan_explain(world, from, to).plan
}

/// [`plan`] with the reasons each rejected planner gave.
pub fn plan_explain(world: &dyn NavWorld, from: Vec3, to: Vec3) -> PlanReport {
    plan_explain_budget(world, from, to, PlanBudget::new())
}

/// [`plan_explain`] sharing an existing budget (the walk's cancellation token and limits).
pub fn plan_explain_budget(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    mut budget: PlanBudget,
) -> PlanReport {
    let mut notes = Vec::new();
    let plan = plan_inner(world, from, to, &mut budget, &mut notes);
    if plan.is_none() && budget.stopped() {
        notes.push("plan budget exhausted (time / work / cancelled)".into());
    }
    PlanReport { plan, notes }
}

/// Runs `plan_explain` under the concurrent-plan gate; `None` when the walk was cancelled
/// before or while waiting for a permit.
pub async fn plan_guarded(
    world: Arc<dyn NavWorld + Send + Sync>,
    from: Vec3,
    to: Vec3,
    gate: Arc<PlanGate>,
    cancel: PlanCancel,
) -> Option<PlanReport> {
    if cancel.is_cancelled() {
        return None;
    }
    let permit = tokio::select! {
        permit = gate.acquire() => permit,
        _ = cancel.cancelled() => return None,
    };
    let report = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        plan_explain_budget(&*world, from, to, PlanBudget::new().with_cancel(cancel))
    })
    .await
    .ok()?;
    Some(report)
}

fn plan_inner(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    budget: &mut PlanBudget,
    notes: &mut Vec<String>,
) -> Option<Plan> {
    let dist = dist2(from, to);
    if !budget.poll() {
        return None;
    }
    // Beyond the navmesh range the transfer-road graph gives the long middle of the route.
    if dist > NAV_DIRECT_MAX_M {
        // A road edge the terrain / obstacle check rejects is dropped and the road A* asked
        // again (a few times): one bad edge does not cost the rest of the network.
        let mut banned: Vec<(Vec3, Vec3)> = Vec::new();
        for _ in 0..=MAX_ROAD_RETRIES {
            let Some(roads) = world.road_route_avoiding(from, to, &banned, budget) else {
                break;
            };
            match plan_road_route(world, from, to, &roads, budget) {
                Ok(plan) => return Some(plan),
                Err((why, bad_edge)) => {
                    notes.push(why);
                    match bad_edge {
                        Some(edge) => banned.push(edge),
                        None => break,
                    }
                }
            }
            if budget.stopped() {
                return None;
            }
        }
    }
    if dist <= NAV_DIRECT_MAX_M {
        if !budget.tick() {
            return None;
        }
        match world.nav_route(from, to) {
            Some(nav) if !nav.is_empty() => {
                if !budget.charge(nav.len()) {
                    return None;
                }
                let mut prev = from;
                let mut rejected = None;
                for p in &nav {
                    if let Err(why) = segment_check(world, prev, *p, budget) {
                        if budget.stopped() {
                            return None;
                        }
                        rejected = Some(format!(
                            "navmesh: segment ({:.1}, {:.1}) -> ({:.1}, {:.1}) rejected: {why}",
                            prev.x, prev.y, p.x, p.y
                        ));
                        break;
                    }
                    prev = *p;
                }
                match rejected {
                    None => {
                        return Some(Plan {
                            points: nav,
                            source: "navmesh",
                        })
                    }
                    Some(note) => notes.push(note),
                }
            }
            Some(_) => notes.push("navmesh: empty route".into()),
            None => notes.push("navmesh: no route".into()),
        }
    } else {
        notes.push(format!("navmesh: skipped (distance {dist:.0} m)"));
    }
    // The fine grid stays inside a box around the two points; a route that must go round a
    // cliff far outside it is found by the coarse corridor below. Its diagnostics come from
    // the same search, so a failed route does not pay for a second full grid run.
    let mut fine_note = None;
    if dist <= 700.0 {
        let stats = std::cell::RefCell::new(GridStats::default());
        if let Some(points) = grid_search(
            world,
            from,
            to,
            2.0,
            200.0,
            GridMode::Fine,
            Some(&stats),
            budget,
        ) {
            return Some(Plan {
                points,
                source: "heightmap",
            });
        }
        fine_note = Some(format!(
            "heightmap: no route: every candidate path is blocked; {}",
            stats.borrow().describe((to.x, to.y))
        ));
    }
    let Some(coarse) = coarse_route(world, from, to, budget) else {
        notes.push(fine_note.unwrap_or_else(|| "heightmap: no route".into()));
        return None;
    };
    if let Some(note) = fine_note {
        notes.push(note);
    }
    // Chain legs of about NAV_LEG_M along the coarse corridor. Each leg is walked on the best
    // planner that can do it: the navmesh, else the coarse segment when it passes the walker's
    // terrain / obstacle rules, else a fine 2 m terrain A* confined to the leg.
    let mut anchors: Vec<usize> = Vec::new();
    let mut since = 0.0f32;
    let mut prev = from;
    for (i, p) in coarse.iter().enumerate() {
        since += dist2(prev, *p);
        prev = *p;
        if since >= NAV_LEG_M && i + 1 < coarse.len() {
            anchors.push(i);
            since = 0.0;
        }
    }
    anchors.push(coarse.len() - 1);
    let mut route = Vec::new();
    let mut leg_start = 0usize;
    let mut leg_from = from;
    let (mut legs_nav, mut legs_grid) = (0usize, 0usize);
    for end in anchors {
        let leg_to = coarse[end];
        let coarse_leg = &coarse[leg_start..=end];
        if !budget.tick() {
            return None;
        }
        let nav_leg = world.nav_route(leg_from, leg_to).filter(|points| {
            if !budget.charge(points.len()) {
                return false;
            }
            let mut prev = leg_from;
            points.iter().all(|p| {
                let ok = segment_check(world, prev, *p, budget).is_ok();
                prev = *p;
                ok
            })
        });
        if budget.stopped() {
            return None;
        }
        if let Some(points) = nav_leg {
            route.extend(points);
            legs_nav += 1;
        } else {
            legs_grid += 1;
            let mut prev = leg_from;
            let coarse_ok = coarse_leg.iter().all(|p| {
                let ok = segment_check(world, prev, *p, budget).is_ok();
                prev = *p;
                ok
            });
            if budget.stopped() {
                return None;
            }
            if coarse_ok {
                route.extend(coarse_leg.iter().copied());
            } else if let Some(fine) = grid_route_budget(world, leg_from, leg_to, 2.0, 60.0, budget)
            {
                route.extend(fine);
            } else if dist <= 700.0 {
                // A short route is only worth walking when every leg is known to pass.
                return None;
            } else {
                // Keep the coarse corridor: the walker's recovery ladder deals with the
                // obstacle the 16 m sampling could not see.
                route.extend(coarse_leg.iter().copied());
            }
        }
        leg_start = end + 1;
        leg_from = leg_to;
    }
    let source = if legs_grid == 0 {
        "navmesh"
    } else if legs_nav == 0 {
        "heightmap"
    } else {
        "navmesh+heightmap"
    };
    Some(Plan {
        points: route,
        source,
    })
}

/// Chains the road route between `from` and `to`: a planned first leg to the first road node,
/// the road nodes, and a planned last leg to the goal. `Err` explains why the road route was
/// rejected (the caller then tries the ordinary planners).
fn plan_road_route(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    roads: &[Vec3],
    budget: &mut PlanBudget,
) -> Result<Plan, (String, Option<(Vec3, Vec3)>)> {
    let (Some(&first), Some(&last)) = (roads.first(), roads.last()) else {
        return Err(("roads: empty route".into(), None));
    };
    // Both ends must be close to the network and the road itself must not be a long detour
    // compared with walking straight (a 1 km snap made "roads" win over every other planner).
    let access = dist2(from, first) + dist2(last, to);
    if dist2(from, first) > roads::MAX_SNAP_M || dist2(last, to) > roads::MAX_SNAP_M {
        return Err((
            format!(
                "roads: access legs too long ({:.0} m / {:.0} m)",
                dist2(from, first),
                dist2(last, to)
            ),
            None,
        ));
    }
    // The detour test needs no collision query: it runs before the segment checks.
    let road_len: f32 = roads.windows(2).map(|pair| dist3(pair[0], pair[1])).sum();
    let straight = dist3(from, to).max(1.0);
    if road_len > 1.5 * straight + access {
        return Err((
            format!("roads: detour {road_len:.0} m for a {straight:.0} m trip"),
            None,
        ));
    }
    let mut prev = first;
    for p in roads.iter().skip(1) {
        if let Err(why) = segment_check_coarse(world, prev, *p, budget) {
            // An edge the budget cut short is not the edge's fault: only a real rejection
            // drops it from the network.
            let edge = (!budget.stopped()).then_some((prev, *p));
            return Err((format!("roads: segment rejected: {why}"), edge));
        }
        prev = *p;
    }
    if budget.stopped() {
        return Err(("roads: plan budget exhausted".into(), None));
    }
    let head = plan_leg(world, from, first, budget)
        .ok_or_else(|| ("roads: no first leg to the road network".to_string(), None))?;
    let tail = plan_leg(world, last, to, budget)
        .ok_or_else(|| ("roads: no last leg from the road network".to_string(), None))?;
    let mut points = head;
    points.extend(roads.iter().skip(1).copied());
    points.extend(tail);
    if points.is_empty() {
        points.push(to);
    }
    Ok(Plan {
        points,
        source: "roads",
    })
}

/// Plans one road leg (`from` excluded, `to` last) with the navmesh, the fine height grid or
/// the coarse corridor.
fn plan_leg(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    budget: &mut PlanBudget,
) -> Option<Vec<Vec3>> {
    let dist = dist2(from, to);
    if dist <= STOP_DISTANCE {
        return Some(vec![to]);
    }
    if dist <= NAV_DIRECT_MAX_M + NAV_LEG_M {
        if !budget.tick() {
            return None;
        }
        if let Some(nav) = world.nav_route(from, to) {
            if !budget.charge(nav.len()) {
                return None;
            }
            let mut prev = from;
            let checked = nav.iter().all(|p| {
                let ok = segment_check(world, prev, *p, budget).is_ok();
                prev = *p;
                ok
            });
            if checked && !nav.is_empty() && !budget.stopped() {
                return Some(nav);
            }
        }
    }
    if let Some(route) = grid_route_budget(world, from, to, 2.0, 200.0, budget) {
        return Some(route);
    }
    coarse_route(world, from, to, budget)
}

fn coarse_route(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    budget: &mut PlanBudget,
) -> Option<Vec<Vec3>> {
    let dist = dist2(from, to);
    let margin = (dist * 0.2).clamp(500.0, 1500.0);
    for step in [2.0f32, 4.0, 6.0, 8.0, 12.0, 16.0] {
        if !budget.poll() {
            return None;
        }
        let w = ((to.x - from.x).abs() + 2.0 * margin) / step + 2.0;
        let h = ((to.y - from.y).abs() + 2.0 * margin) / step + 2.0;
        if w * h <= MAX_GRID_CELLS as f32 {
            // Keep off cells beside a cliff first (the corridor then runs down the middle of
            // a pass instead of hugging its wall), then accept them.
            for mode in [GridMode::CoarseKeepOff, GridMode::Coarse] {
                if let Some(route) =
                    grid_route_mode_budget(world, from, to, step, margin, mode, budget)
                {
                    return Some(route);
                }
                if budget.stopped() {
                    return None;
                }
            }
        }
    }
    None
}

/// Terrain A* at `step` metres inside the bounding box grown by `margin`.
pub fn grid_route(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    step: f32,
    margin: f32,
) -> Option<Vec<Vec3>> {
    grid_route_mode(world, from, to, step, margin, GridMode::Fine)
}

/// How [`grid_route_mode`] judges a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    /// The walker's own rules (per-step climb, obstacles).
    Fine,
    /// Slope-only corridor.
    Coarse,
    /// [`GridMode::Coarse`] that also refuses cells next to a cliff-steep neighbour.
    CoarseKeepOff,
}

/// [`grid_route`]; the coarse modes plan a long corridor: cells many metres apart are compared by
/// slope only (the per-step climb limit of a walker is meaningless over 16 m, which blocked
/// every hill), obstacles are left to the per-leg refinement, and flatter ground is cheaper so
/// the corridor follows valleys and passes instead of climbing ridges.
pub fn grid_route_mode(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    step: f32,
    margin: f32,
    mode: GridMode,
) -> Option<Vec<Vec3>> {
    let mut budget = PlanBudget::new();
    grid_route_mode_budget(world, from, to, step, margin, mode, &mut budget)
}

fn grid_route_budget(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    step: f32,
    margin: f32,
    budget: &mut PlanBudget,
) -> Option<Vec<Vec3>> {
    grid_route_mode_budget(world, from, to, step, margin, GridMode::Fine, budget)
}

fn grid_route_mode_budget(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    step: f32,
    margin: f32,
    mode: GridMode,
    budget: &mut PlanBudget,
) -> Option<Vec<Vec3>> {
    grid_search(world, from, to, step, margin, mode, None, budget)
}

/// What a failed grid search saw: where it stalled and what rejected its steps.
#[derive(Debug, Default, Clone)]
pub struct GridStats {
    pub expanded: usize,
    pub no_height: usize,
    pub obstacle_high: usize,
    pub obstacle_low_unfit: usize,
    pub terrain: usize,
    /// The plan's node budget ran out.
    pub budget_exhausted: bool,
    /// The expanded cell nearest to the goal and how far from it.
    pub best: Option<((f32, f32), f32)>,
    /// The first obstacle-rejected step.
    pub first_obstacle: Option<((f32, f32), (f32, f32))>,
}

impl GridStats {
    fn describe(&self, goal: (f32, f32)) -> String {
        let best = self.best.map_or("none".to_string(), |((x, y), d)| {
            format!("({x:.0}, {y:.0}) {d:.0} m from the goal")
        });
        let first = self.first_obstacle.map_or(String::new(), |(a, b)| {
            format!(
                "; first obstacle step ({:.0}, {:.0}) -> ({:.0}, {:.0})",
                a.0, a.1, b.0, b.1
            )
        });
        format!(
            "search expanded {} cells and stalled at {best} (goal ({:.0}, {:.0})); rejected: {} by high obstacles, {} by unjumpable low obstacles, {} by terrain, {} without height data{first}{}",
            self.expanded,
            goal.0,
            goal.1,
            self.obstacle_high,
            self.obstacle_low_unfit,
            self.terrain,
            self.no_height,
            if self.budget_exhausted {
                "; plan budget exhausted"
            } else {
                ""
            }
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn grid_search(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    step: f32,
    margin: f32,
    mode: GridMode,
    stats: Option<&std::cell::RefCell<GridStats>>,
    budget: &mut PlanBudget,
) -> Option<Vec<Vec3>> {
    let coarse = mode != GridMode::Fine;
    let (min_x, min_y) = (from.x.min(to.x) - margin, from.y.min(to.y) - margin);
    let (max_x, max_y) = (from.x.max(to.x) + margin, from.y.max(to.y) + margin);
    let min_x = min_x.max(0.0);
    let min_y = min_y.max(0.0);
    let w = (((max_x - min_x) / step).ceil() as i32 + 1).max(2);
    let h = (((max_y - min_y) / step).ceil() as i32 + 1).max(2);
    let n = w as usize * h as usize;
    if n > MAX_GRID_CELLS {
        return None;
    }
    // The height prefill is the dominant cost of a coarse corridor: count every cell and stop
    // as soon as the plan budget or the walk's cancellation token says so.
    if !budget.charge(n) {
        if let Some(st) = stats {
            st.borrow_mut().budget_exhausted = true;
        }
        return None;
    }
    let idx = |ix: i32, iy: i32| (ix * h + iy) as usize;
    let pos = |ix: i32, iy: i32| Vec3::new(min_x + ix as f32 * step, min_y + iy as f32 * step, 0.0);
    let mut heights = vec![f32::NAN; n];
    let mut prefilled = 0usize;
    for ix in 0..w {
        for iy in 0..h {
            let (x, y) = (pos(ix, iy).x, pos(ix, iy).y);
            heights[idx(ix, iy)] = world.height(x, y).unwrap_or(f32::NAN);
            prefilled += 1;
            if prefilled.is_multiple_of(4096) && !budget.poll() {
                if let Some(st) = stats {
                    st.borrow_mut().budget_exhausted = true;
                }
                return None;
            }
        }
    }
    let goal = (
        ((to.x - min_x) / step).round() as i32,
        ((to.y - min_y) / step).round() as i32,
    );
    let start = (
        ((from.x - min_x) / step).round() as i32,
        ((from.y - min_y) / step).round() as i32,
    );
    if start.0 < 0 || start.1 < 0 || start.0 >= w || start.1 >= h {
        return None;
    }
    if goal.0 < 0 || goal.1 < 0 || goal.0 >= w || goal.1 >= h {
        return None;
    }
    let steep_limit = MAX_CLIMB_DEG.to_radians().tan();
    let mut near_cliff = vec![false; n];
    if mode == GridMode::CoarseKeepOff {
        for ix in 0..w {
            for iy in 0..h {
                let i = idx(ix, iy);
                if !heights[i].is_finite() {
                    continue;
                }
                near_cliff[i] = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|(dx, dy)| {
                    let (nx, ny) = (ix + dx, iy + dy);
                    nx >= 0
                        && ny >= 0
                        && nx < w
                        && ny < h
                        && heights[idx(nx, ny)].is_finite()
                        && (heights[idx(nx, ny)] - heights[i]).abs() / step > steep_limit
                });
            }
        }
    }
    let node_ok = |i: usize| heights[i].is_finite();
    // A start / goal cell without height data (a world edge, a hole) snaps to the nearest cell
    // that has some.
    let snap = |c: (i32, i32)| -> Option<(i32, i32)> {
        if node_ok(idx(c.0, c.1)) {
            return Some(c);
        }
        for radius in 1..=SNAP_CELLS {
            let mut best: Option<((i32, i32), i32)> = None;
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    if dx.abs().max(dy.abs()) != radius {
                        continue;
                    }
                    let (nx, ny) = (c.0 + dx, c.1 + dy);
                    if nx < 0 || ny < 0 || nx >= w || ny >= h || !node_ok(idx(nx, ny)) {
                        continue;
                    }
                    let d2 = dx * dx + dy * dy;
                    if best.is_none_or(|(_, b)| d2 < b) {
                        best = Some(((nx, ny), d2));
                    }
                }
            }
            if let Some((cell, _)) = best {
                return Some(cell);
            }
        }
        None
    };
    let (Some(start), Some(goal)) = (snap(start), snap(goal)) else {
        return None;
    };
    let start_i = idx(start.0, start.1);
    let goal_i = idx(goal.0, goal.1);
    // Water depth per cell, asked once (the collision water query is the expensive part of a
    // coarse corridor over hundreds of thousands of cells).
    let water_cache = std::cell::RefCell::new(vec![f32::NAN; n]);
    // Every collision query the plan spends is counted after the expansion; the water cache
    // avoids asking the same cell twice, the small direct-mapped obstacle cache avoids the
    // repeated capsule sweeps of neighbouring expansions.
    let queries = std::cell::Cell::new(0usize);
    let water_at = |i: usize, x: f32, y: f32, ground: f32| -> f32 {
        let cached = water_cache.borrow()[i];
        if !cached.is_nan() {
            return cached;
        }
        queries.set(queries.get() + 1);
        let depth = world.water_depth(x, y, ground);
        water_cache.borrow_mut()[i] = depth;
        depth
    };
    const OBSTACLE_CACHE: usize = 1 << 14;
    let obstacle_cache = std::cell::RefCell::new(vec![(u64::MAX, 0u8); OBSTACLE_CACHE]);
    let obstacle_at = |a: usize, b: usize, a3: Vec3, b3: Vec3| -> Option<Obstacle> {
        let key = ((a as u64) << 32) | b as u64;
        let slot = (key.wrapping_mul(0x9E37_79B1_9E37_79B1) >> 50) as usize & (OBSTACLE_CACHE - 1);
        let (cached_key, cached) = obstacle_cache.borrow()[slot];
        if cached_key == key && cached != 0 {
            return match cached {
                1 => None,
                2 => Some(Obstacle::Low(0.0)),
                _ => Some(Obstacle::High(JUMP_HEIGHT_M + 0.1)),
            };
        }
        queries.set(queries.get() + 1);
        let obstacle = world.obstacle(a3, b3);
        let value = match obstacle {
            None => 1,
            Some(Obstacle::Low(_)) => 2,
            Some(Obstacle::High(_)) => 3,
        };
        obstacle_cache.borrow_mut()[slot] = (key, value);
        obstacle
    };
    let cost = |a: usize, b: usize, d: f32| -> Option<f32> {
        let (ha, hb) = (heights[a], heights[b]);
        let dz = hb - ha;
        let (ax, ay) = ((a / h as usize) as i32, (a % h as usize) as i32);
        let (bx, by) = ((b / h as usize) as i32, (b % h as usize) as i32);
        let a_pos = pos(ax, ay);
        let b_pos = pos(bx, by);
        let water = water_at(b, b_pos.x, b_pos.y, hb);
        let water_a = water_at(a, a_pos.x, a_pos.y, ha);
        let a3 = Vec3::new(a_pos.x, a_pos.y, ha);
        let b3 = Vec3::new(b_pos.x, b_pos.y, hb);
        if water > SWIM_DEPTH_M || water_a > SWIM_DEPTH_M {
            return Some(d * (RUN_SPEED_MPS / SWIM_SPEED_MPS));
        }
        if coarse {
            let slope = dz.abs() / d.max(0.001);
            if slope > steep_limit {
                return None;
            }
            return Some(d * (1.0 + 2.0 * slope));
        }
        // The first step leaves where the bot already stands: a capsule wedged against a rock
        // sees an "obstacle" in every direction, and the walker's recovery ladder (backtrack /
        // step out) is what gets it out, not a refusal to plan.
        let obstacle = if a == start_i {
            None
        } else {
            obstacle_at(a, b, a3, b3)
        };
        if let Some(obstacle) = obstacle {
            // A fence is jumped, but a "low obstacle" the sweep reports on a steep hillside is
            // the hill itself: the ground must be climbable as well.
            return match obstacle {
                Obstacle::Low(h)
                    if h <= JUMP_HEIGHT_M
                        && d <= JUMP_DISTANCE_M
                        && terrain_step_ok(d, dz).is_ok() =>
                {
                    Some(d + 2.0)
                }
                other => {
                    if let Some(st) = stats {
                        let mut st = st.borrow_mut();
                        if matches!(other, Obstacle::High(_)) {
                            st.obstacle_high += 1;
                        } else {
                            st.obstacle_low_unfit += 1;
                        }
                        st.first_obstacle
                            .get_or_insert(((a_pos.x, a_pos.y), (b_pos.x, b_pos.y)));
                    }
                    None
                }
            };
        }
        if terrain_step_ok(d, dz).is_err() {
            if let Some(st) = stats {
                st.borrow_mut().terrain += 1;
            }
            return None;
        }
        Some(d)
    };
    let heuristic = |i: usize| -> f32 {
        let ix = (i / h as usize) as i32;
        let iy = (i % h as usize) as i32;
        let dx = (goal.0 - ix).abs() as f32;
        let dy = (goal.1 - iy).abs() as f32;
        ((dx + dy) + (std::f32::consts::SQRT_2 - 2.0) * dx.min(dy)) * step
    };
    let mut g = vec![f32::INFINITY; n];
    let mut parent = vec![-1i32; n];
    let mut closed = vec![false; n];
    let mut open: BinaryHeap<(std::cmp::Reverse<u64>, usize)> = BinaryHeap::new();
    g[start_i] = 0.0;
    open.push((
        std::cmp::Reverse((heuristic(start_i) * 100.0) as u64),
        start_i,
    ));
    let mut found = false;
    // The expanded cell nearest to the goal: a goal inside a collider (an NPC standing in a
    // building, a doodad) cannot be entered, but the bot only has to come close to it.
    let mut nearest: Option<(usize, f32)> = None;
    while let Some((_, current)) = open.pop() {
        if !budget.tick() {
            if let Some(st) = stats {
                st.borrow_mut().budget_exhausted = true;
            }
            return None;
        }
        if !closed[current] && mode == GridMode::Fine {
            let (cx, cy) = ((current / h as usize) as i32, (current % h as usize) as i32);
            let cell = pos(cx, cy);
            let d = ((cell.x - to.x).powi(2) + (cell.y - to.y).powi(2)).sqrt();
            if nearest.is_none_or(|(_, b)| d < b) {
                nearest = Some((current, d));
            }
        }
        if current == goal_i {
            found = true;
            break;
        }
        if closed[current] {
            continue;
        }
        closed[current] = true;
        let ix = (current / h as usize) as i32;
        let iy = (current % h as usize) as i32;
        if let Some(st) = stats {
            let mut st = st.borrow_mut();
            st.expanded += 1;
            let cell = pos(ix, iy);
            let d = ((cell.x - to.x).powi(2) + (cell.y - to.y).powi(2)).sqrt();
            if st.best.is_none_or(|(_, b)| d < b) {
                st.best = Some(((cell.x, cell.y), d));
            }
        }
        for (dx, dy) in [
            (-1, 0),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (-1, 1),
            (1, -1),
            (1, 1),
        ] {
            let (nx, ny) = (ix + dx, iy + dy);
            if nx < 0 || ny < 0 || nx >= w || ny >= h {
                continue;
            }
            let ni = idx(nx, ny);
            if !closed[ni] && !node_ok(ni) {
                if let Some(st) = stats {
                    st.borrow_mut().no_height += 1;
                }
            }
            if closed[ni] || !node_ok(ni) || (near_cliff[ni] && ni != goal_i) {
                continue;
            }
            let d = if dx == 0 || dy == 0 {
                step
            } else {
                step * std::f32::consts::SQRT_2
            };
            let Some(step_cost) = cost(current, ni, d) else {
                continue;
            };
            let ng = g[current] + step_cost;
            if ng + 1e-4 >= g[ni] {
                continue;
            }
            g[ni] = ng;
            parent[ni] = current as i32;
            open.push((std::cmp::Reverse(((ng + heuristic(ni)) * 100.0) as u64), ni));
        }
        let spent = queries.replace(0);
        if !budget.charge(spent) {
            if let Some(st) = stats {
                st.borrow_mut().budget_exhausted = true;
            }
            return None;
        }
    }
    let mut goal_i = goal_i;
    let mut reached_goal = true;
    if !found {
        // Come as close as the search got when that is within reach of an interaction.
        match nearest {
            Some((cell, d)) if d <= GOAL_TOLERANCE_M && cell != start_i => {
                goal_i = cell;
                reached_goal = false;
            }
            _ => return None,
        }
    }
    let mut nodes = Vec::new();
    let mut current = goal_i;
    while current != start_i {
        nodes.push(current);
        let p = parent[current];
        if p < 0 {
            return None;
        }
        current = p as usize;
    }
    nodes.reverse();
    let coord = |i: usize| {
        (
            (nodes[i] / h as usize) as i32,
            (nodes[i] % h as usize) as i32,
        )
    };
    let mut route = Vec::with_capacity(nodes.len());
    for i in 0..nodes.len() {
        let keep = if i == 0 || i + 1 == nodes.len() {
            true
        } else {
            let (ax, ay) = coord(i - 1);
            let (bx, by) = coord(i);
            let (cx, cy) = coord(i + 1);
            (bx - ax, by - ay) != (cx - bx, cy - by)
        };
        if keep {
            let (ix, iy) = coord(i);
            let p = pos(ix, iy);
            route.push(Vec3::new(p.x, p.y, heights[nodes[i]]));
        }
    }
    let last = route.len().saturating_sub(1);
    if route.is_empty() {
        route.push(to);
    } else if reached_goal {
        route[last] = to;
    }
    Some(route)
}

/// The steepest slope a walker simply runs up: a 0.54 m run step then rises at most the 0.5 m
/// step height (about 43 degrees).
const RUN_SLOPE: f32 = STEP_HEIGHT_M / (RUN_SPEED_MPS * STEP_MS as f32 / 1000.0);

/// Can a walker climb / drop `dz` metres over `d` metres of ground (no obstacle involved)? A
/// sample `d` metres apart that rises more than one hop is still fine on a slope the walker
/// runs up step by step (the 2 m sampling must not turn a 35 degree hillside into a cliff).
fn terrain_step_ok(d: f32, dz: f32) -> Result<(), &'static str> {
    if dz > 0.0 {
        let slope = MAX_CLIMB_DEG.to_radians().tan();
        if dz > STEP_HEIGHT_M && d > 0.0 && dz / d > RUN_SLOPE {
            if dz > JUMP_HEIGHT_M || d > JUMP_DISTANCE_M {
                return Err("climb too high");
            }
            if dz / d > slope {
                return Err("slope too steep");
            }
        } else if dz > STEP_HEIGHT_M && d <= 0.0 && dz > JUMP_HEIGHT_M {
            return Err("climb too high");
        }
    } else if -dz > MAX_SAFE_DROP_M {
        return Err("drop too deep");
    }
    Ok(())
}

/// The jump arc at `t` seconds: a straight interpolation from `from` to the landing `to`
/// plus the vertical jump parabola. At `t = JUMP_AIR_TIME_S` it ends exactly at `to.z` (a
/// ledge up to `JUMP_HEIGHT_M` above, or a safe drop below), never at the take-off height.
pub fn jump_arc(from: Vec3, to: Vec3, t: f32) -> Vec3 {
    let t = t.clamp(0.0, JUMP_AIR_TIME_S);
    let k = if JUMP_AIR_TIME_S > 0.0 {
        t / JUMP_AIR_TIME_S
    } else {
        1.0
    };
    Vec3::new(
        from.x + (to.x - from.x) * k,
        from.y + (to.y - from.y) * k,
        from.z + (to.z - from.z) * k + JUMP_SPEED_MPS * t - 0.5 * JUMP_GRAVITY * t * t,
    )
}

/// Walks the straight segment in 2 m samples and applies the same terrain / obstacle rules
/// the step executor and the grid planner use, so a long navmesh leg that gently climbs a
/// few metres is not mistaken for one impossible step.
fn segment_check(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    budget: &mut PlanBudget,
) -> Result<(), String> {
    segment_check_step(world, from, to, 2.0, budget)
}

/// [`segment_check`] at a coarser sampling (the road graph's long edges): every sample is a
/// collision query and counts against the plan budget.
fn segment_check_coarse(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    budget: &mut PlanBudget,
) -> Result<(), String> {
    segment_check_step(world, from, to, 16.0, budget)
}

fn segment_check_step(
    world: &dyn NavWorld,
    from: Vec3,
    to: Vec3,
    sample_m: f32,
    budget: &mut PlanBudget,
) -> Result<(), String> {
    let d = dist2(from, to);
    let n = (d / sample_m).ceil().max(1.0) as usize;
    // The walker stands on the surface: use its height, not a possibly off-by-a-bit `from.z`.
    let mut prev = Vec3::new(
        from.x,
        from.y,
        world.height(from.x, from.y).unwrap_or(from.z),
    );
    for i in 1..=n {
        if !budget.tick() {
            return Err("plan budget exhausted".into());
        }
        let t = i as f32 / n as f32;
        let (x, y) = (from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        let Some(h) = world.height(x, y) else {
            return Err(format!("no height data at ({x:.1}, {y:.1})"));
        };
        let here = Vec3::new(x, y, h);
        terrain_step_ok(dist2(prev, here), h - prev.z).map_err(|why| {
            format!(
                "{why} ({:.2} m over {:.1} m)",
                h - prev.z,
                dist2(prev, here)
            )
        })?;
        if world.water_depth(x, y, h) <= SWIM_DEPTH_M {
            match world.obstacle(prev, here) {
                Some(Obstacle::High(top)) => {
                    return Err(format!("obstacle {top:.1} m high at ({x:.1}, {y:.1})"))
                }
                Some(Obstacle::Low(top)) if top > JUMP_HEIGHT_M => {
                    return Err(format!("obstacle {top:.1} m high at ({x:.1}, {y:.1})"))
                }
                _ => {}
            }
        }
        prev = here;
    }
    Ok(())
}

fn dist2(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

fn dist3(a: Vec3, b: Vec3) -> f32 {
    (dist2(a, b).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

/// The live navmesh / heightmap / collision view of one world instance (through the
/// `aaemu-game` facade [`NavView`]).
pub struct GameNav<'a> {
    view: NavView<'a>,
    roads: Option<&'a RoadGraphCache>,
}

impl<'a> GameNav<'a> {
    pub fn of_player(
        server: &'a GameServer,
        player: &PlayerRef,
        roads: Option<&'a RoadGraphCache>,
    ) -> Option<Self> {
        let view = NavView::of_object(server, &WorldObject::from(player.clone()))?;
        Some(Self { view, roads })
    }

    pub fn of_instance(
        server: &'a GameServer,
        instance_id: u32,
        roads: Option<&'a RoadGraphCache>,
    ) -> Option<Self> {
        Some(Self {
            view: NavView::of_instance(server, instance_id)?,
            roads,
        })
    }

    pub fn template_id(&self) -> u32 {
        self.view.template_id()
    }
}

impl NavWorld for GameNav<'_> {
    fn height(&self, x: f32, y: f32) -> Option<f32> {
        self.view.height(x, y)
    }

    fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        self.view.water_depth(x, y, ground)
    }

    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        let mut points: Vec<Vec3> = self.view.nav_route(from, to)?.into_iter().skip(1).collect();
        if points.is_empty() {
            points.push(to);
        }
        let last = points.len() - 1;
        points[last] = to;
        Some(points)
    }

    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        let graph = self
            .roads?
            .get(self.view.server(), self.view.template_id())?;
        graph.route(from, to)
    }

    fn road_route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.road_route_avoiding(from, to, &[], budget)
    }

    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        let graph = self
            .roads?
            .get(self.view.server(), self.view.template_id())?;
        graph.route_avoiding(from, to, banned, budget)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        // The capsule's lowest point is `STEP_HEIGHT_M` above the feet (anything lower is
        // stepped over), so a bot standing on the ground never overlaps the terrain it sweeps
        // along: centre = feet + step + half the height, cylinder half height = half the
        // height minus the cap radius.
        let centre = STEP_HEIGHT_M + CAPSULE_HEIGHT / 2.0;
        let hit = self.view.sweep_capsule(
            [from.x, from.y, from.z + centre],
            [to.x, to.y, to.z + centre],
            CAPSULE_RADIUS,
            CAPSULE_HEIGHT / 2.0 - CAPSULE_RADIUS,
        )??;
        // The top of the obstacle within a jump's reach above the step's ground.
        let base = from.z.max(to.z);
        let top = self
            .view
            .ground_probe(hit.point.x, hit.point.y, base + JUMP_HEIGHT_M, 0.05, 32.0)
            .flatten()
            .unwrap_or(hit.point.z);
        let height = (top - base).max(0.0);
        Some(if height > 0.1 && height <= JUMP_HEIGHT_M + 0.1 {
            Obstacle::Low(height)
        } else {
            Obstacle::High(height.max(JUMP_HEIGHT_M + 0.1))
        })
    }

    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        // Sweep the whole arc the walker will send, not just the first ground step. The
        // capsule's bottom starts a hair above the feet so the take-off ground is not a hit.
        let horizontal = dist2(from, to);
        let samples = ((horizontal / 0.5).ceil() as usize).clamp(4, 16);
        let mut prev = from;
        for i in 1..=samples {
            let t = i as f32 / samples as f32 * JUMP_AIR_TIME_S;
            let here = jump_arc(from, to, t);
            let (a, b) = (
                [prev.x, prev.y, prev.z + CAPSULE_HEIGHT / 2.0 + 0.05],
                [here.x, here.y, here.z + CAPSULE_HEIGHT / 2.0 + 0.05],
            );
            match self.view.sweep_capsule(
                a,
                b,
                CAPSULE_RADIUS,
                CAPSULE_HEIGHT / 2.0 - CAPSULE_RADIUS,
            ) {
                None => return None,
                Some(Some(_)) => return Some(false),
                Some(None) => {}
            }
            prev = here;
        }
        Some(true)
    }

    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.view.zone_at(x, y)
    }
}

/// The owned [`GameNav`]: the walker and the planner move it into `spawn_blocking`, so the
/// geometry-loading collision queries never run on a tokio worker.
#[derive(Clone)]
pub struct OwnedGameNav {
    server: Arc<GameServer>,
    player: PlayerRef,
    roads: RoadGraphCache,
}

impl OwnedGameNav {
    pub fn of_player(
        server: Arc<GameServer>,
        player: PlayerRef,
        roads: RoadGraphCache,
    ) -> Option<Self> {
        NavView::of_object(&server, &WorldObject::from(player.clone()))?;
        Some(Self {
            server,
            player,
            roads,
        })
    }

    fn view(&self) -> Option<NavView<'_>> {
        NavView::of_object(&self.server, &WorldObject::from(self.player.clone()))
    }
}

impl NavWorld for OwnedGameNav {
    fn height(&self, x: f32, y: f32) -> Option<f32> {
        self.view()?.height(x, y)
    }

    fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        self.view()
            .map_or(0.0, |view| view.water_depth(x, y, ground))
    }

    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?.nav_route(from, to)
    }

    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?.road_route(from, to)
    }

    fn road_route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?
            .road_route_budget(from, to, budget)
    }

    fn road_route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut PlanBudget,
    ) -> Option<Vec<Vec3>> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?
            .road_route_avoiding(from, to, banned, budget)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?.obstacle(from, to)
    }

    fn jump_clear(&self, from: Vec3, to: Vec3) -> Option<bool> {
        GameNav::of_player(&self.server, &self.player, Some(&self.roads))?.jump_clear(from, to)
    }

    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.view()?.zone_at(x, y)
    }
}

/// The recovery ladder, in order. Only player-legal rungs: a re-plan, a jump / detour, a
/// backtrack along the breadcrumbs, more detours, then give up (the caller reports the
/// failure). There is no GM teleport, no recall skill/item (P2 has none) and no suicide rung;
/// the old bots' `recovery:recall` / `recovery:die` are deliberately gone.
pub const LADDER: [&str; 7] = [
    "recovery:repath",
    "recovery:jump_or_detour",
    "recovery:backtrack",
    "recovery:jump_or_detour",
    "recovery:jump_or_detour",
    "recovery:jump_or_detour",
    "recovery:abandon",
];
/// After this many rungs in total the ladder jumps to the abandon rung (a bot that keeps
/// finding "progress" in a loop must still give up).
const MAX_TOTAL_RUNGS: u32 = 12;
const ABANDON_RUNG: u8 = 6;

/// The stuck tracker. A rung is due when the bot has made no 5 m progress for 30 s, or stayed
/// at the same position for three blocked steps; after the first rung each further rung needs
/// the previous one to have had [`RUNG_WINDOW_MS`] without [`RUNG_PROGRESS_M`] of progress.
#[derive(Debug, Default)]
pub struct StuckTracker {
    best: Option<(f32, f32)>,
    best_ms: i64,
    last: Option<(f32, f32)>,
    blocked: u32,
    /// Rungs delivered in the current episode (`0` = none yet).
    delivered: u8,
    total: u32,
    rung_ms: i64,
    rung_pos: (f32, f32),
}

impl StuckTracker {
    /// Records a position; `true` when the 30 s / 5 m no-progress rule says a rung is due.
    pub fn observe(&mut self, pos: (f32, f32), now_ms: i64) -> bool {
        match self.best {
            None => {
                self.best = Some(pos);
                self.best_ms = now_ms;
                false
            }
            Some(best) => {
                if ((pos.0 - best.0).powi(2) + (pos.1 - best.1).powi(2)).sqrt() >= STUCK_PROGRESS_M
                {
                    self.best = Some(pos);
                    self.best_ms = now_ms;
                    false
                } else if now_ms - self.best_ms >= STUCK_WINDOW_MS && self.due(pos, now_ms) {
                    self.best = Some(pos);
                    self.best_ms = now_ms;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Records a blocked step; `true` when a rung is due.
    pub fn blocked(&mut self, pos: (f32, f32), now_ms: i64) -> bool {
        self.observe(pos, now_ms);
        if self.last == Some(pos) {
            self.blocked = self.blocked.saturating_add(1);
        } else {
            self.blocked = 0;
        }
        self.last = Some(pos);
        if self.blocked >= BLOCKED_STEPS && self.due(pos, now_ms) {
            self.blocked = 0;
            return true;
        }
        false
    }

    /// May the next rung be tried now?
    fn due(&mut self, pos: (f32, f32), now_ms: i64) -> bool {
        if self.delivered == 0 {
            return true;
        }
        let moved = ((pos.0 - self.rung_pos.0).powi(2) + (pos.1 - self.rung_pos.1).powi(2)).sqrt();
        if moved >= RUNG_PROGRESS_M {
            // The last rung got the bot moving: whatever blocks it now is a new problem.
            self.delivered = 0;
            return true;
        }
        now_ms - self.rung_ms >= RUNG_WINDOW_MS
    }

    /// The next recovery rung as a trace label; call after `observe` / `blocked` returned `true`.
    pub fn rung(&mut self, pos: (f32, f32), now_ms: i64) -> Option<&'static str> {
        self.total += 1;
        if self.total > MAX_TOTAL_RUNGS {
            // A drop / jump resets the episode but not `total`: the cap is absolute.
            if self.delivered > ABANDON_RUNG {
                return None;
            }
            self.delivered = ABANDON_RUNG + 1;
            self.rung_ms = now_ms;
            self.rung_pos = pos;
            return LADDER.get(usize::from(ABANDON_RUNG)).copied();
        }
        let label = *LADDER.get(usize::from(self.delivered))?;
        self.delivered += 1;
        self.rung_ms = now_ms;
        self.rung_pos = pos;
        Some(label)
    }

    /// Starts a new stuck episode (a drop / jump ended it) but keeps the total rung count:
    /// otherwise a bot that alternates drops and blocks bypasses [`MAX_TOTAL_RUNGS`].
    pub fn reset(&mut self) {
        let total = self.total;
        *self = Self::default();
        self.total = total;
    }
}

/// The next step of a plan, with obstacle checks and the default speeds.
pub fn next_step(world: &dyn NavWorld, from: Vec3, target: Vec3) -> Option<MoveStep> {
    next_step_with_speeds(world, from, target, RUN_SPEED_MPS, SWIM_SPEED_MPS)
}

/// [`next_step`] with the unit's actual run / swim speed (slows, trade packs, buffs): a swim
/// step is `swim_speed * STEP_MS` long, exactly like a real client's.
pub fn next_step_with_speeds(
    world: &dyn NavWorld,
    from: Vec3,
    target: Vec3,
    run_speed: f32,
    swim_speed: f32,
) -> Option<MoveStep> {
    let dist = dist2(from, target);
    if dist <= STOP_DISTANCE {
        return Some(MoveStep {
            to: target,
            action: StepAction::Run,
        });
    }
    let ground = world.height(from.x, from.y).unwrap_or(from.z);
    let in_water = world.water_depth(from.x, from.y, ground) > SWIM_DEPTH_M;
    let speed = if in_water { swim_speed } else { run_speed };
    let step_len = (speed * STEP_MS as f32 / 1000.0).min(dist);
    let nx = from.x + (target.x - from.x) / dist * step_len;
    let ny = from.y + (target.y - from.y) / dist * step_len;
    let nz = world
        .height(nx, ny)
        .unwrap_or_else(|| from.z + (target.z - from.z) * (step_len / dist));
    let to = Vec3::new(nx, ny, nz);
    let dy = to.z - from.z;
    let horizontal = dist2(from, to);
    if let Some(obstacle) = world.obstacle(from, to) {
        return match obstacle {
            Obstacle::Low(h) if h <= JUMP_HEIGHT_M => {
                let jump_len = (run_speed * JUMP_AIR_TIME_S).min(dist);
                let jx = from.x + (target.x - from.x) / dist * jump_len;
                let jy = from.y + (target.y - from.y) / dist * jump_len;
                jump_step(world, from, Vec3::new(jx, jy, 0.0), h)
            }
            _ => None,
        };
    }
    let water = world.water_depth(to.x, to.y, to.z);
    if water > SWIM_DEPTH_M {
        let level = to.z + water;
        return Some(MoveStep {
            to: Vec3::new(to.x, to.y, level - SWIM_DEPTH_M),
            action: StepAction::Swim,
        });
    }
    if dy > STEP_HEIGHT_M {
        if dy <= JUMP_HEIGHT_M && horizontal <= run_speed * JUMP_AIR_TIME_S {
            return jump_step(world, from, to, dy);
        }
        return None;
    }
    if dy < -STEP_HEIGHT_M {
        if -dy <= MAX_SAFE_DROP_M {
            return Some(MoveStep {
                to,
                action: StepAction::Drop(-dy),
            });
        }
        return None;
    }
    Some(MoveStep {
        to,
        action: StepAction::Run,
    })
}

/// A jump step only when the landing has ground, is not deep water, is not a drop beyond the
/// safe fall and the whole arc is free. The landing height is the terrain at the landing
/// point (the arc interpolates to it, so it never ends at the take-off height).
fn jump_step(world: &dyn NavWorld, from: Vec3, landing: Vec3, height: f32) -> Option<MoveStep> {
    let jz = world.height(landing.x, landing.y)?;
    let to = Vec3::new(landing.x, landing.y, jz);
    if from.z - jz > MAX_SAFE_DROP_M {
        return None;
    }
    if world.water_depth(to.x, to.y, to.z) > SWIM_DEPTH_M {
        return None;
    }
    if world.jump_clear(from, to) == Some(false) {
        return None;
    }
    Some(MoveStep {
        to,
        action: StepAction::Jump(height),
    })
}

/// Heading offsets (degrees) a walker tries, in order, when the straight way is blocked: it
/// slides around a trunk, a rock or a corner like a player steering past it.
const STEER_DEGREES: [f32; 4] = [30.0, -30.0, 70.0, -70.0];

/// [`next_step`], and when the way straight to `target` is blocked the first step that works
/// at a different heading (`bias` = last successful side, `1` or `-1`, tried first).
pub fn next_step_steered(
    world: &dyn NavWorld,
    from: Vec3,
    target: Vec3,
    bias: &mut i8,
    run_speed: f32,
    swim_speed: f32,
) -> Option<MoveStep> {
    if let Some(step) = next_step_with_speeds(world, from, target, run_speed, swim_speed) {
        return Some(step);
    }
    let dist = dist2(from, target);
    if dist <= STOP_DISTANCE {
        return None;
    }
    let base = (target.y - from.y).atan2(target.x - from.x);
    let reach = dist.min(run_speed * JUMP_AIR_TIME_S);
    for degrees in STEER_DEGREES {
        let signed = degrees * f32::from(*bias);
        let a = base + signed.to_radians();
        let (x, y) = (from.x + reach * a.cos(), from.y + reach * a.sin());
        let z = world.height(x, y).unwrap_or(from.z);
        if let Some(step) =
            next_step_with_speeds(world, from, Vec3::new(x, y, z), run_speed, swim_speed)
        {
            *bias = if signed >= 0.0 { 1 } else { -1 };
            return Some(step);
        }
    }
    None
}

//! The bot walker: a real-client-like route follower on top of [`super`].
//!
//! Movement packets are the same `CSMoveUnitPacket` a client sends, dispatched through the
//! bot's [`SocketlessConnection`](../host/session.rs), so the server's `CSMoveUnitPacket`
//! handler (`world::session::move_unit`) applies them exactly as it does a player's: observers
//! see running, jumping (actor flag 0x10), falling (0x80 + `fallVel`) and swimming (stance
//! `SWIM`), and the collision service's movement verifier sees the same sample stream. Step
//! length and packet velocity use the unit's current `MoveSpeedMul` / `SwimSpeedMul` (slows,
//! trade packs, buffs), like a real client. Route planning and the steering probes run in
//! `spawn_blocking` under a bounded [`PlanBudget`]; the server never refuses a move
//! (`world::session::move_unit` sets the position unconditionally), so `refused` only fires in
//! tests or after an external position change.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use aaemu_game::models::movement::{
    GameStanceType, MoveKind, MoveType, MoveTypeActorFlags, MoveTypeAlertness, MoveTypeFlags,
    UnitMove,
};
use aaemu_game::packets::c2g_world::CSMoveUnitPacket;
use aaemu_game::systems::skills::controllers::{roll_pitch_yaw_sbytes_movement, Vec3};
use aaemu_game::world::{PlayerRef, WorldObject};
use aaemu_game::GameServer;
use tokio::sync::oneshot;

use crate::host::{BotSession, WalkSend};

use super::{
    self as nav, Avoid, MoveStep, NavWorld, OwnedGameNav, PlanCancel, StepAction, StuckTracker,
    GOAL_TOLERANCE_M, JUMP_AIR_TIME_S, JUMP_GRAVITY, JUMP_SPEED_MPS, RUN_SPEED_MPS, STEP_MS,
    SWIM_SPEED_MPS,
};

/// Close enough to the target to stop when the caller asks for no radius.
pub const STOP_DISTANCE: f32 = nav::STOP_DISTANCE;
/// How far back the breadcrumb trail reaches.
const BREADCRUMBS: usize = 120;
/// A step that ends this far from where the walker sent the bot was refused by the server.
///
/// The server never refuses a move: `world::session::move_unit` sets the position
/// unconditionally, so this only fires in tests (or when another system moved the bot between
/// the packet and the next tick).
const REJECTED_MOVE_M: f32 = 1.0;
/// How long a refused step stands before the probes run again, in milliseconds (the walker
/// ticks every 100 ms; the stuck tracker still counts every tick).
const BLOCKED_CACHE_MS: i64 = 500;
/// A detour fan step, in metres.
const DETOUR_M: f32 = 8.0;
/// Plain replans of the same route before the walk gives up.
const MAX_REPLANS: u32 = 3;
/// Radius of the circles the repath rung routes around.
const AVOID_RADIUS_M: f32 = 3.0;
/// A walk may take this many times what the rest of its route needs at the bot's current speed
/// ...
const DEADLINE_ROUTE_FACTOR: f32 = 3.0;
/// ... plus this much slack, in seconds.
const DEADLINE_SLACK_S: f32 = 30.0;
/// The speed (m/s) below which the allowance stops growing (a rooted bot does not get an
/// infinite one).
const DEADLINE_MIN_SPEED: f32 = 0.5;
/// A sanity ceiling of one allowance (a day) that keeps the `Duration` conversion finite; no
/// route needs anything near it.
const MAX_ALLOWANCE_S: f32 = 86_400.0;
/// Metres closer along its route that count as progress (they extend the allowance).
const PROGRESS_M: f32 = 5.0;
/// A walk whose remaining route did not shrink by [`PROGRESS_M`] for this long is given up,
/// however much of its allowance is left.
const NO_PROGRESS_TIMEOUT_S: f32 = 180.0;
/// A position change this large that the walker's own packets did not make is a server
/// teleport, not movement. The walker's packets are excluded by comparing against where the
/// bot stood right after its last packet: a safe drop (up to `MAX_SAFE_DROP_M`, 9.5 m) moves
/// the bot further than this in one step.
const MAX_TELEPORT_STEP_M: f32 = 8.0;

/// Why a walk ended without arriving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavFailure {
    /// No planner produced a route.
    Unreachable(String),
    /// The player-legal recovery ladder ran out.
    Stuck(String),
    /// The walk ran out of its route-scaled allowance or stopped getting closer
    /// ([`WalkClock`]).
    Timeout,
    /// A newer `goto` replaced this walk, or the bot left the world.
    Interrupted(&'static str),
}

impl std::fmt::Display for NavFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(reason) => write!(f, "unreachable: {reason}"),
            Self::Stuck(reason) => write!(f, "stuck: {reason}"),
            Self::Timeout => write!(f, "timeout"),
            Self::Interrupted(reason) => write!(f, "interrupted: {reason}"),
        }
    }
}

/// `Arrived | Failed(reason)`, the API of [`BotSession::goto`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavOutcome {
    Arrived,
    Failed(NavFailure),
}

impl NavOutcome {
    pub fn is_arrived(&self) -> bool {
        matches!(self, Self::Arrived)
    }
}

impl std::fmt::Display for NavOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Arrived => f.write_str("arrived"),
            Self::Failed(reason) => write!(f, "failed: {reason}"),
        }
    }
}

/// A running `goto`: await [`Goto::wait`] for the outcome.
pub struct Goto {
    rx: oneshot::Receiver<NavOutcome>,
}

impl Goto {
    /// Waits for the walk to arrive or fail.
    pub async fn wait(self) -> NavOutcome {
        self.rx
            .await
            .unwrap_or(NavOutcome::Failed(NavFailure::Interrupted(
                "navigation task ended",
            )))
    }
}

impl BotSession {
    /// Walks to `target`, stopping within `radius` metres: `Arrived` or `Failed(reason)`.
    /// A second `goto` cancels the first (its future resolves `Interrupted`).
    pub async fn goto(
        self: &Arc<Self>,
        server: &Arc<GameServer>,
        target: Vec3,
        radius: f32,
    ) -> NavOutcome {
        self.start_goto(server, target, radius).wait().await
    }

    /// Starts [`BotSession::goto`] without waiting (the operator command).
    pub fn start_goto(
        self: &Arc<Self>,
        server: &Arc<GameServer>,
        target: Vec3,
        radius: f32,
    ) -> Goto {
        let (tx, rx) = oneshot::channel();
        let generation = self.bump_nav_generation();
        let bot = self.clone();
        let server = server.clone();
        tokio::spawn(async move {
            let outcome = walk(&bot, &server, target, radius, generation).await;
            let at = bot.position().await;
            tracing::info!(
                "Bots: '{}' goto ({:.1}, {:.1}, {:.1}) r={radius:.1}: {:?} at {:?}",
                bot.name,
                target.x,
                target.y,
                target.z,
                outcome,
                at
            );
            let _ = tx.send(outcome);
        });
        Goto { rx }
    }
}

/// What the pure [`RouteWalker`] wants the caller to do next.
#[derive(Debug, Clone, PartialEq)]
pub enum WalkAction {
    /// The bot is within the arrival radius.
    Arrived,
    /// Send this movement packet.
    Step(MoveStep),
    /// Nothing to do this tick (a rung just ran, or the step is blocked but not yet stuck).
    Wait,
    /// Produce a new plan from the current position (`avoid` = the circles to route around
    /// when this is the repath rung).
    Replan {
        avoid: Option<(f32, f32)>,
        reason: &'static str,
    },
    /// Give up.
    Failed(NavFailure),
}

/// The state machine of one walk, free of I/O so the recovery sequences can be tested without
/// a server: it decides the next step from the server's position, the plan and the stuck
/// tracker.
pub struct RouteWalker {
    world: Arc<dyn NavWorld + Send + Sync>,
    final_target: Vec3,
    target: Vec3,
    radius: f32,
    plan: Vec<Vec3>,
    index: usize,
    breadcrumbs: VecDeque<Vec3>,
    detour_index: usize,
    steer_bias: i8,
    retrace: bool,
    replans: u32,
    tracker: StuckTracker,
    /// The unit's current speeds (m/s), read from its attributes every tick.
    run_speed: f32,
    swim_speed: f32,
    /// The last steering result for the same position and waypoint: the probes (capsule
    /// sweeps, water) run at most once per tick.
    last_step: Option<(Vec3, Vec3, MoveStep)>,
    /// The position / waypoint a step was refused at, and until when the refusal stands: a
    /// blocked walker does not repeat the (capsule sweeping) probes every tick while it waits
    /// for the stuck ladder.
    last_blocked: Option<(Vec3, Vec3, i64)>,
    /// Every rung delivered in this walk (diagnostics / traces).
    pub rungs: Vec<&'static str>,
}

impl RouteWalker {
    pub fn new(
        world: Arc<dyn NavWorld + Send + Sync>,
        plan: Vec<Vec3>,
        final_target: Vec3,
        radius: f32,
    ) -> Self {
        Self {
            world,
            final_target,
            target: final_target,
            radius,
            plan,
            index: 0,
            breadcrumbs: VecDeque::new(),
            detour_index: 0,
            steer_bias: 1,
            retrace: false,
            replans: 0,
            tracker: StuckTracker::default(),
            run_speed: RUN_SPEED_MPS,
            swim_speed: nav::SWIM_SPEED_MPS,
            last_step: None,
            last_blocked: None,
            rungs: Vec::new(),
        }
    }

    /// The unit's actual run / swim speeds for the next step.
    pub fn set_speeds(&mut self, run_speed: f32, swim_speed: f32) {
        if (self.run_speed - run_speed).abs() > f32::EPSILON
            || (self.swim_speed - swim_speed).abs() > f32::EPSILON
        {
            self.last_step = None;
            self.last_blocked = None;
        }
        self.run_speed = run_speed.max(0.1);
        self.swim_speed = swim_speed.max(0.1);
    }

    /// Replaces the plan; `recovery` plans do not count towards [`MAX_REPLANS`].
    pub fn set_plan(&mut self, plan: Vec<Vec3>, recovery: bool) {
        if !recovery {
            self.replans += 1;
        }
        self.plan = plan;
        self.index = 0;
        self.retrace = false;
        self.last_step = None;
        self.last_blocked = None;
    }

    pub fn plan_len(&self) -> usize {
        self.plan.len()
    }

    /// The length of the route still ahead of `pos`: to the waypoint the walker is heading for
    /// and along the rest of the plan; the straight way to the target once the plan is used up.
    pub fn remaining_len(&self, pos: Vec3) -> f32 {
        let Some(next) = self.plan.get(self.index) else {
            return dist3(pos, self.final_target);
        };
        let mut len = dist3(pos, *next);
        for pair in self.plan[self.index..].windows(2) {
            len += dist3(pair[0], pair[1]);
        }
        len
    }

    /// Decides the next action from the server's position `pos` at `now_ms`.
    pub fn next(&mut self, pos: Vec3, now_ms: i64) -> WalkAction {
        if self.tracker.observe((pos.x, pos.y), now_ms) {
            return self.deliver_rung(pos, now_ms);
        }
        loop {
            if self.index >= self.plan.len() {
                self.retrace = false;
                if self.arrived(pos) {
                    return WalkAction::Arrived;
                }
                if self.replans >= MAX_REPLANS {
                    return WalkAction::Failed(NavFailure::Stuck(
                        "the route kept ending short of the target".into(),
                    ));
                }
                return WalkAction::Replan {
                    avoid: None,
                    reason: "plan exhausted",
                };
            }
            let waypoint = self.plan[self.index];
            if dist2(pos, waypoint) <= STOP_DISTANCE {
                self.index += 1;
                continue;
            }
            if let Some((cached_pos, cached_waypoint, step)) = self.last_step {
                if dist2(cached_pos, pos) <= 0.01 && dist2(cached_waypoint, waypoint) <= 0.01 {
                    return WalkAction::Step(step);
                }
            }
            if let Some((cached_pos, cached_waypoint, until)) = self.last_blocked {
                if now_ms < until
                    && dist2(cached_pos, pos) <= 0.01
                    && dist2(cached_waypoint, waypoint) <= 0.01
                {
                    return self.blocked_action(pos, now_ms);
                }
            }
            let retraced = nav::Retrace(self.world.as_ref());
            let world: &dyn NavWorld = if self.retrace {
                &retraced
            } else {
                self.world.as_ref()
            };
            let step = match nav::next_step_steered_explained(
                world,
                pos,
                waypoint,
                &mut self.steer_bias,
                self.run_speed,
                self.swim_speed,
            ) {
                Ok(step) => step,
                Err(why) => {
                    tracing::debug!(
                        "Bots: blocked at ({:.1}, {:.1}, {:.1}) heading for ({:.1}, {:.1}, {:.1}): {why}",
                        pos.x,
                        pos.y,
                        pos.z,
                        waypoint.x,
                        waypoint.y,
                        waypoint.z
                    );
                    self.last_blocked = Some((pos, waypoint, now_ms + BLOCKED_CACHE_MS));
                    return self.blocked_action(pos, now_ms);
                }
            };
            self.last_step = Some((pos, waypoint, step));
            if self.breadcrumbs.back().is_none_or(|p| dist2(*p, pos) > 1.0) {
                self.breadcrumbs.push_back(pos);
                if self.breadcrumbs.len() > BREADCRUMBS {
                    self.breadcrumbs.pop_front();
                }
            }
            return WalkAction::Step(step);
        }
    }

    /// Records that the server refused the move to `pos` (the bot did not move as sent).
    /// Production servers do not refuse moves (`world::session::move_unit` always sets the
    /// position); this path is exercised by the tests and by external position changes.
    pub fn refused(&mut self, pos: Vec3, now_ms: i64) -> Option<WalkAction> {
        if self.tracker.blocked((pos.x, pos.y), now_ms) {
            return Some(self.deliver_rung(pos, now_ms));
        }
        None
    }

    /// A drop ended a fall: the tracker starts a new episode.
    pub fn reset_stuck(&mut self) {
        self.tracker.reset();
    }

    fn arrived(&self, pos: Vec3) -> bool {
        let close = self.radius.max(STOP_DISTANCE);
        dist2(pos, self.final_target) <= close
            || (self.index >= self.plan.len()
                && dist2(pos, self.final_target) <= GOAL_TOLERANCE_M + close)
    }

    /// A blocked tick: the stuck tracker counts it and either hands out a rung or waits.
    fn blocked_action(&mut self, pos: Vec3, now_ms: i64) -> WalkAction {
        if self.tracker.blocked((pos.x, pos.y), now_ms) {
            return self.deliver_rung(pos, now_ms);
        }
        WalkAction::Wait
    }

    fn deliver_rung(&mut self, pos: Vec3, now_ms: i64) -> WalkAction {
        self.last_step = None;
        self.last_blocked = None;
        let Some(rung) = self.tracker.rung((pos.x, pos.y), now_ms) else {
            return WalkAction::Failed(NavFailure::Stuck(
                "the recovery ladder had no rung left".into(),
            ));
        };
        self.rungs.push(rung);
        match rung {
            "recovery:repath" => WalkAction::Replan {
                avoid: Some((pos.x, pos.y)),
                reason: rung,
            },
            "recovery:jump_or_detour" => {
                let angles = [20.0f32, -20.0, 40.0, -40.0];
                let angle = angles[self.detour_index % angles.len()].to_radians();
                self.detour_index += 1;
                let (dx, dy) = (self.final_target.x - pos.x, self.final_target.y - pos.y);
                let want = dy.atan2(dx) + angle;
                let (x, y) = (pos.x + DETOUR_M * want.cos(), pos.y + DETOUR_M * want.sin());
                let z = self.world.height(x, y).unwrap_or(pos.z);
                self.target = Vec3::new(x, y, z);
                self.plan = vec![self.target];
                self.index = 0;
                self.retrace = false;
                WalkAction::Wait
            }
            "recovery:backtrack" => {
                // Retrace the trail the bot came by, back to a spot at least 6 m away: ground
                // it stood on is passable even when the capsule sweeps call every direction
                // closed (a pocket between two rocks it slid into).
                let mut trail: Vec<Vec3> = Vec::new();
                for crumb in self.breadcrumbs.iter().rev() {
                    trail.push(*crumb);
                    if dist2(*crumb, pos) > 6.0 {
                        break;
                    }
                }
                if trail.last().is_some_and(|c| dist2(*c, pos) > 6.0) {
                    self.plan = trail;
                } else {
                    // No trail (the bot was placed here): step out toward the goal without the
                    // sweeps, which call a pocket closed on every side.
                    let d = dist2(pos, self.final_target).max(0.001);
                    let k = (DETOUR_M / d).min(1.0);
                    let (x, y) = (
                        pos.x + (self.final_target.x - pos.x) * k,
                        pos.y + (self.final_target.y - pos.y) * k,
                    );
                    self.plan = vec![Vec3::new(x, y, self.world.height(x, y).unwrap_or(pos.z))];
                }
                self.target = self.plan.last().copied().unwrap_or(self.final_target);
                self.index = 0;
                self.retrace = true;
                WalkAction::Wait
            }
            "recovery:abandon" => WalkAction::Failed(NavFailure::Stuck(
                "every alternative route ended stuck".into(),
            )),
            _ => WalkAction::Wait,
        }
    }
}

/// One jump in flight: airborne samples, then the landing.
struct JumpState {
    from: Vec3,
    to: Vec3,
    elapsed: f32,
    /// The `fallVel` of the landing packet: the fall from the jump's apex to the landing
    /// ([`nav::jump_fall_height`]; a jump rises before it falls). Like a real client, the walker
    /// reports it in the packet of the touchdown only, never while the bot is still in the air.
    landing_fall_vel: u16,
}

/// What the walker sends for one tick of a jump.
#[derive(Debug, Clone, Copy, PartialEq)]
enum JumpTick {
    /// An airborne sample (no `fallVel`).
    Air {
        at: Vec3,
        delta: (f32, f32),
        vel_z: f32,
    },
    /// The touchdown at the landing point, carrying the landing's `fallVel`.
    Landing { fall_vel: u16 },
}

impl JumpState {
    fn new(from: Vec3, to: Vec3) -> Self {
        Self {
            from,
            to,
            elapsed: 0.0,
            landing_fall_vel: aaemu_game::systems::ai::nav::fall_velocity(nav::jump_fall_height(
                from.z, to.z,
            )),
        }
    }

    /// Advances the flight by one [`STEP_MS`] tick.
    fn tick(&mut self) -> JumpTick {
        self.elapsed += STEP_MS as f32 / 1000.0;
        if self.elapsed >= JUMP_AIR_TIME_S {
            return JumpTick::Landing {
                fall_vel: self.landing_fall_vel,
            };
        }
        JumpTick::Air {
            at: self.position(),
            delta: self.step_delta(),
            vel_z: self.vertical_velocity(),
        }
    }

    fn position(&self) -> Vec3 {
        nav::jump_arc(self.from, self.to, self.elapsed)
    }

    /// The horizontal displacement of one [`STEP_MS`] tick of the flight: the packet velocity of a
    /// jump is its real horizontal speed (a short jump is slow), not the running speed.
    fn step_delta(&self) -> (f32, f32) {
        let (dx, dy) = delta2(self.from, self.to);
        let per_step = STEP_MS as f32 / 1000.0 / JUMP_AIR_TIME_S;
        (dx * per_step, dy * per_step)
    }

    fn vertical_velocity(&self) -> f32 {
        // Derivative of the arc: the parabola plus the landing-height interpolation.
        let dz = self.to.z - self.from.z;
        let climb = if JUMP_AIR_TIME_S > 0.0 {
            dz / JUMP_AIR_TIME_S
        } else {
            0.0
        };
        JUMP_SPEED_MPS + climb - JUMP_GRAVITY * self.elapsed
    }
}

/// The walk task: plan in `spawn_blocking`, then send a real `CSMoveUnitPacket` every
/// [`STEP_MS`] until arrival / failure. Only one walk per bot: the generation check stops an
/// older task after a new `goto`.
async fn walk(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    target: Vec3,
    radius: f32,
    generation: u64,
) -> NavOutcome {
    let Some(player) = bot.player().await else {
        return NavOutcome::Failed(NavFailure::Interrupted("not in the world"));
    };
    let start = position(&player);
    let instance_id = player.data().unit.transform.instance_id;
    let Some(owned) = OwnedGameNav::of_player(server.clone(), player.clone(), bot.road_cache())
    else {
        return NavOutcome::Failed(NavFailure::Unreachable(
            "the bot's world instance is unknown".into(),
        ));
    };
    let world: Arc<dyn NavWorld + Send + Sync> = Arc::new(owned);
    // The height under a far target may load a heightmap cell from the client files: not on an
    // async worker.
    let target_z = {
        let world = world.clone();
        let (x, y, fallback) = (target.x, target.y, start.z);
        tokio::task::spawn_blocking(move || world.height(x, y).unwrap_or(fallback))
            .await
            .unwrap_or(start.z)
    };
    let final_target = Vec3::new(target.x, target.y, target_z);
    let (run_speed, swim_speed) = speeds(&player);
    let plan = match plan_blocking(
        server,
        &player,
        bot,
        start,
        final_target,
        Vec::new(),
        bot.nav_cancel(generation),
    )
    .await
    {
        Ok(points) => points,
        Err(reason) => {
            if bot.nav_generation() != generation {
                return NavOutcome::Failed(NavFailure::Interrupted(bot.interrupt_reason()));
            }
            return NavOutcome::Failed(NavFailure::Unreachable(reason));
        }
    };
    let mut walker = RouteWalker::new(world, plan, final_target, radius);
    walker.set_speeds(run_speed, swim_speed);
    let started = tokio::time::Instant::now();
    let mut clock = WalkClock::new(
        walker.remaining_len(start),
        run_speed,
        started,
        bot.walk_time_cap().map(|cap| started + cap),
    );
    let mut jump: Option<JumpState> = None;
    let mut last_pos = start;
    let mut tick = tokio::time::interval(Duration::from_millis(STEP_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Some(failure) = interruption(bot, &player, generation, instance_id) {
            return NavOutcome::Failed(failure);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let (run_speed, swim_speed) = speeds(&player);
        let pos = position(&player);
        if dist3(pos, last_pos) > MAX_TELEPORT_STEP_M {
            return NavOutcome::Failed(NavFailure::Interrupted("the bot was teleported"));
        }
        last_pos = pos;
        if let Some(limit) = clock.check(
            walker.remaining_len(pos),
            run_speed,
            tokio::time::Instant::now(),
        ) {
            tracing::info!(
                "Bots: '{}' walk timed out ({limit:?}) at ({:.1}, {:.1}, {:.1}), {:.0} m of route left",
                bot.name,
                pos.x,
                pos.y,
                pos.z,
                walker.remaining_len(pos)
            );
            let outcome = NavOutcome::Failed(NavFailure::Timeout);
            return stop_and_end(bot, server, &player, generation, pos, final_target, outcome)
                .await;
        }
        walker.set_speeds(run_speed, swim_speed);
        if let Some(state) = jump.as_mut() {
            let (from, to) = (state.from, state.to);
            let sent = match state.tick() {
                JumpTick::Air { at, delta, vel_z } => {
                    let (_, _, rotation_z) =
                        roll_pitch_yaw_sbytes_movement(0.0, 0.0, heading(from, to));
                    send_move(
                        bot,
                        server,
                        &player,
                        generation,
                        at,
                        delta,
                        vel_z,
                        rotation_z,
                        GameStanceType::COMBAT,
                        MoveTypeFlags::MOVING | MoveTypeFlags::JUMPING,
                        MoveTypeActorFlags(
                            MoveTypeActorFlags::JUMPING.0 | MoveTypeActorFlags::FLAG2.0,
                        ),
                        0,
                        run_speed,
                    )
                    .await
                }
                JumpTick::Landing { fall_vel } => {
                    jump = None;
                    send_stop(
                        bot,
                        server,
                        &player,
                        generation,
                        to,
                        heading(pos, to),
                        fall_vel,
                    )
                    .await
                }
            };
            if let Err(failure) = sent {
                return NavOutcome::Failed(failure);
            }
            last_pos = position(&player);
            continue;
        }
        // The steering probes (capsule sweeps / water) may load collision geometry, so they
        // run on a blocking worker; the walker moves there and back.
        let (walker_back, action) = tokio::task::spawn_blocking(move || {
            let mut walker = walker;
            let action = walker.next(pos, now);
            (walker, action)
        })
        .await
        .expect("the navigation step task panicked");
        walker = walker_back;
        #[cfg(test)]
        bot.pause_before_send().await;
        // The step task took a while: a cancel, a newer goto or a logout may have come in
        // meanwhile, and a stale walk sends nothing (the connection re-checks the generation
        // and the online state under its lock once more).
        if let Some(failure) = interruption(bot, &player, generation, instance_id) {
            return NavOutcome::Failed(failure);
        }
        match action {
            WalkAction::Arrived => {
                return stop_and_end(
                    bot,
                    server,
                    &player,
                    generation,
                    pos,
                    final_target,
                    NavOutcome::Arrived,
                )
                .await;
            }
            WalkAction::Wait => continue,
            WalkAction::Failed(failure) => {
                let outcome = NavOutcome::Failed(failure);
                return stop_and_end(bot, server, &player, generation, pos, final_target, outcome)
                    .await;
            }
            WalkAction::Replan { avoid, reason } => {
                if let Some(failure) = replan(
                    bot,
                    server,
                    &player,
                    &mut walker,
                    pos,
                    final_target,
                    avoid,
                    reason,
                    generation,
                )
                .await
                {
                    return NavOutcome::Failed(failure);
                }
                // A new route: the allowance is derived from what is left of it.
                clock.replanned(
                    walker.remaining_len(pos),
                    run_speed,
                    tokio::time::Instant::now(),
                );
            }
            WalkAction::Step(step) => {
                if matches!(step.action, StepAction::Drop(_)) {
                    // The drop is a controlled fall, not a stuck episode.
                    walker.reset_stuck();
                }
                let sent = match send_step(
                    bot, server, &player, generation, pos, step, &mut jump, run_speed, swim_speed,
                )
                .await
                {
                    Ok(sent) => sent,
                    Err(failure) => return NavOutcome::Failed(failure),
                };
                // Where the bot stands after its own packet is the baseline of the next
                // tick's teleport check (a drop moves it far; that is not a teleport).
                last_pos = position(&player);
                if sent && dist2(position(&player), step.to) > REJECTED_MOVE_M {
                    let actual = position(&player);
                    let now = chrono::Utc::now().timestamp_millis();
                    if let Some(action) = walker.refused(actual, now) {
                        match action {
                            WalkAction::Failed(failure) => {
                                return NavOutcome::Failed(failure);
                            }
                            WalkAction::Replan { avoid, reason } => {
                                if let Some(failure) = replan(
                                    bot,
                                    server,
                                    &player,
                                    &mut walker,
                                    actual,
                                    final_target,
                                    avoid,
                                    reason,
                                    generation,
                                )
                                .await
                                {
                                    return NavOutcome::Failed(failure);
                                }
                                clock.replanned(
                                    walker.remaining_len(actual),
                                    run_speed,
                                    tokio::time::Instant::now(),
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

/// Plans a new route from `pos` (the repath rung routes around the stuck circles) and hands it
/// to the walker. `Some(failure)` ends the walk.
#[allow(clippy::too_many_arguments)]
async fn replan(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    player: &PlayerRef,
    walker: &mut RouteWalker,
    pos: Vec3,
    final_target: Vec3,
    avoid: Option<(f32, f32)>,
    reason: &str,
    generation: u64,
) -> Option<NavFailure> {
    let mut spots = Vec::new();
    if let Some(spot) = avoid {
        spots.push(spot);
        spots.extend(bot.stuck_spots().await);
    }
    match plan_blocking(
        server,
        player,
        bot,
        pos,
        final_target,
        spots,
        bot.nav_cancel(generation),
    )
    .await
    {
        Ok(points) => {
            tracing::debug!("Bots: '{}' {reason}: {} waypoints", bot.name, points.len());
            walker.set_plan(points, avoid.is_some());
            None
        }
        Err(why) => {
            if bot.nav_generation() != generation {
                return Some(NavFailure::Interrupted(bot.interrupt_reason()));
            }
            if let Some(spot) = avoid {
                // The repath rung must not end the walk: keep the straight target so the next
                // rung gets its window (the recovery ladder, not a hard failure).
                bot.record_stuck_spot(spot).await;
                walker.set_plan(vec![final_target], true);
                tracing::debug!("Bots: '{}' {reason} found no route: {why}", bot.name);
                None
            } else {
                Some(NavFailure::Unreachable(why))
            }
        }
    }
}

/// Sends the packet of one [`MoveStep`]: `Ok(true)` when a movement packet was sent (a jump
/// starts its own tick loop instead and gives `Ok(false)`), `Err` when the walk went stale and
/// nothing was sent.
#[allow(clippy::too_many_arguments)]
async fn send_step(
    bot: &Arc<BotSession>,
    server: &Arc<GameServer>,
    player: &PlayerRef,
    generation: u64,
    pos: Vec3,
    step: MoveStep,
    jump: &mut Option<JumpState>,
    run_speed: f32,
    swim_speed: f32,
) -> Result<bool, NavFailure> {
    match step.action {
        StepAction::Run => {
            let (_, _, rotation_z) =
                roll_pitch_yaw_sbytes_movement(0.0, 0.0, heading(pos, step.to));
            send_move(
                bot,
                server,
                player,
                generation,
                step.to,
                delta2(pos, step.to),
                0.0,
                rotation_z,
                GameStanceType::COMBAT,
                MoveTypeFlags::MOVING,
                MoveTypeActorFlags(
                    MoveTypeActorFlags::STANDING_ON_SOLID.0 | MoveTypeActorFlags::FLAG2.0,
                ),
                0,
                run_speed,
            )
            .await?;
        }
        StepAction::Swim => {
            let (_, _, rotation_z) =
                roll_pitch_yaw_sbytes_movement(0.0, 0.0, heading(pos, step.to));
            send_move(
                bot,
                server,
                player,
                generation,
                step.to,
                delta2(pos, step.to),
                0.0,
                rotation_z,
                GameStanceType::SWIM,
                MoveTypeFlags::MOVING,
                MoveTypeActorFlags::NONE,
                0,
                swim_speed,
            )
            .await?;
        }
        StepAction::Jump(_) => {
            tracing::debug!(
                "Bots: '{}' jumps ({:.1}, {:.1}, {:.2}) -> ({:.1}, {:.1}, {:.2}): {:+.2} m",
                bot.name,
                pos.x,
                pos.y,
                pos.z,
                step.to.x,
                step.to.y,
                step.to.z,
                step.to.z - pos.z
            );
            *jump = Some(JumpState::new(pos, step.to));
            return Ok(false);
        }
        StepAction::Drop(height) => {
            tracing::debug!(
                "Bots: '{}' drops {height:.2} m at ({:.1}, {:.1}, {:.2})",
                bot.name,
                pos.x,
                pos.y,
                pos.z
            );
            let (_, _, rotation_z) =
                roll_pitch_yaw_sbytes_movement(0.0, 0.0, heading(pos, step.to));
            send_move(
                bot,
                server,
                player,
                generation,
                step.to,
                delta2(pos, step.to),
                0.0,
                rotation_z,
                GameStanceType::COMBAT,
                MoveTypeFlags::MOVING,
                MoveTypeActorFlags(MoveTypeActorFlags::FLAG8.0 | MoveTypeActorFlags::FLAG2.0),
                aaemu_game::systems::ai::nav::fall_velocity(height),
                run_speed,
            )
            .await?;
        }
    }
    Ok(true)
}

/// `BotSession::dispatch_walk` of one unit movement of the walk of `generation`.
#[allow(clippy::too_many_arguments)]
async fn send_move(
    bot: &BotSession,
    server: &GameServer,
    player: &PlayerRef,
    generation: u64,
    pos: Vec3,
    (dx, dy): (f32, f32),
    vel_z_mps: f32,
    rotation_z: i8,
    stance: GameStanceType,
    flags: MoveTypeFlags,
    actor_flags: MoveTypeActorFlags,
    fall_vel: u16,
    max_speed_mps: f32,
) -> Result<(), NavFailure> {
    let horizontal = (dx * dx + dy * dy).sqrt();
    // A real client's step is `actual speed * elapsed` (slows / trade packs / buffs included);
    // the verifier sees roughly this speed.
    let speed = if horizontal > 0.0 {
        (horizontal / (STEP_MS as f32 / 1000.0) * 1000.0).min(max_speed_mps * 1000.0)
    } else {
        0.0
    };
    let (vel_x, vel_y) = if horizontal > 0.0 {
        (
            (dx / horizontal * speed) as i16,
            (dy / horizontal * speed) as i16,
        )
    } else {
        (0, 0)
    };
    let unit = UnitMove {
        x: pos.x,
        y: pos.y,
        z: pos.z,
        vel_x,
        vel_y,
        vel_z: (vel_z_mps * 1000.0).clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16,
        rotation_z,
        stance,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags,
        fall_vel,
        ..UnitMove::default()
    };
    let mut mt = MoveType::new(MoveKind::Unit(unit));
    mt.flags = flags;
    mt.time = bot.next_move_time_ms();
    bot.dispatch_walk(
        server,
        generation,
        CSMoveUnitPacket {
            obj_id: player.obj_id,
            move_type: mt,
        },
    )
    .await
    .ok(bot)
}

/// The unit movement of a stop at `pos`; the fall velocity block is on the wire only with the
/// actor flag 0x80, so the flag goes with a non-zero `fall_vel`.
fn stop_move(pos: Vec3, rot_z: f32, fall_vel: u16) -> UnitMove {
    let (_, _, rotation_z) = roll_pitch_yaw_sbytes_movement(0.0, 0.0, rot_z);
    let fall_flag = if fall_vel > 0 {
        MoveTypeActorFlags::FLAG8.0
    } else {
        0
    };
    UnitMove {
        x: pos.x,
        y: pos.y,
        z: pos.z,
        rotation_z,
        stance: GameStanceType::COMBAT,
        alertness: MoveTypeAlertness::IDLE,
        actor_flags: MoveTypeActorFlags(
            MoveTypeActorFlags::STANDING_ON_SOLID.0 | MoveTypeActorFlags::FLAG2.0 | fall_flag,
        ),
        fall_vel,
        ..UnitMove::default()
    }
}

/// The packet that ends a movement (the `stopping` flag) at `pos`; after a jump it is the
/// landing packet and carries the landing's `fall_vel` (on the wire only with actor flag 0x80).
async fn send_stop(
    bot: &BotSession,
    server: &GameServer,
    player: &PlayerRef,
    generation: u64,
    pos: Vec3,
    rot_z: f32,
    fall_vel: u16,
) -> Result<(), NavFailure> {
    let mut mt = MoveType::new(MoveKind::Unit(stop_move(pos, rot_z, fall_vel)));
    mt.flags = MoveTypeFlags::STOPPING;
    mt.time = bot.next_move_time_ms();
    bot.dispatch_walk(
        server,
        generation,
        CSMoveUnitPacket {
            obj_id: player.obj_id,
            move_type: mt,
        },
    )
    .await
    .ok(bot)
}

/// Ends the walk with `outcome` after the bot's closing stop packet. A walk that went stale on
/// the way (cancelled or replaced, or the bot left) sends nothing and reports that instead.
async fn stop_and_end(
    bot: &BotSession,
    server: &GameServer,
    player: &PlayerRef,
    generation: u64,
    pos: Vec3,
    final_target: Vec3,
    outcome: NavOutcome,
) -> NavOutcome {
    match send_stop(
        bot,
        server,
        player,
        generation,
        pos,
        heading(pos, final_target),
        0,
    )
    .await
    {
        Ok(()) => outcome,
        Err(stale) => NavOutcome::Failed(stale),
    }
}

impl WalkSend {
    /// `Ok` when the packet went out, the matching interruption when the walk was stale.
    fn ok(self, bot: &BotSession) -> Result<(), NavFailure> {
        match self {
            WalkSend::Sent => Ok(()),
            WalkSend::Superseded => Err(NavFailure::Interrupted(bot.interrupt_reason())),
            WalkSend::Offline => Err(NavFailure::Interrupted("the bot is offline")),
        }
    }
}

/// Why the walk of `generation` must end before it sends another packet, if it must: a cancel
/// or a newer `goto`, or the bot died, left the world, is held by the server (teleport / zone
/// load), stands on another object or changed instance. Checked every tick and again after
/// every await that precedes a packet (the step task), not only at the top of the tick.
fn interruption(
    bot: &BotSession,
    player: &PlayerRef,
    generation: u64,
    instance_id: u32,
) -> Option<NavFailure> {
    if bot.nav_generation() != generation {
        return Some(NavFailure::Interrupted(bot.interrupt_reason()));
    }
    let (hp, online, disabled, parent, instance) = {
        let c = player.data();
        (
            c.unit.hp,
            c.is_online,
            c.disabled_set_position,
            c.unit.transform.parent_obj_id,
            c.unit.transform.instance_id,
        )
    };
    let reason = if hp == 0 {
        "the bot died"
    } else if !online {
        "the bot is offline"
    } else if disabled {
        "the server holds the position (teleport / zone load)"
    } else if parent != 0 {
        // Standing on a ship / mate: the ground navigator has no local frame.
        "standing on another object"
    } else if instance != instance_id {
        "the bot changed instance"
    } else {
        return None;
    };
    Some(NavFailure::Interrupted(reason))
}

/// Plans `from` -> `to` off the async workers. `spots` are stuck circles to route around (the
/// repath rung and the bot's own stuck history).
async fn plan_blocking(
    server: &Arc<GameServer>,
    player: &PlayerRef,
    bot: &Arc<BotSession>,
    from: Vec3,
    to: Vec3,
    spots: Vec<(f32, f32)>,
    cancel: PlanCancel,
) -> Result<Vec<Vec3>, String> {
    let Some(nav) = OwnedGameNav::of_player(server.clone(), player.clone(), bot.road_cache())
    else {
        return Err("the bot's world instance is unknown".to_string());
    };
    let base: Arc<dyn NavWorld + Send + Sync> = Arc::new(nav);
    let world: Arc<dyn NavWorld + Send + Sync> = if spots.is_empty() {
        base
    } else {
        Arc::new(Avoid {
            world: base,
            spots,
            radius: AVOID_RADIUS_M,
        })
    };
    let started = std::time::Instant::now();
    let Some(report) = nav::plan_guarded(world, from, to, bot.plan_gate(), cancel).await else {
        return Err("plan cancelled".to_string());
    };
    match report.plan {
        Some(plan) => {
            tracing::info!(
                "Bots: route ({:.1}, {:.1}) -> ({:.1}, {:.1}) planned via {} ({} waypoints) in {} ms (gate wait included)",
                from.x,
                from.y,
                to.x,
                to.y,
                plan.source,
                plan.points.len(),
                started.elapsed().as_millis()
            );
            Ok(plan.points)
        }
        None => Err(report.failure().unwrap_or_else(|| "no route".to_string())),
    }
}

/// The bot's current run / swim speed (m/s) from its unit attributes: slows, trade packs and
/// buffs move it exactly like a player.
pub(crate) fn speeds(player: &PlayerRef) -> (f32, f32) {
    let obj = WorldObject::from(player.clone());
    (
        RUN_SPEED_MPS * aaemu_game::systems::ai::nav::movement_speed_mul(&obj),
        SWIM_SPEED_MPS * aaemu_game::systems::ai::nav::swim_speed_mul(&obj),
    )
}

/// The time limit of one walk: an allowance that follows the route and the bot, and a
/// no-progress timeout.
///
/// * The allowance is [`DEADLINE_ROUTE_FACTOR`] times what the rest of the route needs at the
///   bot's *current* speed plus [`DEADLINE_SLACK_S`]. Nothing caps it below what the route
///   needs: a 10 km route gets over an hour, where a fixed 30 minute cap ended walks of
///   about 3 km (less for a slowed bot) while they were still making progress.
/// * A re-plan derives it again from the new route ([`WalkClock::replanned`]) and every
///   [`PROGRESS_M`] the bot gets closer along its route extends it, so a slowed bot or a long
///   detour that keeps progressing is not timed out half way.
/// * A walk whose remaining route has not shrunk by [`PROGRESS_M`] for
///   [`NO_PROGRESS_TIMEOUT_S`] is given up whatever is left of its allowance, so a bot that
///   wanders without getting closer still ends.
struct WalkClock {
    deadline: tokio::time::Instant,
    /// The shortest remaining route length since the last re-plan and when it was reached.
    best_m: f32,
    best_at: tokio::time::Instant,
    /// Test hook: an absolute stop (`BotSession::walk_time_cap`).
    hard_stop: Option<tokio::time::Instant>,
}

/// Which limit of the [`WalkClock`] ended a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkLimit {
    /// The allowance for the rest of the route ran out.
    Allowance,
    /// The bot stopped getting closer.
    NoProgress,
    /// The test cap.
    Capped,
}

impl WalkClock {
    fn new(
        remaining_m: f32,
        speed: f32,
        now: tokio::time::Instant,
        hard_stop: Option<tokio::time::Instant>,
    ) -> Self {
        Self {
            deadline: now + allowance(remaining_m, speed),
            best_m: remaining_m,
            best_at: now,
            hard_stop,
        }
    }

    /// A new route from the bot's position: the allowance and the progress mark start again
    /// from its length (lengths along different routes are not comparable).
    fn replanned(&mut self, remaining_m: f32, speed: f32, now: tokio::time::Instant) {
        self.deadline = now + allowance(remaining_m, speed);
        self.best_m = remaining_m;
        self.best_at = now;
    }

    /// Records the route still ahead at `now` (progress extends the allowance); the limit that
    /// has been reached, if any.
    fn check(
        &mut self,
        remaining_m: f32,
        speed: f32,
        now: tokio::time::Instant,
    ) -> Option<WalkLimit> {
        if remaining_m + PROGRESS_M <= self.best_m {
            self.best_m = remaining_m;
            self.best_at = now;
            self.deadline = self.deadline.max(now + allowance(remaining_m, speed));
        }
        if self.hard_stop.is_some_and(|stop| now >= stop) {
            Some(WalkLimit::Capped)
        } else if now >= self.deadline {
            Some(WalkLimit::Allowance)
        } else if now.duration_since(self.best_at) >= Duration::from_secs_f32(NO_PROGRESS_TIMEOUT_S)
        {
            Some(WalkLimit::NoProgress)
        } else {
            None
        }
    }
}

/// The time the rest of a route of `remaining_m` metres is allowed to take at `speed` m/s.
fn allowance(remaining_m: f32, speed: f32) -> Duration {
    let secs =
        remaining_m / speed.max(DEADLINE_MIN_SPEED) * DEADLINE_ROUTE_FACTOR + DEADLINE_SLACK_S;
    let secs = if secs.is_finite() {
        secs.clamp(DEADLINE_SLACK_S, MAX_ALLOWANCE_S)
    } else {
        DEADLINE_SLACK_S
    };
    Duration::from_secs_f32(secs)
}

fn position(player: &PlayerRef) -> Vec3 {
    let c = player.data();
    let t = c.unit.transform;
    Vec3::new(t.x, t.y, t.z)
}

fn dist3(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

fn dist2(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

fn delta2(a: Vec3, b: Vec3) -> (f32, f32) {
    (b.x - a.x, b.y - a.y)
}

fn heading(a: Vec3, b: Vec3) -> f32 {
    let angle = f64::from(b.y - a.y).atan2(f64::from(b.x - a.x));
    (angle - std::f64::consts::FRAC_PI_2) as f32
}

#[cfg(test)]
mod tests;

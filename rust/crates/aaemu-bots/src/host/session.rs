//! One live bot: its socketless connection, the replies it can read, and its think schedule.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aaemu_game::net::registry::ClientPacket;
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::world::session::{self, leave_target};
use aaemu_game::world::PlayerRef;
use aaemu_game::GameServer;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::combat::{self, CombatState, Mode};
use crate::nav::movement::{NavFailure, NavOutcome};
use crate::nav::{PlanCancel, PlanGate, RoadGraphCache};
use crate::rotation::ClassData;

/// Idle cadence of a bot between thinks (the cadence table / governor comes in P2/P3).
/// A bot character in the world on a socketless connection.
pub struct BotSession {
    pub character_id: u32,
    pub name: String,
    conn: Mutex<SocketlessConnection>,
    next_think: Mutex<Instant>,
    disconnected: AtomicBool,
    /// The road graphs of this bot's server (the host's shared cache, or a private one for a bot
    /// built on its own); built on the first route that needs one.
    roads: OnceLock<RoadGraphCache>,
    /// Generation of the running navigation task: a `goto` bumps it and earlier walkers stop.
    nav_generation: Arc<AtomicU64>,
    /// The generation [`BotSession::cancel_navigation`] produced: a walk that stops at exactly
    /// this generation was cancelled (a removal), not replaced by a newer `goto`.
    cancelled_generation: AtomicU64,
    /// The host's concurrent-plan gate (defaults to the server-wide gate in tests).
    plan_gate: OnceLock<Arc<PlanGate>>,
    /// Movement-packet clock: `ms_of_day` at the session start plus the monotonic time since
    /// then (`ms_of_day` itself wraps at UTC midnight and the verifier would see time go
    /// backwards), so it runs at wall-clock rate like a real client's, also across idle stretches.
    move_clock: (u32, std::time::Instant),
    /// Places the walker got stuck at (routed around by later plans).
    stuck_spots: Mutex<Vec<(f32, f32)>>,
    /// Client packets dispatched through this bot's connection (all senders).
    dispatched: AtomicU64,
    /// Test hook: an upper bound (ms) for the overall walk deadline (`0` = none).
    #[cfg(test)]
    walk_deadline_cap_ms: AtomicU64,
    /// Test hook: parks the next walk step between choosing it and sending it.
    #[cfg(test)]
    step_pause: std::sync::Mutex<Option<Arc<StepPause>>>,
    /// The combat / survival brain (target, rotation, resting, death, counters).
    brain: Mutex<CombatState>,
    /// Generation of the navigation task that owns the walk (`0` = none). A walk replaced or
    /// cancelled by a newer generation is not walking any more, even before its task ends.
    walk_generation: Arc<AtomicU64>,
    /// The outcome of the last walk that ended, with its generation (combat reads it back to
    /// tell a failed approach from a completed one).
    last_walk: Mutex<Option<(u64, NavOutcome)>>,
}

/// What became of one packet of a walk ([`BotSession::dispatch_walk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkSend {
    /// The packet went through the handler.
    Sent,
    /// A cancel or a newer `goto` replaced the walk: nothing was sent.
    Superseded,
    /// The bot is not in the world any more: nothing was sent.
    Offline,
}

/// The test hook of [`BotSession::pause_next_step`]: the walk notifies `reached` when it is
/// parked and continues once `resume` is notified.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct StepPause {
    pub reached: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
}

impl BotSession {
    pub fn new(conn: SocketlessConnection, character_id: u32, name: String) -> Self {
        // Stagger the first think by character id so a roster does not tick in lockstep.
        let jitter = Duration::from_millis(u64::from(character_id % 250));
        Self {
            character_id,
            name,
            conn: Mutex::new(conn),
            next_think: Mutex::new(Instant::now() + jitter),
            disconnected: AtomicBool::new(false),
            roads: OnceLock::new(),
            nav_generation: Arc::new(AtomicU64::new(0)),
            cancelled_generation: AtomicU64::new(u64::MAX),
            plan_gate: OnceLock::new(),
            move_clock: (
                aaemu_game::systems::ai::npc::ms_of_day(),
                std::time::Instant::now(),
            ),
            stuck_spots: Mutex::new(Vec::new()),
            dispatched: AtomicU64::new(0),
            #[cfg(test)]
            walk_deadline_cap_ms: AtomicU64::new(0),
            #[cfg(test)]
            step_pause: std::sync::Mutex::new(None),
            brain: Mutex::new(CombatState::default()),
            walk_generation: Arc::new(AtomicU64::new(0)),
            last_walk: Mutex::new(None),
        }
    }

    /// The combat brain (tests, the `/bot` commands and the live report).
    pub fn brain(&self) -> &Mutex<CombatState> {
        &self.brain
    }

    /// `/bot fight` / `/bot grind`: switch what the bot hunts and think right away. A new
    /// command drops the old fight (its target, approach walk and refusals).
    pub async fn set_mode(&self, mode: Mode) {
        {
            let mut state = self.brain.lock().await;
            state.mode = mode;
            state.disengage();
            state.loot_target = None;
            state.ignored.clear();
        }
        *self.next_think.lock().await = Instant::now();
    }

    /// A running navigation task (`goto`, the combat approach / flee / loot walks) of the
    /// current generation.
    pub fn walking(&self) -> bool {
        let generation = self.walk_generation.load(Ordering::Relaxed);
        generation != 0 && generation == self.nav_generation()
    }

    pub(crate) fn set_walk_generation(&self, generation: u64) {
        self.walk_generation.store(generation, Ordering::Relaxed);
    }

    /// Ends the walk of `generation` (a newer walk keeps the flag).
    pub(crate) fn clear_walk_generation(&self, generation: u64) {
        let _ = self.walk_generation.compare_exchange(
            generation,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Installs the class data of this bot: its archetype, rotation, level gap and cadence.
    /// Called by the host before the bot is on the roster.
    pub async fn configure(&self, class: Option<&ClassData>, max_gap: u8, tick_ms: u64) {
        let ability = self.player().await.map(|p| p.data().ability1);
        let archetype = ability
            .and_then(|a| class.and_then(|c| c.for_ability(a)))
            .cloned();
        let rotation = archetype
            .as_ref()
            .and_then(|a| class.and_then(|c| c.rotation_for(a)));
        let mut brain = self.brain.lock().await;
        combat::configure(&mut brain, archetype, rotation, max_gap, tick_ms);
    }

    /// The active character (`None` in the lobby or after a logout).
    pub async fn player(&self) -> Option<PlayerRef> {
        self.conn.lock().await.conn.active_char.clone()
    }

    /// `(x, y, z)` of the active character.
    pub async fn position(&self) -> Option<(f32, f32, f32)> {
        let player = self.player().await?;
        let c = player.data();
        let t = c.unit.transform;
        Some((t.x, t.y, t.z))
    }

    /// The generation of the running navigation task.
    pub(crate) fn nav_generation(&self) -> u64 {
        self.nav_generation.load(Ordering::SeqCst)
    }

    /// Starts a new navigation generation; any older walker notices and stops.
    pub(crate) fn bump_nav_generation(&self) -> u64 {
        self.nav_generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Cancels the running walk (the bot is being removed): it ends `Interrupted`.
    pub fn cancel_navigation(&self) {
        let generation = self.bump_nav_generation();
        self.cancelled_generation
            .store(generation, Ordering::SeqCst);
    }

    /// Why a walk of an older generation stopped: cancelled, or replaced by a newer `goto`.
    pub(crate) fn interrupt_reason(&self) -> &'static str {
        if self.nav_generation.load(Ordering::SeqCst)
            == self.cancelled_generation.load(Ordering::SeqCst)
        {
            "navigation cancelled"
        } else {
            "replaced by a newer goto"
        }
    }

    /// A token that cancels the plan of `generation` once a newer `goto` replaces it.
    pub(crate) fn nav_cancel(&self, generation: u64) -> PlanCancel {
        PlanCancel::generation(self.nav_generation.clone(), generation)
    }

    /// The host's plan gate (or the server-wide default when the bot was built directly).
    pub(crate) fn plan_gate(&self) -> Arc<PlanGate> {
        self.plan_gate
            .get_or_init(crate::nav::global_plan_gate)
            .clone()
    }

    /// Installs the host's configured gate (first call wins).
    pub fn install_plan_gate(&self, gate: Arc<PlanGate>) {
        let _ = self.plan_gate.set(gate);
    }

    /// The road graphs of this bot's server (a private cache when the host did not install its
    /// shared one).
    pub(crate) fn road_cache(&self) -> RoadGraphCache {
        self.roads.get_or_init(RoadGraphCache::default).clone()
    }

    /// Installs the host's road graph cache (first call wins).
    pub fn install_road_cache(&self, cache: RoadGraphCache) {
        let _ = self.roads.set(cache);
    }

    /// The next movement-packet time: a monotonic per-session clock at wall-clock rate, not the
    /// UTC day clock.
    pub(crate) fn next_move_time_ms(&self) -> u32 {
        let (base, started) = self.move_clock;
        base.wrapping_add(started.elapsed().as_millis() as u32)
    }

    /// The longest a walk of this bot may run, whatever its route allows (a test hook, see
    /// [`BotSession::set_walk_deadline_cap`]; `None` in production).
    pub(crate) fn walk_time_cap(&self) -> Option<Duration> {
        #[cfg(test)]
        {
            let cap = self.walk_deadline_cap_ms.load(Ordering::Relaxed);
            if cap > 0 {
                return Some(Duration::from_millis(cap));
            }
        }
        None
    }

    /// Test hook: no walk of this bot may run longer than `cap`.
    #[cfg(test)]
    pub(crate) fn set_walk_deadline_cap(&self, cap: Duration) {
        self.walk_deadline_cap_ms
            .store(cap.as_millis() as u64, Ordering::Relaxed);
    }

    /// Places the walker got stuck at (bounded; the oldest fall off).
    pub(crate) async fn stuck_spots(&self) -> Vec<(f32, f32)> {
        self.stuck_spots.lock().await.clone()
    }

    /// Records a stuck place for later plans to route around.
    pub(crate) async fn record_stuck_spot(&self, spot: (f32, f32)) {
        let mut spots = self.stuck_spots.lock().await;
        spots.push(spot);
        if spots.len() > 32 {
            spots.remove(0);
        }
    }

    /// Dispatches one client packet through the real handler on this bot's connection. A bot
    /// that left the world (removal / logout / a closed connection) sends nothing: the check
    /// runs under the connection lock a logout holds, like [`BotSession::dispatch_walk`]'s.
    pub async fn dispatch<P: ClientPacket + std::fmt::Debug>(
        &self,
        server: &GameServer,
        packet: P,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().await;
        let online = conn
            .conn
            .active_char
            .as_ref()
            .is_some_and(|player| player.data().is_online);
        if !online {
            anyhow::bail!("bot '{}' is not in the world", self.name);
        }
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        conn.dispatch(server, packet).await?;
        Ok(())
    }

    /// Records the outcome of the walk of `generation` (the walk task calls it once).
    pub(crate) async fn record_walk_outcome(&self, generation: u64, outcome: NavOutcome) {
        *self.last_walk.lock().await = Some((generation, outcome));
    }

    /// Did the walk of `generation` end failed? A walk replaced or cancelled is not a failure
    /// (something newer took over), so only unreachable / stuck / timeout count.
    pub(crate) async fn walk_failed(&self, generation: u64) -> bool {
        match self.last_walk.lock().await.as_ref() {
            Some((g, NavOutcome::Failed(failure))) => {
                *g == generation && !matches!(failure, NavFailure::Interrupted(_))
            }
            _ => false,
        }
    }

    /// Dispatches one packet of the walk of `generation`, unless that walk is stale by the time
    /// the connection is free: a cancel or a newer `goto` (the generation moved on) or a logout
    /// (the character is offline) stops the packet here, under the connection lock a logout
    /// also holds, so no movement follows the cancel however long the walk waited for its step,
    /// the lock or the handler.
    pub(crate) async fn dispatch_walk<P: ClientPacket + std::fmt::Debug>(
        &self,
        server: &GameServer,
        generation: u64,
        packet: P,
    ) -> WalkSend {
        let mut conn = self.conn.lock().await;
        if self.nav_generation() != generation {
            return WalkSend::Superseded;
        }
        let online = conn
            .conn
            .active_char
            .as_ref()
            .is_some_and(|player| player.data().is_online);
        if !online {
            return WalkSend::Offline;
        }
        self.dispatched.fetch_add(1, Ordering::Relaxed);
        let _ = conn.dispatch(server, packet).await;
        WalkSend::Sent
    }

    /// Client packets dispatched through this bot's connection so far.
    pub fn packets_dispatched(&self) -> u64 {
        self.dispatched.load(Ordering::Relaxed)
    }

    /// [`BotSession::packets_dispatched`] once a dispatch in flight has finished: it waits for
    /// the connection lock a dispatch holds from its generation check to its count.
    #[cfg(test)]
    pub(crate) async fn packets_dispatched_settled(&self) -> u64 {
        let _conn = self.conn.lock().await;
        self.packets_dispatched()
    }

    /// Test hook: the next walk step parks after the walker chose it and before it is sent,
    /// until the returned handle is released (the window the blocking step task leaves open).
    #[cfg(test)]
    pub(crate) fn pause_next_step(&self) -> Arc<StepPause> {
        let pause = Arc::new(StepPause::default());
        *self.step_pause.lock().expect("step pause poisoned") = Some(pause.clone());
        pause
    }

    /// The park point of [`BotSession::pause_next_step`] (a no-op without a pending pause).
    #[cfg(test)]
    pub(crate) async fn pause_before_send(&self) {
        let pause = self.step_pause.lock().expect("step pause poisoned").take();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }
    }

    /// The S->C frames the server wrote to this bot since the last call (the replies a client
    /// would read off the socket).
    pub async fn drain_replies(&self) -> Vec<(u16, Vec<u8>)> {
        self.conn.lock().await.drain()
    }

    /// `(obj_id, is_online)` of the active character.
    pub async fn state(&self) -> (u32, bool) {
        let conn = self.conn.lock().await;
        match &conn.conn.active_char {
            Some(player) => {
                let online = player.data().is_online;
                (player.obj_id, online)
            }
            None => (0, false),
        }
    }

    /// One scheduler pass. Services the connection's commands every tick; the think itself
    /// (draining replies; the AI in P2) runs on the per-bot `next_think` cadence.
    /// `false` = the connection is closing and the host must disconnect it.
    pub async fn tick(self: &Arc<Self>, server: &Arc<GameServer>) -> bool {
        {
            let mut conn = self.conn.lock().await;
            if !conn.service_commands(server).await {
                return false;
            }
        }
        let due = {
            let next = self.next_think.lock().await;
            Instant::now() >= *next
        };
        if !due {
            return true;
        }
        let mut next = self.next_think.lock().await;
        if Instant::now() < *next {
            return true;
        }
        let wait = combat::think(self, server).await;
        *next = Instant::now() + wait.max(Duration::from_millis(20));
        true
    }

    /// The normal logout without the client's logout timer: save + remove from the world. The
    /// connection stays alive (in the lobby) until [`BotSession::disconnect`].
    pub async fn logout(&self, server: &GameServer) {
        // The fight belongs to the session: clear it under the brain lock (the same
        // brain-then-connection order a think takes) so no think can start another packet
        // between the clear and the leave.
        let mut brain = self.brain.lock().await;
        brain.clear_session();
        let mut conn = self.conn.lock().await;
        session::leave_world_now(server, &mut conn.conn, leave_target::CHARACTER_SELECT).await;
    }

    /// The `GameConnection.OnDisconnect` cleanup (frees the account/connection and any session
    /// reservation). Idempotent: the host calls it once after a logout or a closed connection.
    pub async fn disconnect(&self, server: &GameServer) {
        if self.disconnected.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(server) = server.self_arc() else {
            return;
        };
        self.brain.lock().await.clear_session();
        let mut conn = self.conn.lock().await;
        aaemu_game::net::listener::on_disconnect(&server, &mut conn.conn).await;
    }
}

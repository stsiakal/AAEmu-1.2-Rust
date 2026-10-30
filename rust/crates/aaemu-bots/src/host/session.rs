//! One live bot: its socketless connection, the replies it can read, and its think schedule.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aaemu_game::net::registry::ClientPacket;
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::world::session::{self, leave_target};
use aaemu_game::world::PlayerRef;
use aaemu_game::GameServer;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::nav::{PlanCancel, PlanGate, RoadGraphCache, STEP_MS};

/// Idle cadence of a bot between thinks (the cadence table / governor comes in P2/P3).
const IDLE_THINK: Duration = Duration::from_millis(1_000);

/// A bot character in the world on a socketless connection.
pub struct BotSession {
    pub character_id: u32,
    pub name: String,
    conn: Mutex<SocketlessConnection>,
    next_think: Mutex<Instant>,
    disconnected: AtomicBool,
    /// The long-range road graph of this bot's world (built on the first route that needs it).
    pub(crate) roads: RoadGraphCache,
    /// Generation of the running navigation task: a `goto` bumps it and earlier walkers stop.
    nav_generation: Arc<AtomicU64>,
    /// The generation [`BotSession::cancel_navigation`] produced: a walk that stops at exactly
    /// this generation was cancelled (a removal), not replaced by a newer `goto`.
    cancelled_generation: AtomicU64,
    /// The host's concurrent-plan gate (defaults to the server-wide gate in tests).
    plan_gate: OnceLock<Arc<PlanGate>>,
    /// Monotonic movement-packet time: `ms_of_day` wraps at UTC midnight and the verifier
    /// would see time go backwards.
    move_time_ms: AtomicU32,
    /// Places the walker got stuck at (routed around by later plans).
    stuck_spots: Mutex<Vec<(f32, f32)>>,
    /// Test hook: an upper bound (ms) for the overall walk deadline (`0` = none).
    #[cfg(test)]
    walk_deadline_cap_ms: AtomicU64,
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
            roads: RoadGraphCache,
            nav_generation: Arc::new(AtomicU64::new(0)),
            cancelled_generation: AtomicU64::new(u64::MAX),
            plan_gate: OnceLock::new(),
            move_time_ms: AtomicU32::new(aaemu_game::systems::ai::npc::ms_of_day()),
            stuck_spots: Mutex::new(Vec::new()),
            #[cfg(test)]
            walk_deadline_cap_ms: AtomicU64::new(0),
        }
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

    /// The next movement-packet time: a monotonic per-session clock (100 ms per step), not the
    /// UTC day clock.
    pub(crate) fn next_move_time_ms(&self) -> u32 {
        self.move_time_ms
            .fetch_add(STEP_MS as u32, Ordering::Relaxed)
    }

    /// The overall walk deadline (tests may cap it with [`BotSession::set_walk_deadline_cap`]).
    pub(crate) fn cap_walk_deadline(&self, deadline: Instant) -> Instant {
        #[cfg(test)]
        {
            let cap = self.walk_deadline_cap_ms.load(Ordering::Relaxed);
            if cap > 0 {
                return deadline.min(Instant::now() + Duration::from_millis(cap));
            }
        }
        deadline
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

    /// Dispatches one client packet through the real handler on this bot's connection.
    pub async fn dispatch<P: ClientPacket + std::fmt::Debug>(
        &self,
        server: &GameServer,
        packet: P,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().await;
        conn.dispatch(server, packet).await?;
        Ok(())
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
    pub async fn tick(&self, server: &GameServer) -> bool {
        let mut conn = self.conn.lock().await;
        if !conn.service_commands(server).await {
            return false;
        }
        let now = Instant::now();
        let mut next = self.next_think.lock().await;
        if now >= *next {
            let jitter = Duration::from_millis(u64::from(self.character_id % 250));
            *next = now + IDLE_THINK + jitter;
            let replies = conn.drain();
            if !replies.is_empty() {
                tracing::trace!(
                    character_id = self.character_id,
                    frames = replies.len(),
                    "Bots: drained replies"
                );
            }
        }
        true
    }

    /// The normal logout without the client's logout timer: save + remove from the world. The
    /// connection stays alive (in the lobby) until [`BotSession::disconnect`].
    pub async fn logout(&self, server: &GameServer) {
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
        let mut conn = self.conn.lock().await;
        aaemu_game::net::listener::on_disconnect(&server, &mut conn.conn).await;
    }
}

//! The bots host: the operator-facing roster and the single scheduler task.
//!
//! P1 has no AI yet; the scheduler exists so P2 can add per-bot thinking without changing the
//! lifecycle. Every tick services each bot's `ConnCommand` channel (logout timer, shutdown
//! return-to-list) because a bot has no connection task; the per-bot think interval will drive
//! the activity governor later.

mod session;

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use aaemu_game::config::BotsConfig;
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::GameServer;
use tokio::sync::Mutex;

pub use session::BotSession;
#[cfg(test)]
pub(crate) use session::StepPause;
pub(crate) use session::WalkSend;

use crate::combat::Mode;
use crate::nav::movement::Goto;
use crate::nav::{PlanGate, RoadGraphCache};
use crate::rotation::ClassData;

/// The scheduler tick: connection commands are serviced this often.
const HOST_TICK: Duration = Duration::from_millis(250);

/// What one bot is shown as by `/bot list`.
#[derive(Debug, Clone)]
pub struct BotInfo {
    pub character_id: u32,
    pub name: String,
    pub obj_id: u32,
    pub online: bool,
}

/// The roster of live bots and their scheduler task. The server is held weakly so the host and
/// the command objects cannot keep it alive after shutdown.
pub struct BotHost {
    server: Weak<GameServer>,
    config: BotsConfig,
    plan_gate: Arc<PlanGate>,
    /// The road graphs of this host's server, shared by all its bots.
    roads: RoadGraphCache,
    /// The archetypes and rotations the bots grow and fight with (`None` without class data:
    /// combat then scores the skill tables only).
    class: Option<ClassData>,
    bots: Mutex<HashMap<u32, Arc<BotSession>>>,
}

impl BotHost {
    pub fn new(server: &Arc<GameServer>, config: BotsConfig) -> Self {
        Self {
            server: Arc::downgrade(server),
            plan_gate: Arc::new(PlanGate::new(config.plan_concurrency)),
            roads: RoadGraphCache::default(),
            class: None,
            config,
            bots: Mutex::new(HashMap::new()),
        }
    }

    /// Installs the class data loaded from `Bots.ArchetypesFile` / `Bots.RotationDir`.
    pub fn install_class_data(&mut self, class: ClassData) {
        self.class = Some(class);
    }

    /// Spawns the one host task (it exits once the server is dropped).
    pub fn start(self: &Arc<Self>) {
        let host = self.clone();
        tokio::spawn(async move { host.run().await });
    }

    async fn run(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(HOST_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_report = tokio::time::Instant::now();
        loop {
            ticker.tick().await;
            let Some(server) = self.server.upgrade() else {
                tracing::debug!("Bots: server dropped, host task exits");
                return;
            };
            let bots: Vec<Arc<BotSession>> = self.bots.lock().await.values().cloned().collect();
            for bot in bots {
                if !bot.tick(&server).await {
                    // The connection closed (quit / detach / shutdown): run the disconnect
                    // cleanup once and drop the roster entry.
                    let mut roster = self.bots.lock().await;
                    if roster
                        .get(&bot.character_id)
                        .is_some_and(|b| Arc::ptr_eq(b, &bot))
                    {
                        roster.remove(&bot.character_id);
                    }
                    drop(roster);
                    bot.cancel_navigation();
                    bot.disconnect(&server).await;
                }
            }
            if last_report.elapsed() >= Duration::from_secs(60) {
                last_report = tokio::time::Instant::now();
                self.report(&server).await;
            }
        }
    }

    /// One INFO line per bot a minute: the counters an unattended run is judged by.
    async fn report(&self, server: &GameServer) {
        let bots: Vec<Arc<BotSession>> = self.bots.lock().await.values().cloned().collect();
        for bot in bots {
            let state = bot.brain().lock().await;
            let (x, y, z) = bot.position().await.unwrap_or((0.0, 0.0, 0.0));
            let (hp, mp, max_hp, max_mp, battle) = bot.player().await.map_or(
                (0, 0, 1, 1, false),
                |p| {
                    let c = p.data();
                    (
                        c.unit.hp,
                        c.unit.mp,
                        c.unit.max_hp.max(1),
                        c.unit.max_mp.max(1),
                        c.unit.is_in_battle,
                    )
                },
            );
            tracing::info!(
                "Bots: '{}' {:?} hp {hp}/{max_hp} mp {mp}/{max_mp} battle {battle} target {:?} resting {} at ({x:.0}, {y:.0}, {z:.0}) kills {} deaths {} casts {} refused {} looted {}",
                bot.name,
                state.mode,
                state.target,
                state.resting,
                state.kills,
                state.deaths,
                state.casts_started,
                state.casts_refused,
                state.looted_items
            );
        }
        let _ = server;
    }

    /// `/bot add <name> [archetype]`: creates the character (on the archetype's creation tree
    /// when it is new) on the bot account, then logs it in through the real
    /// select/spawn/notify path. Returns the character id.
    pub async fn add(
        &self,
        server: &GameServer,
        name: &str,
        archetype: Option<&str>,
    ) -> anyhow::Result<u32> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /bot add <name> [archetype]");
        }
        let creation_ability = match archetype {
            Some(key) => match self.class.as_ref().and_then(|c| c.get(key)) {
                Some(a) => Some(a.creation_ability()),
                None => anyhow::bail!("unknown archetype '{key}' (see /bot list or Bots.ArchetypesFile)"),
            },
            None => self
                .class
                .as_ref()
                .and_then(|c| c.default_archetype())
                .map(|a| a.creation_ability()),
        };
        {
            let roster = self.bots.lock().await;
            if roster.len() >= self.config.max_bots {
                anyhow::bail!(
                    "bot limit reached (MaxBots={}); remove one first",
                    self.config.max_bots
                );
            }
            if roster.values().any(|b| b.name.eq_ignore_ascii_case(name)) {
                anyhow::bail!("'{name}' is already a live bot");
            }
        }
        let bot = crate::life::login(server, &self.config, name, creation_ability).await?;
        self.adopt(server, bot).await
    }

    /// Puts a bot that is in the world on the roster. It gets the host's plan gate and road
    /// graphs first: nothing else can reach it before it is on the roster, so no early `goto`
    /// can pin the server-wide fallback gate (`BotSession::plan_gate`; the first call wins).
    async fn adopt(&self, server: &GameServer, bot: Arc<BotSession>) -> anyhow::Result<u32> {
        let character_id = bot.character_id;
        bot.install_plan_gate(self.plan_gate.clone());
        bot.install_road_cache(self.roads.clone());
        bot.configure(self.class.as_ref(), self.config.max_target_level_gap, self.config.brain_tick_ms)
            .await;
        let refusal = {
            let mut roster = self.bots.lock().await;
            if roster.len() >= self.config.max_bots {
                // A concurrent add filled the last slot while this login was running.
                Some(format!(
                    "bot limit reached (MaxBots={}); remove one first",
                    self.config.max_bots
                ))
            } else if let Some(existing) = roster.get(&character_id) {
                // A concurrent add of the same character won the world entry; keep one session.
                Some(format!(
                    "'{}' is already a live bot (character {character_id})",
                    existing.name
                ))
            } else {
                roster.insert(character_id, bot.clone());
                None
            }
        };
        if let Some(reason) = refusal {
            // Do not leave the loser of the race in the world.
            bot.logout(server).await;
            bot.disconnect(server).await;
            anyhow::bail!(reason);
        }
        Ok(character_id)
    }

    /// `/bot remove <name>`: the normal logout (save + world removal + disconnect cleanup).
    pub async fn remove(&self, server: &GameServer, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        let bot = {
            let mut roster = self.bots.lock().await;
            let key = roster
                .iter()
                .find(|(_, b)| b.name.eq_ignore_ascii_case(name))
                .map(|(id, _)| *id);
            key.and_then(|id| roster.remove(&id))
        };
        let Some(bot) = bot else {
            anyhow::bail!("no live bot named '{name}'");
        };
        // Stop the walk before the character leaves the world: no packet may be sent after the
        // logout removed it.
        bot.cancel_navigation();
        bot.logout(server).await;
        bot.disconnect(server).await;
        tracing::info!(
            "Bots: removed '{}' (character {})",
            bot.name,
            bot.character_id
        );
        Ok(())
    }

    /// The live bot named `name`.
    pub async fn bot(&self, name: &str) -> Option<Arc<BotSession>> {
        self.bots
            .lock()
            .await
            .values()
            .find(|b| b.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    /// `/bot goto <name> <x> <y> <z>`: starts walking (the caller awaits [`Goto::wait`]).
    pub async fn goto(
        &self,
        server: &Arc<GameServer>,
        name: &str,
        target: Vec3,
        radius: f32,
    ) -> anyhow::Result<Goto> {
        let Some(bot) = self.bot(name.trim()).await else {
            anyhow::bail!("no live bot named '{}'", name.trim());
        };
        Ok(bot.start_goto(server, target, radius))
    }

    /// `/bot fight <name> [template]`: hunt any monster (or only `template`) within `radius`.
    pub async fn fight(&self, name: &str, wanted: Vec<u32>, radius: f32) -> anyhow::Result<()> {
        let Some(bot) = self.bot(name.trim()).await else {
            anyhow::bail!("no live bot named '{}'", name.trim());
        };
        bot.set_mode(Mode::Fight { wanted, radius }).await;
        Ok(())
    }

    /// `/bot grind <name> <radius>`: kill and loot around the bot's current position.
    pub async fn grind(&self, name: &str, radius: f32) -> anyhow::Result<()> {
        let Some(bot) = self.bot(name.trim()).await else {
            anyhow::bail!("no live bot named '{}'", name.trim());
        };
        let (x, y, _) = bot
            .position()
            .await
            .ok_or_else(|| anyhow::anyhow!("'{}' is not in the world", name.trim()))?;
        bot.set_mode(Mode::Grind {
            center: (x, y),
            radius: radius.max(5.0),
        })
        .await;
        Ok(())
    }

    /// `/bot stop <name>`: stop hunting (the bot stays logged in).
    pub async fn stop(&self, name: &str) -> anyhow::Result<()> {
        let Some(bot) = self.bot(name.trim()).await else {
            anyhow::bail!("no live bot named '{}'", name.trim());
        };
        bot.cancel_navigation();
        bot.set_mode(Mode::Idle).await;
        Ok(())
    }

    /// `/bot list`.
    pub async fn list(&self) -> Vec<BotInfo> {
        let bots: Vec<Arc<BotSession>> = self.bots.lock().await.values().cloned().collect();
        let mut infos = Vec::with_capacity(bots.len());
        for bot in bots {
            let (obj_id, online) = bot.state().await;
            infos.push(BotInfo {
                character_id: bot.character_id,
                name: bot.name.clone(),
                obj_id,
                online,
            });
        }
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aaemu_game::net::socketless::SocketlessConnection;
    use aaemu_game::test_support::{test_character, test_server};

    use super::*;
    use crate::life;

    /// A bot gets the host's plan gate before it is on the roster. `adopt` is parked at the
    /// roster insert (the test holds the roster), after whatever it does first; the gate
    /// asked for now must already be the host's, because an early `goto` that reaches a bot
    /// without one pins the server-wide fallback gate for good (the first call wins).
    /// A walk that a newer generation replaced (the combat approach cancels one before a cast)
    /// is not "walking" any more, even while its task is still unrolling; otherwise the bot
    /// would wait for it forever.
    #[tokio::test]
    async fn a_replaced_walk_is_not_walking() {
        use aaemu_game::net::socketless::SocketlessConnection;
        use aaemu_game::systems::skills::controllers::Vec3;
        use aaemu_game::test_support::{test_character, test_server};

        let server = Arc::new(test_server());
        server.set_self();
        let conn = SocketlessConnection::new(&server, 1_000_003);
        let bot = life::enter_loaded(&server, conn, test_character(3, "Walker", 100.0, 100.0))
            .expect("the bot enters");
        bot.start_goto(&server, Vec3::new(200.0, 100.0, 100.0), 1.0);
        assert!(bot.walking(), "the walk started");
        bot.bump_nav_generation();
        assert!(!bot.walking(), "a replaced walk is not running");
    }

    #[tokio::test]
    async fn a_bot_gets_the_hosts_gate_before_it_is_on_the_roster() {
        let server = Arc::new(test_server());
        server.set_self();
        let host = Arc::new(BotHost::new(&server, BotsConfig::default()));
        let conn = SocketlessConnection::new(&server, 1_000_002);
        let bot = life::enter_loaded(&server, conn, test_character(2, "Walker", 100.0, 100.0))
            .expect("the bot enters");
        let roster = host.bots.lock().await;
        let adopting = tokio::spawn({
            let (host, server, bot) = (host.clone(), server.clone(), bot.clone());
            async move { host.adopt(&server, bot).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            Arc::ptr_eq(&bot.plan_gate(), &host.plan_gate),
            "the bot is not on the roster yet but already has the host's gate"
        );
        drop(roster);
        assert_eq!(adopting.await.expect("task").expect("adopted"), 2);
        assert!(host.bot("Walker").await.is_some(), "on the roster now");
    }
}

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

use crate::nav::movement::Goto;
use crate::nav::PlanGate;

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
    bots: Mutex<HashMap<u32, Arc<BotSession>>>,
}

impl BotHost {
    pub fn new(server: &Arc<GameServer>, config: BotsConfig) -> Self {
        Self {
            server: Arc::downgrade(server),
            plan_gate: Arc::new(PlanGate::new(config.plan_concurrency)),
            config,
            bots: Mutex::new(HashMap::new()),
        }
    }

    /// Spawns the one host task (it exits once the server is dropped).
    pub fn start(self: &Arc<Self>) {
        let host = self.clone();
        tokio::spawn(async move { host.run().await });
    }

    async fn run(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(HOST_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
        }
    }

    /// `/bot add <name>`: creates the character on the bot account when missing, then logs it in
    /// through the real select/spawn/notify path. Returns the character id.
    pub async fn add(&self, server: &GameServer, name: &str) -> anyhow::Result<u32> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("usage: /bot add <name>");
        }
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
        let bot = crate::life::login(server, &self.config, name).await?;
        let character_id = bot.character_id;
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
        bot.install_plan_gate(self.plan_gate.clone());
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

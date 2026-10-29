//! Server-side player bots (private `bots` branch; see `BOTS-PLAN.md`).
//!
//! A bot is a normal character on a dedicated account: it owns a socketless [`GameConnection`]
//! (backend reply channel included) and acts by dispatching the same `CS*` packets a client
//! sends through the normal handlers. Nothing in the model or the wire marks it as a bot, and
//! the bots never use GM commands; `/bot` is an operator tool registered only when enabled.
//!
//! P1 scope: [`host`] (one scheduler task, per-bot next-think time, no AI yet), [`life`]
//! (create-or-load a character, enter through the real session path, save and remove), and
//! [`commands`]. Combat, quests, navigation and the governor come in P2/P3.
//!
//! [`GameConnection`]: aaemu_game::net::connection::GameConnection

pub mod commands;
#[cfg(test)]
mod flow_tests;
pub mod host;
pub mod life;

use std::sync::Arc;

use aaemu_game::GameServer;

pub use host::{BotHost, BotInfo, BotSession};

/// The startup hook of the `aaemu-game-bots` binary: with `Bots.Enabled` it registers the
/// operator commands and spawns the host; otherwise the server runs exactly like `aaemu-game`.
pub fn install(server: &Arc<GameServer>) -> anyhow::Result<()> {
    let config = server.config.bots.clone();
    if !config.enabled {
        tracing::info!("Bots: disabled (Bots.Enabled=false)");
        return Ok(());
    }
    let host = Arc::new(BotHost::new(server, config.clone()));
    commands::register(server, &host);
    host.start();
    tracing::info!(
        "Bots: enabled on accounts {}..{} ({} accounts; /bot add <name>)",
        config.account_base,
        config.account_base.saturating_add(config.max_bots as u32),
        config.max_bots
    );
    Ok(())
}

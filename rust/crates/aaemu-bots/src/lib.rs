//! Server-side player bots (private `bots` branch; see `BOTS-PLAN.md`).
//!
//! A bot is a normal character on a dedicated account: it owns a socketless [`GameConnection`]
//! (backend reply channel included) and acts by dispatching the same `CS*` packets a client
//! sends through the normal handlers. Nothing in the model or the wire marks it as a bot, and
//! the bots never use GM commands; `/bot` is an operator tool registered only when enabled.
//!
//! P1 scope: [`host`] (one scheduler task, per-bot next-think time, no AI yet), [`life`]
//! (create-or-load a character, enter through the real session path, save and remove), and
//! [`commands`]. P2 adds [`nav`]: route planning (`.bai`, height grid, transfer roads) and the
//! movement walker that sends real `CSMoveUnitPacket`s, with [`nav::movement::NavOutcome`] as
//! the `goto` API. Combat, quests and the governor come later.
//!
//! [`GameConnection`]: aaemu_game::net::connection::GameConnection

pub mod combat;
pub mod commands;
#[cfg(test)]
mod combat_flow_tests;
#[cfg(test)]
mod combat_real_data_tests;
#[cfg(test)]
mod flow_tests;
pub mod host;
pub mod life;
pub mod nav;
pub mod rotation;
#[cfg(test)]
mod nav_flow_tests;
#[cfg(test)]
mod nav_real_data_tests;

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
    let class = match rotation::ClassData::load(
        &config.archetypes_file,
        &config.rotation_dir,
        &config.default_archetype,
    ) {
        Ok(class) => Some(class),
        Err(e) => {
            tracing::error!("Bots: class data not loaded ({e}); combat uses the data scoring");
            None
        }
    };
    let mut host = BotHost::new(server, config.clone());
    if let Some(class) = class {
        tracing::info!(
            "Bots: {} archetypes, {} rotations loaded",
            class.archetypes().len(),
            class.archetypes()
                .iter()
                .filter(|a| class.rotation_for(a).is_some())
                .count()
        );
        host.install_class_data(class);
    }
    let host = Arc::new(host);
    commands::register(server, &host);
    host.start();
    start_configured_bots(server, &host);
    tracing::info!(
        "Bots: enabled on accounts {}..{} ({} accounts; /bot add <name>)",
        config.account_base,
        config.account_base.saturating_add(config.max_bots as u32),
        config.max_bots
    );
    Ok(())
}

/// Starts the `Bots.StartCharacters` list through the same path as `/bot add`, then begins
/// grinding around each bot's spawn point when `Bots.AutoGrindRadius` is set. This is the
/// unattended live run's setup: everything the bot does afterwards is a player action.
fn start_configured_bots(server: &Arc<GameServer>, host: &Arc<BotHost>) {
    let specs = server.config.bots.start_characters.clone();
    let radius = server.config.bots.auto_grind_radius;
    if specs.is_empty() {
        return;
    }
    let host = host.clone();
    let server = server.clone();
    tokio::spawn(async move {
        // Let the startup finish (networks, managers) before player sessions enter.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        for spec in specs {
            let (name, archetype) = match spec.split_once(':') {
                Some((name, archetype)) => (name.trim(), Some(archetype.trim())),
                None => (spec.trim(), None),
            };
            if name.is_empty() {
                continue;
            }
            match host.add(&server, name, archetype).await {
                Ok(_) => {
                    if radius > 0.0 {
                        if let Err(e) = host.grind(name, radius).await {
                            tracing::warn!("Bots: could not start grinding '{name}': {e}");
                        }
                    }
                }
                Err(e) => tracing::error!("Bots: could not start '{name}': {e}"),
            }
        }
    });
}

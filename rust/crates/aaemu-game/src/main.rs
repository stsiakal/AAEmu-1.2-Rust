//! `aaemu-game [APP_PATH] [configuration overrides...]`: the stock game server binary.
//!
//! The startup lives in [`aaemu_game::server_main`] (shared with the bots-enabled
//! `aaemu-game-bots` binary, which passes its own extension hook).

use std::sync::Arc;

use aaemu_game::server_main;

fn no_extension(_: &Arc<aaemu_game::GameServer>) -> anyhow::Result<()> {
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server_main::finish(
        server_main::run(std::env::args().skip(1).collect(), Box::new(no_extension)).await,
    )
}

//! `aaemu-game-bots [APP_PATH] [configuration overrides...]`: the game server with the
//! `aaemu-bots` extension linked. It runs the same startup as `aaemu-game`
//! (`aaemu_game::server_main`), then installs the bots host and `/bot` when `Bots.Enabled`.

use std::sync::Arc;

use aaemu_game::server_main;
use aaemu_game::GameServer;

fn bots_extension(server: &Arc<GameServer>) -> anyhow::Result<()> {
    aaemu_bots::install(server)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server_main::finish(
        server_main::run(std::env::args().skip(1).collect(), Box::new(bots_extension)).await,
    )
}

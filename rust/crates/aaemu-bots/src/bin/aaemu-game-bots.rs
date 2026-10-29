//! `aaemu-game-bots [APP_PATH] [configuration overrides...]`: the game server with the
//! `aaemu-bots` extension linked. It installs the bots startup extension and then runs the stock
//! `aaemu-game` entry point (`build.rs` copies the stock main into `OUT_DIR`, so upstream edits
//! to `aaemu-game/src/main.rs` stay the single source).
//!
//! The stock startup resolves its default application directory from `CARGO_MANIFEST_DIR`, which
//! is this crate (`rust/crates/aaemu-bots`) here: run this binary with the application directory
//! as the first argument (live-stack does) or set `AAEMU_APP_PATH`. The same applies to the
//! logged version string, which names this package.

use std::sync::Arc;

use aaemu_game::GameServer;

mod stock {
    include!(concat!(env!("OUT_DIR"), "/stock_main.rs"));

    pub fn run_stock() -> anyhow::Result<()> {
        main()
    }
}

fn bots_extension(server: &Arc<GameServer>) -> anyhow::Result<()> {
    aaemu_bots::install(server)
}

fn main() -> anyhow::Result<()> {
    let _ = aaemu_game::systems::set_startup_extension(Box::new(bots_extension));
    stock::run_stock()
}

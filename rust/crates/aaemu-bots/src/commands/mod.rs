//! `/bot` operator commands (GM only: no `AccessLevels.json` entry, so the command manager's
//! default access level 100 applies). Bots never call them; they are a tool for people.

use std::sync::Arc;

use aaemu_game::systems::services::commands::output::MessageOutput;
use aaemu_game::systems::services::commands::{Command, CommandContext};
use aaemu_game::GameServer;
use futures::future::BoxFuture;

use crate::host::BotHost;

/// `<add <name> | remove <name> | list>`.
pub struct BotCommand {
    host: Arc<BotHost>,
}

impl Command for BotCommand {
    fn command_names(&self) -> Vec<String> {
        vec!["bot".into()]
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        out.send_message(&help());
        Ok(())
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let server = ctx.server()?;
            match args.first().map(String::as_str) {
                Some("add") => {
                    let Some(name) = args.get(1) else {
                        anyhow::bail!("usage: /bot add <name>");
                    };
                    let character_id = self.host.add(server, name).await?;
                    out.send_message(&format!("[bot] added '{name}' (character {character_id})"));
                }
                Some("remove") => {
                    let Some(name) = args.get(1) else {
                        anyhow::bail!("usage: /bot remove <name>");
                    };
                    self.host.remove(server, name).await?;
                    out.send_message(&format!("[bot] removed '{name}'"));
                }
                Some("goto") => {
                    let (Some(name), Some(x), Some(y), Some(z)) =
                        (args.get(1), args.get(2), args.get(3), args.get(4))
                    else {
                        anyhow::bail!("usage: /bot goto <name> <x> <y> <z>");
                    };
                    let target = aaemu_game::systems::skills::controllers::Vec3::new(
                        x.parse::<f32>()
                            .map_err(|_| anyhow::anyhow!("x is not a number: {x}"))?,
                        y.parse::<f32>()
                            .map_err(|_| anyhow::anyhow!("y is not a number: {y}"))?,
                        z.parse::<f32>()
                            .map_err(|_| anyhow::anyhow!("z is not a number: {z}"))?,
                    );
                    let Some(server) = server.self_arc() else {
                        anyhow::bail!("the server is shutting down");
                    };
                    let goto = self.host.goto(&server, name, target, 1.0).await?;
                    out.send_message(&format!(
                        "[bot] '{name}' walking to ({:.1}, {:.1}, {:.1})",
                        target.x, target.y, target.z
                    ));
                    // The outcome arrives after the command returned.
                    let character = ctx.character.clone();
                    let name = name.clone();
                    tokio::spawn(async move {
                        let outcome = goto.wait().await;
                        character.send_message(&format!("[bot] '{name}' goto: {outcome}"));
                    });
                }
                Some("list") => {
                    let bots = self.host.list().await;
                    if bots.is_empty() {
                        out.send_message("[bot] no live bots");
                    }
                    for bot in bots {
                        out.send_message(&format!(
                            "[bot] {} character {} obj {} {}",
                            bot.name,
                            bot.character_id,
                            bot.obj_id,
                            if bot.online { "online" } else { "offline" }
                        ));
                    }
                }
                _ => out.send_message(&help()),
            }
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "add <name> | remove <name> | list | goto <name> <x> <y> <z>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Server-side player bots: add logs a character in through the normal session path, remove saves and logs it out, goto walks one to a point with the nav planner.".into()
    }
}

fn help() -> String {
    "[bot] /bot add <name> | /bot remove <name> | /bot list | /bot goto <name> <x> <y> <z>".into()
}

/// Registers `/bot` on the running server (called by [`crate::install`] only when enabled).
pub fn register(server: &GameServer, host: &Arc<BotHost>) {
    server
        .systems
        .services
        .commands
        .register_names(&["bot"], Arc::new(BotCommand { host: host.clone() }));
}

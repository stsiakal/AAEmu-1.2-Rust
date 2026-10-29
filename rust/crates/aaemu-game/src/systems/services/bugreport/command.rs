//! The `/bug <text>` player command.

use std::sync::Arc;

use serde_json::json;

use super::{report, Trigger};
use crate::systems::services::commands::output::MessageOutput;
use crate::systems::services::commands::{Command, CommandContext, CommandManager};

/// Longest `/bug` text kept (the rest is dropped before the report is queued).
const MAX_TEXT_CHARS: usize = 500;

/// `/bug <text>`: writes a report with the player as actor.
pub struct BugCommand;

impl Command for BugCommand {
    fn command_names(&self) -> Vec<String> {
        vec!["bug".into()]
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let server = ctx.server()?;
        let player = ctx.player()?;
        let mut text = args.join(" ");
        if text.chars().count() > MAX_TEXT_CHARS {
            text = text.chars().take(MAX_TEXT_CHARS).collect();
            text.push('…');
        }
        let queued = report(
            server,
            Trigger::Command {
                text: if text.is_empty() {
                    "(no text)".into()
                } else {
                    text
                },
            },
            Some(player.obj_id),
            None,
            json!({"source": "chat command"}),
        );
        if queued {
            out.send_message("[Bug] Report submitted, thank you.");
        } else {
            out.send_message("[Bug] Bug reports are disabled on this server.");
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<text>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Reports a bug with your character, position and nearby objects.".into()
    }
}

/// Registers `/bug` (access level from `Configurations/AccessLevels.json`).
pub fn register(manager: &CommandManager) {
    manager.register_names(&["bug"], Arc::new(BugCommand));
}

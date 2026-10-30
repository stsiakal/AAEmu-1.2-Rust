//! Character growth through the packets a player sends: the second / third skillset
//! (`CSSwapAbilityPacket`), skills (`CSLearnSkillPacket`) and passives (`CSLearnBuffPacket`).
//!
//! The learn order is the researched `archetypes.json` of the bot's archetype; the server
//! decides what is learnable (tree level, skill points, an active skillset), so a refused
//! packet only costs a retry. Ported from `aaemu-game/src/systems/bots/progression.rs`
//! (skillsets and learning only; gear and quest rewards are later phases).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aaemu_game::models::char_data::ability;
use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::handlers::{
    CSLearnBuffPacket, CSLearnSkillPacket, CSSwapAbilityPacket,
};
use aaemu_game::world::PlayerRef;

use crate::host::BotSession;
use crate::rotation::Archetype;

/// A quiet tick costs almost nothing; learning is retried about this often.
const PASS_INTERVAL_MS: i64 = 15_000;
/// Packets one pass may send (a big learn order should not burst).
const PASS_PACKETS: usize = 6;

#[derive(Debug, Default)]
pub struct ProgressState {
    /// The level the last pass ran at.
    level: u8,
    /// When the next pass may run regardless of the level.
    next_pass_ms: i64,
    /// Skills and passives already known at the last pass.
    known: HashSet<u32>,
    known_passives: HashSet<u32>,
}

/// One pass: the skillsets, the skills and the passives. Returns how many packets it sent.
pub async fn maintain(
    server: &GameServer,
    bot: &Arc<BotSession>,
    archetype: &Archetype,
    state: &mut ProgressState,
    now: i64,
) -> usize {
    let Some(player) = bot.player().await else {
        return 0;
    };
    let level = player.data().unit.level;
    if level == state.level && now < state.next_pass_ms {
        return 0;
    }
    state.level = level;
    state.next_pass_ms = now + PASS_INTERVAL_MS;
    let mut sent = 0;
    sent += take_skillset(server, bot, archetype, &player, level).await;
    sent += learn_skills(server, bot, archetype, state, &player, PASS_PACKETS - sent.min(PASS_PACKETS)).await;
    learn_passives(server, bot, archetype, state, &player).await;
    sent
}

/// Fills the empty second (level 5) / third (level 10) skillset with the next tree of the
/// archetype no active slot holds.
async fn take_skillset(
    server: &GameServer,
    bot: &Arc<BotSession>,
    archetype: &Archetype,
    player: &PlayerRef,
    level: u8,
) -> usize {
    let (actives, obj_id) = {
        let c = player.data();
        ([c.ability1, c.ability2, c.ability3], player.obj_id)
    };
    if actives.contains(&ability::NONE) && level >= 5 {
        let Some(next) = archetype
            .abilities()
            .into_iter()
            .find(|a| *a != 0 && !actives.contains(a))
        else {
            return 0;
        };
        let packet = CSSwapAbilityPacket {
            obj_id,
            old_ability: ability::NONE,
            ability: next,
            auap: false,
        };
        if bot.dispatch(server, packet).await.is_ok() {
            tracing::info!(
                "Bots: '{}' took skillset {next} ({})",
                bot.name,
                archetype.name
            );
            return 1;
        }
    }
    0
}

/// Learns the archetype's skills the server accepts now (the order matters: points, tree level
/// and open slots are the server's).
async fn learn_skills(
    server: &GameServer,
    bot: &Arc<BotSession>,
    archetype: &Archetype,
    state: &mut ProgressState,
    player: &PlayerRef,
    budget: usize,
) -> usize {
    let (actives, level, owned) = {
        let c = player.data();
        let owned: HashSet<u32> = c.skills.skills.iter().map(|s| s.id).collect();
        ([c.ability1, c.ability2, c.ability3], c.unit.level, owned)
    };
    let data = &server.game_data.systems.skills;
    let mut sent = 0;
    for (learn_level, step) in archetype.skills() {
        if sent >= budget || learn_level > level {
            break;
        }
        if owned.contains(&step.skill_id) || state.known.contains(&step.skill_id) {
            continue;
        }
        let Some(template) = data.skill(step.skill_id) else {
            continue;
        };
        if !template.need_learn || !actives.contains(&template.ability_id) {
            continue;
        }
        let ab = {
            let c = player.data();
            aaemu_game::db_character::ab_level(server, &c, template.ability_id)
        };
        if template.ability_level > ab {
            continue;
        }
        if bot
            .dispatch(server, CSLearnSkillPacket { skill_id: step.skill_id })
            .await
            .is_err()
        {
            continue;
        }
        if player
            .data()
            .skills
            .skills
            .iter()
            .any(|s| s.id == step.skill_id)
        {
            state.known.insert(step.skill_id);
            sent += 1;
            tracing::debug!(
                "Bots: '{}' learned skill {} ({})",
                bot.name,
                step.skill_id,
                step.name
            );
        }
    }
    sent
}

/// Learns the archetype's passives (the server enforces `req_points`).
async fn learn_passives(
    server: &GameServer,
    bot: &Arc<BotSession>,
    archetype: &Archetype,
    state: &mut ProgressState,
    player: &PlayerRef,
) {
    let owned: HashSet<u32> = player.data().skills.passive_buffs.iter().copied().collect();
    let level = player.data().unit.level;
    for (learn_level, step) in archetype.passives() {
        if learn_level > level {
            break;
        }
        if owned.contains(&step.buff_id) || state.known_passives.contains(&step.buff_id) {
            continue;
        }
        if bot
            .dispatch(server, CSLearnBuffPacket { buff_id: step.buff_id })
            .await
            .is_err()
        {
            continue;
        }
        if player
            .data()
            .skills
            .passive_buffs
            .contains(&step.buff_id)
        {
            state.known_passives.insert(step.buff_id);
            tracing::debug!(
                "Bots: '{}' learned passive {} ({})",
                bot.name,
                step.buff_id,
                step.name
            );
        }
    }
}

/// Sleep helper for a caller that wants a paced growth pass.
pub const PASS_INTERVAL: Duration = Duration::from_millis(PASS_INTERVAL_MS as u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pass_interval_is_the_pacing_of_the_learn_order() {
        assert_eq!(PASS_INTERVAL.as_millis(), 15_000);
        assert_eq!(PASS_PACKETS, 6);
    }
}

//! Bot lifecycle: create-or-load a character on its own dedicated bot account, enter the world
//! through the real session path, and leave it again (save + removal + disconnect cleanup).
//!
//! There is no shortcut into a subsystem: after the character row loads, the bot goes through
//! `world::session` exactly like a client's select / spawn / notify-in-game / leave packets.

use std::sync::Arc;

use aaemu_game::config::BotsConfig;
use aaemu_game::managers::character::{self, CreateCharacterRequest};
use aaemu_game::models::char_data::{ability, VisualOptions};
use aaemu_game::models::character::Character;
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::world::session::{self, leave_target, SelectDbData};
use aaemu_game::GameServer;

use crate::host::BotSession;

/// Race / gender of a generated bot character (Nuian; the archetype picks the creation tree).
const BOT_RACE: u8 = 1;
const BOT_GENDER: u8 = 1;

/// What `CSSpawnCharacterPacket` carries for a client (every optional visual block present), so
/// `SCUnitStatePacket` is complete for the observers of the bot.
pub fn visual_options() -> VisualOptions {
    VisualOptions {
        flag: 0x1F,
        stp: Some([0; 6]),
        helmet: true,
        back_holdable: true,
        cosplay: true,
        cosplay_backpack: true,
    }
}

/// `/bot add <name>`: finds the bot's account (the one already holding the character, else a
/// free one), creates `name` when it does not exist, then runs the real `CSSelectCharacter`
/// (reservation + MySQL load), spawn, notify-in-game and notify-in-game-completed steps on a
/// socketless connection.
pub async fn login(
    server: &GameServer,
    config: &BotsConfig,
    name: &str,
    creation_ability: Option<u8>,
) -> anyhow::Result<Arc<BotSession>> {
    if server.world.character_by_name(name).is_some() {
        anyhow::bail!("'{name}' is already in the world");
    }
    let (account, characters) = bot_account(server, config, name).await?;
    let mut conn = SocketlessConnection::new(server, account);
    // A failed add must not leave a phantom connection in `server.connections`.
    match login_with(server, name, characters, creation_ability, &mut conn).await {
        Ok((character_id, name, obj_id)) => {
            tracing::info!(
                "Bots: '{name}' (character {character_id}, obj {obj_id}) entered the world on account {account}"
            );
            Ok(Arc::new(BotSession::new(conn, character_id, name)))
        }
        Err(e) => {
            server.connections.remove(conn.id());
            Err(e)
        }
    }
}

/// Where a bot character lives: `AccountBase + n`, one character each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountChoice {
    /// Index in the candidate list (`AccountBase + index` is the account id).
    pub index: usize,
    pub account: u32,
    /// The account already holds a character with the requested name.
    pub existing: bool,
}

/// Picks the bot account for `name`: the account already holding that character (so a removed
/// bot comes back on its own account and keeps its state), else the first empty account in
/// `AccountBase..AccountBase + MaxBots`. Never puts two bot characters on one account.
pub(crate) fn choose_account(
    base: u32,
    max_bots: usize,
    accounts: &[(u32, Vec<String>)],
    name: &str,
) -> anyhow::Result<AccountChoice> {
    for (index, (account, names)) in accounts.iter().enumerate() {
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Ok(AccountChoice {
                index,
                account: *account,
                existing: true,
            });
        }
    }
    for (index, (account, names)) in accounts.iter().enumerate() {
        if names.is_empty() {
            return Ok(AccountChoice {
                index,
                account: *account,
                existing: false,
            });
        }
    }
    anyhow::bail!(
        "no free bot account in {base}..{} (MaxBots={max_bots}, one character per account)",
        base.saturating_add(u32::try_from(max_bots).unwrap_or(u32::MAX))
    )
}

/// Loads the candidate bot accounts and returns the chosen one with its characters.
async fn bot_account(
    server: &GameServer,
    config: &BotsConfig,
    name: &str,
) -> anyhow::Result<(u32, Vec<Character>)> {
    let mut loaded: Vec<(u32, Vec<Character>)> = Vec::with_capacity(config.max_bots);
    let mut names: Vec<(u32, Vec<String>)> = Vec::with_capacity(config.max_bots);
    for n in 0..config.max_bots {
        let account = config
            .account_base
            .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
        // Ensures the dedicated account row exists (`AccountManager.GetAccountDetailsInternal`).
        let _ = aaemu_game::db::get_account_details(&server.db, &server.config, account).await;
        let (characters, packets) =
            aaemu_game::db_character::load_account_characters(server, account).await?;
        // Deletion bookkeeping packets for a client connection; nobody reads this lobby.
        drop(packets);
        names.push((
            account,
            characters.iter().map(|c| c.unit.name.clone()).collect(),
        ));
        loaded.push((account, characters));
    }
    let choice = choose_account(config.account_base, config.max_bots, &names, name)?;
    let (account, characters) = loaded.swap_remove(choice.index);
    Ok((account, characters))
}

async fn login_with(
    server: &GameServer,
    name: &str,
    characters: Vec<Character>,
    creation_ability: Option<u8>,
    conn: &mut SocketlessConnection,
) -> anyhow::Result<(u32, String, u32)> {
    conn.conn.characters = characters;

    let character_id = match conn
        .conn
        .characters
        .iter()
        .find(|c| c.unit.name.eq_ignore_ascii_case(name))
    {
        Some(c) => c.id,
        None => {
            let request = CreateCharacterRequest {
                name: name.to_string(),
                race: BOT_RACE,
                gender: BOT_GENDER,
                body_items: [0; 7],
                model_params: Default::default(),
                ability1: creation_ability.unwrap_or(1),
                ability2: ability::NONE,
                ability3: ability::NONE,
                level: 1,
            };
            character::create(server, &mut conn.conn, request)
                .await
                .map_err(|e| anyhow::anyhow!("creating bot character '{name}': {e}"))?;
            conn.conn
                .characters
                .iter()
                .find(|c| c.unit.name.eq_ignore_ascii_case(name))
                .map(|c| c.id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "character creation for '{name}' failed (name invalid or taken; see the server log)"
                    )
                })?
        }
    };

    session::select_character(server, &mut conn.conn, character_id)
        .await
        .map_err(|e| anyhow::anyhow!("selecting bot character {character_id}: {e}"))?;
    let Some(player) = conn.conn.active_char.clone() else {
        anyhow::bail!("character {character_id} ('{name}') did not enter the world");
    };
    let (character_id, name, obj_id) = {
        let c = player.data();
        (c.id, c.unit.name.clone(), player.obj_id)
    };
    session::enter_world(server, &mut conn.conn, visual_options());
    if server.world.character_by_id(character_id).is_none() {
        // enter_world refused (the server is shutting down): undo the select (save + release).
        session::leave_world_now(server, &mut conn.conn, leave_target::CHARACTER_SELECT).await;
        anyhow::bail!("character {character_id} ('{name}') did not enter the world");
    }
    Ok((character_id, name, obj_id))
}

/// The world-entry half of [`login`] for an already loaded character (tests, and a future roster
/// preloaded before MySQL): reservation, `Character.Load` tail, spawn, notify. No MySQL.
pub fn enter_loaded(
    server: &GameServer,
    mut conn: SocketlessConnection,
    character: Character,
) -> anyhow::Result<Arc<BotSession>> {
    let conn_id = conn.id();
    match enter_loaded_with(server, &mut conn, character) {
        Ok((character_id, name)) => Ok(Arc::new(BotSession::new(conn, character_id, name))),
        Err(e) => {
            server.connections.remove(conn_id);
            Err(e)
        }
    }
}

fn enter_loaded_with(
    server: &GameServer,
    conn: &mut SocketlessConnection,
    character: Character,
) -> anyhow::Result<(u32, String)> {
    let character_id = character.id;
    let name = character.unit.name.clone();
    conn.conn.characters = vec![character.clone()];
    match server
        .enter_world
        .reserve_character(character_id, conn.id())
    {
        Ok(_) => {}
        Err(owner) => anyhow::bail!(
            "character {character_id} ('{name}') is already in the world on session {owner}"
        ),
    }
    if let Err(e) =
        session::select_loaded(server, &mut conn.conn, character, SelectDbData::default())
    {
        server
            .enter_world
            .release_character(character_id, conn.id());
        return Err(anyhow::anyhow!("selecting character {character_id}: {e}"));
    }
    session::enter_world(server, &mut conn.conn, visual_options());
    if server.world.character_by_id(character_id).is_none() {
        if let Some(player) = &conn.conn.active_char {
            server.world.try_remove_character(player.obj_id);
        }
        server
            .enter_world
            .release_character(character_id, conn.id());
        anyhow::bail!("character {character_id} ('{name}') did not enter the world");
    }
    Ok((character_id, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_accounts(base: u32, max_bots: usize) -> Vec<(u32, Vec<String>)> {
        (0..max_bots)
            .map(|n| (base + u32::try_from(n).unwrap(), Vec::new()))
            .collect()
    }

    #[test]
    fn eight_bots_get_eight_accounts() {
        let base = 1_000_000;
        let max_bots = 10;
        let mut accounts = candidate_accounts(base, max_bots);
        let mut chosen = Vec::new();
        for n in 0..8 {
            let name = format!("Bot{n}");
            let choice = choose_account(base, max_bots, &accounts, &name).expect("free account");
            assert!(!choice.existing);
            accounts[choice.index].1.push(name);
            chosen.push(choice.account);
        }
        chosen.sort_unstable();
        chosen.dedup();
        assert_eq!(chosen.len(), 8, "one account per bot");
        assert_eq!(
            chosen,
            (0..8).map(|n| base + n).collect::<Vec<_>>(),
            "accounts are AccountBase + n"
        );
    }

    #[test]
    fn an_existing_bot_keeps_its_account_and_a_full_set_has_no_free_slot() {
        let base = 1_000_000;
        let max_bots = 3;
        let accounts = vec![
            (base, vec!["Alpha".to_string()]),
            (base + 1, vec!["Beta".to_string()]),
            (base + 2, Vec::new()),
        ];
        let reuse = choose_account(base, max_bots, &accounts, "beta").expect("existing");
        assert!(reuse.existing);
        assert_eq!(reuse.account, base + 1);
        let free = choose_account(base, max_bots, &accounts, "Gamma").expect("free");
        assert!(!free.existing);
        assert_eq!(free.account, base + 2);

        let full = vec![
            (base, vec!["Alpha".to_string()]),
            (base + 1, vec!["Beta".to_string()]),
            (base + 2, vec!["Gamma".to_string()]),
        ];
        let error = choose_account(base, max_bots, &full, "Delta").expect_err("no free account");
        assert!(error.to_string().contains("no free bot account"), "{error}");
    }
}

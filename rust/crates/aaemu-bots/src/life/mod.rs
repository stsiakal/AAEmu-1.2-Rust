//! Bot lifecycle: create-or-load a character on the dedicated bot account, enter the world
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

/// Race / gender / ability of a generated bot character (Nuian fighter, the first ability set).
const BOT_RACE: u8 = 1;
const BOT_GENDER: u8 = 1;
const BOT_ABILITY: u8 = 1;

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

/// `/bot add <name>`: loads the bot's account characters, creates `name` when it does not exist,
/// then runs the real `CSSelectCharacter` (reservation + MySQL load), spawn, notify-in-game and
/// notify-in-game-completed steps on a socketless connection.
pub async fn login(
    server: &GameServer,
    config: &BotsConfig,
    name: &str,
) -> anyhow::Result<Arc<BotSession>> {
    if server.world.character_by_name(name).is_some() {
        anyhow::bail!("'{name}' is already in the world");
    }
    let account = config.account;
    // Ensures the dedicated account row exists (`AccountManager.GetAccountDetailsInternal`).
    let _ = aaemu_game::db::get_account_details(&server.db, &server.config, account).await;
    let mut conn = SocketlessConnection::new(server, account);
    // A failed add must not leave a phantom connection in `server.connections`.
    match login_with(server, account, name, &mut conn).await {
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

async fn login_with(
    server: &GameServer,
    account: u32,
    name: &str,
    conn: &mut SocketlessConnection,
) -> anyhow::Result<(u32, String, u32)> {
    let (characters, packets) =
        aaemu_game::db_character::load_account_characters(server, account).await?;
    // Deletion bookkeeping packets for a client connection; nobody reads this connection's lobby.
    drop(packets);
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
                ability1: BOT_ABILITY,
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
        server.world.try_remove_character(character_id);
        server
            .enter_world
            .release_character(character_id, conn.id());
        anyhow::bail!("character {character_id} ('{name}') did not enter the world");
    }
    Ok((character_id, name))
}

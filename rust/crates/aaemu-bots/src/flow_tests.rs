//! P1 acceptance flow without MySQL: a bot logs in through the real session path, a nearby
//! TestClient player sees it as an ordinary character, the bot walks one step through the real
//! `CSMoveUnitPacket` handler, and the bot logs out (world removal + disconnect cleanup; the
//! departure save goes through `SaveManager` and is covered by the live check).

use std::sync::Arc;

use aaemu_game::models::movement::{MoveKind, MoveType, UnitMove};
use aaemu_game::net::socketless::SocketlessConnection;
use aaemu_game::opcodes::sc;
use aaemu_game::packets::c2g_world::CSMoveUnitPacket;
use aaemu_game::test_support::{spawn_player, test_character, test_server, TestClient};

use crate::life;

#[tokio::test]
async fn a_bot_logs_in_is_visible_walks_and_logs_out() {
    let server = Arc::new(test_server());
    server.set_self();

    // A nearby player (an ordinary TestClient with its own socketless connection).
    let mut player_client = TestClient::new(&server, 1, 1000);
    let player_obj = server.ids.object.get_next_id().expect("object id");
    let player = spawn_player(
        &server,
        &player_client,
        test_character(1, "Player", 100.0, 100.0),
        player_obj,
    );
    player_client.drain();

    // The bot enters through select_loaded + spawn_character + notify_in_game(_completed).
    let conn = SocketlessConnection::new(&server, 1_000_000);
    let bot_conn_id = conn.id();
    let bot = life::enter_loaded(&server, conn, test_character(2, "BotOne", 102.0, 100.0))
        .expect("bot enters the world");
    let bot_obj = server
        .world
        .character_by_id(2)
        .expect("bot is in the world")
        .obj_id;

    // The player sees a normal character: an SCUnitStatePacket with the character's name tag and
    // no bot-specific packet or marker.
    let seen = player_client.drain();
    let state = seen
        .iter()
        .find(|(op, _)| *op == sc::SCUnitStatePacket)
        .expect("the player sees the bot's unit state");
    assert_eq!(state.1[2], 0, "BaseUnitType.Character, not a special kind");
    assert_eq!(&state.1[5..11], b"BotOne");
    let player_frames = player_client.drain();
    assert!(!player_frames
        .iter()
        .any(|(op, _)| *op == sc::SCUnitsRemovedPacket));

    // The bot can read its own replies: the login burst plus the nearby player's unit state.
    let replies = bot.drain_replies().await;
    assert!(replies
        .iter()
        .any(|(op, _)| *op == sc::SCCharacterStatePacket));
    assert!(replies
        .iter()
        .any(|(op, body)| *op == sc::SCUnitStatePacket && body.windows(6).any(|w| w == b"Player")));

    // One step through the real CSMoveUnit handler; the nearby player gets the movement packet.
    let move_type = MoveType::new(MoveKind::Unit(UnitMove {
        x: 104.0,
        y: 100.0,
        z: 100.0,
        ..Default::default()
    }));
    bot.dispatch(
        &server,
        CSMoveUnitPacket {
            obj_id: bot_obj,
            move_type,
        },
    )
    .await
    .expect("the bot moves through the handler");
    let moved = player_client.drain();
    assert!(
        moved
            .iter()
            .any(|(op, _)| *op == sc::SCOneUnitMovementPacket),
        "the nearby player sees the bot move"
    );
    let bot_player = server
        .world
        .character_by_id(2)
        .expect("bot still in the world");
    assert!((bot_player.data().unit.transform.x - 104.0).abs() < 0.01);

    // Logout: the normal save + world removal, then the disconnect cleanup.
    bot.logout(&server).await;
    bot.disconnect(&server).await;
    let gone = player_client.drain();
    assert!(
        gone.iter().any(|(op, _)| *op == sc::SCUnitsRemovedPacket),
        "the nearby player is told the bot left"
    );
    assert!(server.world.character_by_id(2).is_none());
    assert_eq!(server.enter_world.character_owner(2), None);
    assert!(server.connections.get(bot_conn_id).is_none());
    assert_eq!(
        server.world.used_obj_id(2),
        Some(bot_obj),
        "the object id is kept for the next session of the character"
    );
    drop((player, bot));
}

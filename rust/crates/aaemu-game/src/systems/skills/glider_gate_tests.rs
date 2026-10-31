//! 1.2 glider rules: the deploy / linked-item gate (`FUN_393925c0`, error 326), the back-slot
//! change refusal (543) and the interaction refusal (334).

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::items::{backpack_type, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::Inv;
use crate::test_support::*;
use crate::world::{PlayerRef, WorldObject};

use super::data::effects::{BuffEffect, EffectTemplate};
use super::data::{BuffTemplate, EffectKind};
use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{SkillCastTarget, SkillCaster, SkillObject};

const PLAYER_OBJ: u32 = 0x100;
const GLIDER_ITEM: u32 = 14_677;
const GLIDER_SKILL: u32 = 12_373;
const GLIDER_USE_SKILL: u32 = 14_901;
const GLIDER_BUFF: u32 = 1_029;
const GLIDER_EFFECT: u32 = 9_100;
const OTHER_ITEM: u32 = 6_002;

fn glider_data() -> GameData {
    let mut data = crate::systems::economy::tests::test_data();
    let skills = &mut data.systems.skills;
    skills.add_buff(BuffTemplate {
        id: GLIDER_BUFF,
        kind: 1,
        duration: 0,
        gliding: true,
        ..Default::default()
    });
    skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: GLIDER_EFFECT,
            buff_id: Some(GLIDER_BUFF),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        GLIDER_EFFECT,
    );
    let mut deploy = super::tests::skill_template(GLIDER_SKILL);
    deploy.target_type = 0;
    deploy.damage_type_id = 0;
    deploy.skill_controller_id = super::glider::GLIDER_SKILL_CONTROLLER_ID;
    deploy.effects.push(super::tests::skill_effect(
        GLIDER_EFFECT,
        EffectKind::BuffEffect,
        GLIDER_EFFECT,
    ));
    skills.add_skill(deploy);
    let mut linked = super::tests::skill_template(GLIDER_USE_SKILL);
    linked.target_type = 0;
    linked.damage_type_id = 0;
    linked.link_backpack_type_id = backpack_type::GLIDER;
    linked.link_equip_slot_id = equip_slot::BACKPACK;
    skills.add_skill(linked);
    data.skills
        .default_skills
        .push(crate::data::skills::DefaultSkill {
            skill_id: GLIDER_USE_SKILL,
            slot: 24,
            add_to_slot: true,
        });
    data.items.templates.insert(
        GLIDER_ITEM,
        ItemTemplate {
            use_skill_id: GLIDER_SKILL,
            kind: TemplateKind::Backpack {
                backpack_type: backpack_type::GLIDER,
            },
            ..ItemTemplate::plain(GLIDER_ITEM, 1)
        },
    );
    data.items
        .templates
        .insert(OTHER_ITEM, ItemTemplate::plain(OTHER_ITEM, 1));
    data
}

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
}

fn fixture() -> Fixture {
    let server = Arc::new(test_server_with(glider_data()));
    server.set_self();
    super::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Glider", 100.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    client.drain();
    Fixture {
        server,
        client,
        player,
    }
}

/// Puts `template` into `st` at `slot` and registers the container for `server.items` lookups.
fn give_at(
    server: &GameServer,
    player: &PlayerRef,
    item_id: u64,
    template_id: u32,
    st: u8,
    slot: i32,
) -> u64 {
    let template = server.game_data.items.template(template_id).expect("item");
    let mut item = Item::create(item_id, template, 1, 0, 1, &server.game_data.items, 0);
    item.owner_id = u64::from(player.character_id);
    let container = Inv::of(player).get(st).expect("container");
    container
        .lock()
        .unwrap()
        .add_new_item(item, slot, &server.game_data.items)
        .expect("add item");
    server.items.add_container(container);
    item_id
}

fn item_cast(skill_id: u32, item_id: u64) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Item {
            obj_id: PLAYER_OBJ,
            item_id,
            item_template_id: GLIDER_ITEM,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Unit(PLAYER_OBJ),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

fn unit_cast(skill_id: u32) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster: SkillCaster::Unit(PLAYER_OBJ),
        target: SkillCastTarget::Unit(PLAYER_OBJ),
        flag: 0,
        skill_object: SkillObject::None,
    }
}

fn error_body(error: i16) -> Vec<u8> {
    let mut s = PacketStream::new();
    s.write_i16(error)
        .write_i16(error)
        .write_u32(0)
        .write_bool(true);
    s.into_bytes()
}

fn has_error(frames: &[(u16, Vec<u8>)], error: i16) -> bool {
    frames
        .iter()
        .any(|(op, body)| *op == sc::SCErrorMsgPacket && *body == error_body(error))
}

/// A deploy cast must carry the equipped glider, not an item that merely sits in the bag.
#[tokio::test]
async fn glider_deploy_from_the_bag_is_refused() {
    let mut f = fixture();
    let item_id = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100,
        GLIDER_ITEM,
        slot_type::INVENTORY,
        0,
    );
    start_skill(
        &f.server,
        &mut f.client.conn,
        item_cast(GLIDER_SKILL, item_id),
    );
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::MUST_EQUIP_PROPER_ITEM),
        "{frames:x?}"
    );
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillStartedPacket));
    assert!(!f.player.data().unit.buffs.check_buff(GLIDER_BUFF));
}

/// The linked glider UI skill (14901) also needs an equipped glider.
#[tokio::test]
async fn linked_glider_skill_without_a_glider_is_refused() {
    let mut f = fixture();
    start_skill(&f.server, &mut f.client.conn, unit_cast(GLIDER_USE_SKILL));
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::MUST_EQUIP_PROPER_ITEM),
        "{frames:x?}"
    );
    assert!(frames.iter().any(|(op, _)| *op == sc::SCSkillStartedPacket));
}

/// Deploying the equipped glider (the normal air start) still applies the gliding buff.
#[tokio::test]
async fn equipped_glider_deploy_starts_the_gliding_buff() {
    let mut f = fixture();
    let item_id = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100 + 1,
        GLIDER_ITEM,
        slot_type::EQUIPMENT,
        equip_slot::BACKPACK,
    );
    assert!(f.server.items.item_by_id(item_id).is_some());
    assert_eq!(
        f.server
            .game_data
            .items
            .template(GLIDER_ITEM)
            .map(|t| t.use_skill_id),
        Some(GLIDER_SKILL)
    );
    assert!(f
        .server
        .systems
        .skills
        .ext
        .items()
        .owned_item(&f.server, &f.player, item_id)
        .is_some());
    start_skill(
        &f.server,
        &mut f.client.conn,
        item_cast(GLIDER_SKILL, item_id),
    );
    let frames = f.client.drain();
    assert!(
        !has_error(&frames, error_message::MUST_EQUIP_PROPER_ITEM),
        "{frames:x?}"
    );
    let frames = f.client.drain();
    assert!(
        f.player.data().unit.buffs.check_buff(GLIDER_BUFF),
        "{frames:x?}"
    );
    let item = Inv::of(&f.player)
        .equipped(equip_slot::BACKPACK)
        .expect("glider stays equipped");
    assert_eq!(item.id, item_id);
}

/// 1.2 refuses a back-slot change while the glider is open (543).
#[tokio::test]
async fn backpack_change_while_gliding_is_refused() {
    use crate::systems::economy::item_handlers::{CSSwapItemsPacket, MoveArgs};

    let mut f = fixture();
    let glider = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100 + 2,
        GLIDER_ITEM,
        slot_type::EQUIPMENT,
        equip_slot::BACKPACK,
    );
    let other = give_at(
        &f.server,
        &f.player,
        PLAYER_OBJ as u64 * 100 + 3,
        OTHER_ITEM,
        slot_type::INVENTORY,
        0,
    );
    let me = WorldObject::from(&f.player);
    super::buffs::add_buff_by_id(&f.server, &me, GLIDER_BUFF, &me, 0);
    f.client.drain();

    // Another item into the back slot.
    let swap = CSSwapItemsPacket {
        args: MoveArgs {
            from_item_id: other,
            to_item_id: 0,
            from_slot_type: slot_type::INVENTORY,
            from_slot: 0,
            to_slot_type: slot_type::EQUIPMENT,
            to_slot: equip_slot::BACKPACK as u8,
        },
    };
    swap.handle(&f.server, &mut f.client.conn).await.unwrap();
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::CANNOT_CHANGE_BACKPACK_IN_GLIDING),
        "{frames:x?}"
    );
    assert_eq!(
        Inv::of(&f.player)
            .equipped(equip_slot::BACKPACK)
            .expect("glider stays")
            .id,
        glider
    );
    assert_eq!(
        Inv::of(&f.player)
            .item_at(slot_type::INVENTORY, 0)
            .expect("other stays")
            .id,
        other
    );

    // The equipped glider out to the bag.
    let swap = CSSwapItemsPacket {
        args: MoveArgs {
            from_item_id: glider,
            to_item_id: 0,
            from_slot_type: slot_type::EQUIPMENT,
            from_slot: equip_slot::BACKPACK as u8,
            to_slot_type: slot_type::INVENTORY,
            to_slot: 1,
        },
    };
    swap.handle(&f.server, &mut f.client.conn).await.unwrap();
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::CANNOT_CHANGE_BACKPACK_IN_GLIDING),
        "{frames:x?}"
    );
    assert_eq!(
        Inv::of(&f.player)
            .equipped(equip_slot::BACKPACK)
            .expect("glider stays")
            .id,
        glider
    );
}

/// 1.2 refuses interactions while gliding (334).
#[tokio::test]
async fn interaction_while_gliding_is_refused() {
    use crate::systems::npcs::doodad::coffer::CSCofferInteractionPacket;
    use crate::systems::npcs::npc::packets::CSStartInteractionPacket;

    let mut f = fixture();
    let me = WorldObject::from(&f.player);
    super::buffs::add_buff_by_id(&f.server, &me, GLIDER_BUFF, &me, 0);
    f.client.drain();

    let start = CSStartInteractionPacket {
        npc_obj_id: 0x999,
        obj_id: 0,
        extra_info: 0,
        pick_id: 0,
        mouse_button: 0,
        modifier_keys: 0,
    };
    start.handle(&f.server, &mut f.client.conn).await.unwrap();
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::CANNOT_INTERACT_WHILE_GLIDING),
        "{frames:x?}"
    );
    assert!(!frames
        .iter()
        .any(|(op, _)| *op == sc::SCNpcInteractionSkillListPacket));

    let coffer = CSCofferInteractionPacket {
        coffer_obj_id: 0x999,
        opening: true,
    };
    coffer.handle(&f.server, &mut f.client.conn).await.unwrap();
    let frames = f.client.drain();
    assert!(
        has_error(&frames, error_message::CANNOT_INTERACT_WHILE_GLIDING),
        "{frames:x?}"
    );
}

/// Every real 1.2 glider item and glider skill satisfies the gate's data assumptions.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn every_real_glider_item_and_skill_matches_the_gate() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact.sqlite3");
    let files = crate::data::client_files::ClientFileManager::new();
    let d = super::data::Data::load_from(&conn, &files).expect("skill data");
    let mut skills = conn
        .prepare(
            "SELECT i.id, i.use_skill_id FROM item_backpacks ib \
             JOIN items i ON i.id = ib.item_id \
             WHERE ib.backpack_type_id = 2 AND i.use_skill_id <> 0 ORDER BY i.id",
        )
        .expect("glider items");
    let mut items = 0usize;
    let mut seen = std::collections::HashSet::new();
    for row in skills
        .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))
        .expect("rows")
    {
        let (item_id, skill_id) = row.expect("row");
        items += 1;
        seen.insert(skill_id);
        let skill = d.skill(skill_id).expect("glider use skill");
        assert_eq!(
            skill.skill_controller_id,
            super::glider::GLIDER_SKILL_CONTROLLER_ID,
            "item {item_id} skill {skill_id}"
        );
        assert!(
            super::glider::is_glider_skill(skill),
            "item {item_id} skill {skill_id}"
        );
        assert!(
            skill.effects.iter().any(|e| {
                e.template.is_some_and(|(kind, id)| {
                    kind == EffectKind::BuffEffect
                        && d.buff(effect_buff_id(&d, kind, id))
                            .is_some_and(|b| b.gliding)
                })
            }),
            "item {item_id} skill {skill_id} must apply a gliding buff"
        );
    }
    assert_eq!(items, 36, "1.2 glider items with a use skill");
    assert!(
        seen.len() > 1 && seen.len() <= items,
        "glider items share their use skills: {} skills",
        seen.len()
    );
    let mut linked = conn
        .prepare("SELECT id FROM skills WHERE link_backpack_type_id = 2 ORDER BY id")
        .expect("linked skills");
    let linked: Vec<u32> = linked
        .query_map([], |r| r.get::<_, u32>(0))
        .expect("rows")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(linked, vec![14_901, 17_657], "1.2 linked glider skills");
    assert_eq!(
        d.skill(14_901).expect("glider use").link_backpack_type_id,
        backpack_type::GLIDER
    );
    assert_eq!(
        d.skill(14_901).expect("glider use").link_equip_slot_id,
        equip_slot::BACKPACK
    );
    assert_eq!(
        d.skill(17_657).expect("glider fold").link_backpack_type_id,
        backpack_type::GLIDER
    );
    assert_eq!(d.skill(17_657).expect("glider fold").link_equip_slot_id, -1);
    // The item deploy skill is not a linked-backpack skill (its item supplies the glider).
    assert_eq!(
        d.skill(GLIDER_SKILL)
            .expect("experimental glider")
            .link_backpack_type_id,
        1
    );
    // The S14 fixture: item 14677 -> skill 12373 -> buff 1029.
    assert_eq!(
        d.skill(GLIDER_SKILL)
            .expect("experimental glider")
            .effects
            .len(),
        1
    );
    assert!(d.buff(GLIDER_BUFF).is_some_and(|b| b.gliding));
}

fn effect_buff_id(d: &super::data::Data, kind: EffectKind, id: u32) -> u32 {
    match d.effect(kind, id) {
        Some(EffectTemplate::Buff(b)) => b.buff_id.unwrap_or(0),
        _ => 0,
    }
}

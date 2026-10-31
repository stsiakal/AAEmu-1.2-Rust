//! 1.2 glider rules shared by the skill, interaction and item-move entry points.
//!
//! Evidence:
//! * A glider is a back-slot item with `item_backpacks.backpack_type_id = 2` whose
//!   `items.use_skill_id` applies a `buffs.gliding = 't'` buff (S14 research, data check).
//! * The client's skill start resolves a `link_equip_slot_id` skill through the equipped item
//!   and requires the item's backpack type to equal `link_backpack_type_id`
//!   (`FUN_393925c0`); a failure raises `must_equip_proper_item` (326) before any packet.
//! * The client refuses to change the equipped backpack while gliding
//!   (`FUN_394e41f0`, error 543 `cannot_change_backpack_in_gliding`) and refuses interactions
//!   while gliding (`FUN_393b3d50`, error 334 `cannot_interact_while_gliding`).
//!
//! Not implemented here: an airborne deploy requirement. The 1.2 skill has only the authored
//! `unit_reqs` NotUnderWater and NoBuffTag 306 (`unit_reqs` 28592/28593), the client start path
//! `FUN_393989e0` checks only the linked item, and the startup lift is designed to take off from
//! the ground; the double-jump height / freefall gate remains capture-pending (S14-C4).

use crate::data::items::{backpack_type, TemplateKind};
use crate::models::container::equip_slot;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::world::{PlayerRef, WorldObject};

use super::data::SkillTemplate;

/// `skills.skill_controller_id` of every 1.2 glider deploy skill (31 rows; no
/// `skill_controllers` row 2687 exists, so the server never runs a controller for the player).
pub const GLIDER_SKILL_CONTROLLER_ID: u32 = 2687;

/// Whether `skill` is a glider skill: the deploy skill (`skill_controller_id` 2687) or one of
/// the linked glider UI skills (`link_backpack_type_id` 2, e.g. 14901 use / 17657 fold).
pub fn is_glider_skill(skill: &SkillTemplate) -> bool {
    skill.skill_controller_id == GLIDER_SKILL_CONTROLLER_ID
        || skill.link_backpack_type_id == backpack_type::GLIDER
}

/// The character's equipped glider (`item_backpacks.backpack_type_id = 2` in the back slot).
pub fn equipped_glider(server: &GameServer, player: &PlayerRef) -> Option<Item> {
    let item = Inv::of(player).equipped(equip_slot::BACKPACK)?;
    let template = server.game_data.items.template(item.template_id)?;
    matches!(
        &template.kind,
        TemplateKind::Backpack {
            backpack_type: kind
        } if *kind == backpack_type::GLIDER
    )
    .then_some(item)
}

/// Whether the unit currently has an active buff with `buffs.gliding = 't'` (the 1.2 client's
/// stance 4 / `FUN_39357bc0` state).
pub fn is_gliding(server: &GameServer, unit: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    unit.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| data.buff(e.template.id).is_some_and(|t| t.gliding))
    })
}

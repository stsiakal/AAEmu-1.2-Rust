//! The 1.2 seed-builder placement rules (`FUN_393a8fa0`), shared by every path that creates a
//! planted doodad (`CSCreateDoodadPacket` and the scripted `CreatePlayerDoodad`), so a modified
//! client cannot bypass them.
//!
//! Evidence (see PORTING.md, `systems::npcs::doodad`):
//! * range: `FUN_393a8fa0` step 3, `dist3D^2 > 900`; F1 refuses at 30 m and accepts 29 m.
//! * water: step 1, group 40 (marine crops) needs water, every other group refuses it; the
//!   capture's text for the refusal is `DOODAD_SIM_AREA` (276) `Can't be installed here.`
//! * overlap: step 5, sphere test `dist3D < r_new + r_other` when both doodads are round
//!   (`FUN_3983b360`), radius `max(sim_radius / 1000, 0.1)`; step 6, units use the new doodad's
//!   radius. The oriented-box branch of step 5/6 is not decoded: non-round pairs keep the flat
//!   test on the new radius (the client PR AAEmu/AABevy1.2#48 made the same choice).
//!
//! Not implemented (open items in PORTING.md):
//! * the 6 m terrain-height spread of step 2 (`FUN_393a7720`; footprint sampling undecoded,
//!   capture F7 not driven), skipped by the 1.2 client for groups 2, 3, 5, 12, 37, 40 and 80;
//! * the house/farm area rule of step 0/0b (the server keeps its own house/public-farm checks).
//!
//! The client's water test (`worldvtbl+0xf4`) was not decoded; this uses the server's
//! `WorldInstance.IsWater` (water volumes + ocean), the same query class.

use crate::server::GameServer;
use crate::systems::npcs::doodad::packets::error_message;
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::world::{PlayerRef, WorldInstance, WorldObject};

/// `FUN_393a8fa0` step 3: strictly nearer than 30 m in 3D (F1: 29 m valid, 30 m refused).
pub const PLACE_RANGE: f32 = 30.0;
/// `FUN_3983b340`: marine crops, the only group that needs water (and refuses dry land).
pub const MARINE_CROPS: u32 = 40;
/// The client PR's water depth assumption (`height > spot.z + 0.3`); the decompiled water test
/// (`worldvtbl+0xf4`) and capture F5 were not decoded, so the 0.3 m margin is capture-pending.
pub const WATER_DEPTH_MARGIN: f32 = 0.3;
/// `FUN_3983b360`: the groups whose doodads overlap as spheres.
pub const ROUND_GROUPS: [u32; 6] = [2, 3, 5, 12, 40, 80];
/// `FUN_3983b360`: this template overlaps as a sphere whatever its group.
pub const SPHERE_TEMPLATE: u32 = 1219;
/// `FUN_3983b360`: this group overlaps as a sphere whatever the template.
pub const SPHERE_GROUP: u32 = 38;

/// Why a spot cannot hold the doodad, with the `ErrorMessageType` the 1.2 client shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `DOODAD_SIM_AREA` (276), the 1.2 area refusal ("Can't be installed here.").
    Area,
    /// `DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD` (277).
    Doodad,
    /// `DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT` (278).
    Unit,
    /// `TOO_FAR_AWAY` (154).
    TooFar,
}

impl Refusal {
    pub const fn error_message(self) -> i16 {
        match self {
            Self::Area => error_message::DOODAD_SIM_AREA,
            Self::Doodad => error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD,
            Self::Unit => error_message::DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT,
            Self::TooFar => error_message::TOO_FAR_AWAY,
        }
    }
}

/// `FUN_3983b280`: the room a doodad keeps around it, `max(sim_radius * 0.001, 0.1)` m.
pub fn radius(template: &DoodadTemplate) -> f32 {
    (template.sim_radius as f32 * 0.001).max(0.1)
}

/// `FUN_3983b360`: does this doodad overlap as a sphere?
pub fn is_round(template: &DoodadTemplate) -> bool {
    ROUND_GROUPS.contains(&template.group_id)
        || template.id == SPHERE_TEMPLATE
        || template.group_id == SPHERE_GROUP
}

/// A doodad obstruction at a world position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shape {
    pub pos: (f32, f32, f32),
    pub radius: f32,
    pub round: bool,
}

impl Shape {
    pub fn of(template: &DoodadTemplate, pos: (f32, f32, f32)) -> Self {
        Self {
            pos,
            radius: radius(template),
            round: is_round(template),
        }
    }
}

fn distance_squared(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)
}

fn flat_distance_squared(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)
}

/// The 1.2 doodad-overlap test: spheres on the sum of radii for two round doodads, otherwise the
/// flat distance on the new doodad's radius (the oriented boxes are not decoded).
pub fn doodad_overlaps(new: Shape, other: Shape) -> bool {
    let limit = if new.round && other.round {
        new.radius + other.radius
    } else {
        new.radius
    };
    let limit_squared = limit * limit;
    if new.round && other.round {
        distance_squared(new.pos, other.pos) < limit_squared
    } else {
        flat_distance_squared(new.pos, other.pos) < limit_squared
    }
}

/// The 1.2 unit-overlap test: a sphere of the new doodad's radius for a round doodad, the flat
/// radius otherwise.
pub fn unit_overlaps(new: Shape, other: (f32, f32, f32)) -> bool {
    let limit_squared = new.radius * new.radius;
    if new.round {
        distance_squared(new.pos, other) < limit_squared
    } else {
        flat_distance_squared(new.pos, other) < limit_squared
    }
}

/// The placement refusal of `pos` for `template`, or `None` when the spot is free.
///
/// Order follows `FUN_393a8fa0`: area (restrict zone) -> water -> range -> doodads -> units.
pub fn refusal(
    server: &GameServer,
    world: &WorldInstance,
    player: &PlayerRef,
    template: &DoodadTemplate,
    pos: (f32, f32, f32),
) -> Option<Refusal> {
    let (x, y, z) = pos;
    // F11: the 1.2 client refuses the restricted-zone seed with 276 before it starts a builder.
    if template.restrict_zone_id != 0
        && template.restrict_zone_id != world.template.zone_id_at(x, y)
    {
        return Some(Refusal::Area);
    }
    let wet = crate::systems::ai::collision::is_water(server, world.id, x, y, z)
        && crate::systems::ai::collision::water_level(server, world.id, x, y, z)
            .is_some_and(|surface| surface > z + WATER_DEPTH_MARGIN);
    if template.group_id == MARINE_CROPS {
        if !wet {
            return Some(Refusal::Area);
        }
    } else if wet {
        return Some(Refusal::Area);
    }
    let player_pos = world.world_pos_rot(&WorldObject::from(player));
    let player_pos = (player_pos.x, player_pos.y, player_pos.z);
    let distance_squared = distance_squared(player_pos, pos);
    if !distance_squared.is_finite() || distance_squared >= PLACE_RANGE * PLACE_RANGE {
        return Some(Refusal::TooFar);
    }
    let new = Shape::of(template, pos);
    let mut units: Vec<(f32, f32, f32)> = Vec::new();
    for obj in nearby_objects(world, x, y) {
        if obj.obj_id() == player.obj_id {
            continue;
        }
        let other_pos = world.world_pos_rot(&obj);
        let other_pos = (other_pos.x, other_pos.y, other_pos.z);
        if let Some(doodad) = obj.as_doodad() {
            let Some(other_template) = server
                .game_data
                .systems
                .npcs
                .doodad
                .template(doodad.data().template_id)
            else {
                continue;
            };
            if doodad_overlaps(new, Shape::of(other_template, other_pos)) {
                return Some(Refusal::Doodad);
            }
        } else if matches!(obj, WorldObject::House(_) | WorldObject::Shipyard(_))
            || obj.with_unit(|u| u.unit().is_dead()).unwrap_or(false)
        {
            continue;
        } else {
            units.push(other_pos);
        }
    }
    if units.iter().any(|&p| unit_overlaps(new, p)) {
        return Some(Refusal::Unit);
    }
    None
}

/// The visible objects of the spot's region and its neighbours.
fn nearby_objects(world: &WorldInstance, x: f32, y: f32) -> Vec<WorldObject> {
    let Some(region) = world.region_by_pos(x, y) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for neighbor in world.neighbors(region) {
        out.extend(world.objects_in_region(neighbor));
    }
    out
}

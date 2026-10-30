//! Navigation facade for non-NPC movers (the in-server player bots, `aaemu-bots`).
//!
//! The NPC AI reaches into `geodata` / `heightmap` / `collision` directly. A player-sized
//! mover that lives outside this crate needs the same three layers through one small type:
//! `.bai` routes ([`NavView::nav_route`], with the C# `FindPath` + `ReducePath` fallback),
//! heightmap heights, and the collision queries the movement code checks its packets with
//! ([`NavView::sweep_capsule`], [`NavView::ground_probe`], [`fall_velocity`],
//! [`move_verify_flags_teleport`]).
//!
//! Nothing here changes NPC behaviour: the module only *calls* the existing geodata,
//! heightmap and collision APIs.

use std::sync::Arc;

use crate::data::world_templates::WorldTemplate;
use crate::data::world_templates::REGION_SIZE;
use crate::models::unit::UnitAttribute;
use crate::server::GameServer;
use crate::systems::ai::collision;
use crate::systems::ai::geodata::astar::PathNode;
use crate::systems::ai::geodata::graph::DEFAULT_AGENT_RADIUS;
use crate::systems::skills::controllers::Vec3;
use crate::world::{now_ms, WorldObject};

/// The navigation view of one world instance for a player-sized mover.
///
/// Create it from the mover ([`NavView::of_object`]) or from an instance id
/// ([`NavView::of_instance`]); it holds the server and the instance's world template, not any
/// object lock.
pub struct NavView<'a> {
    server: &'a GameServer,
    instance_id: u32,
    template: Arc<WorldTemplate>,
    /// The zone key at the mover's position (the `.bai` fallback / waypoint selector).
    zone_key: u32,
}

impl<'a> NavView<'a> {
    /// The view of the instance a world object is in (`None` when its instance is unknown).
    pub fn of_object(server: &'a GameServer, obj: &WorldObject) -> Option<Self> {
        let instance_id = obj.transform().instance_id;
        let mut view = Self::of_instance(server, instance_id)?;
        view.zone_key = obj.transform().zone_id;
        Some(view)
    }

    /// The view of a world instance (`None` when the instance does not exist).
    pub fn of_instance(server: &'a GameServer, instance_id: u32) -> Option<Self> {
        let world = server.world.world(instance_id)?;
        Some(Self {
            server,
            instance_id,
            template: world.template.clone(),
            zone_key: 0,
        })
    }

    pub fn server(&self) -> &'a GameServer {
        self.server
    }

    pub fn instance_id(&self) -> u32 {
        self.instance_id
    }

    pub fn template_id(&self) -> u32 {
        self.template.id
    }

    /// The zone key at a world position (`None` outside the world data).
    pub fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.template.zone_key_at(
            (x as i32).div_euclid(REGION_SIZE),
            (y as i32).div_euclid(REGION_SIZE),
        )
    }

    /// The raw heightmap height (`WorldInstance.GetHeight`); `None` without height data.
    pub fn height(&self, x: f32, y: f32) -> Option<f32> {
        let heights = self.server.systems.npcs.heightmaps.get()?;
        heights.world_height(self.template.id, x, y).ok()
    }

    /// The ocean surface of this world template (the water fallback without a water volume).
    pub fn ocean_level(&self) -> f32 {
        self.template.ocean_level
    }

    /// The water depth over `ground` at `(x, y)`: a water volume's surface, else the ocean.
    pub fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        if let Some(level) = collision::water_level(self.server, self.instance_id, x, y, ground) {
            return (level - ground).max(0.0);
        }
        (self.template.ocean_level - ground).max(0.0)
    }

    /// The ground under `(x, y, z)` from the collision world (`None` without the service).
    pub fn ground_height(&self, x: f32, y: f32, z: f32) -> Option<f32> {
        collision::ground_height(self.server, self.instance_id, x, y, z)
    }

    /// A vertical capsule sweep; `None` without the collision service, `Some(None)` when the
    /// segment is free.
    pub fn sweep_capsule(
        &self,
        from: [f32; 3],
        to: [f32; 3],
        radius: f32,
        half_height: f32,
    ) -> Option<Option<collision::query::SweepHit>> {
        collision::sweep_capsule(
            self.server,
            self.instance_id,
            from,
            to,
            radius,
            half_height,
            collision::query::mask::SOLID_TERRAIN,
        )
    }

    /// The highest solid surface (static geometry or terrain) under `(x, y)` between `z + up`
    /// and `z - down`; `None` without the collision service, `Some(None)` when nothing is hit.
    pub fn ground_probe(&self, x: f32, y: f32, z: f32, up: f32, down: f32) -> Option<Option<f32>> {
        collision::ground_height_probe(self.server, self.instance_id, x, y, z, up, down)
    }

    /// The `.bai` node nearest / enclosing `pos` (`AiGeoData::resolve_nav_node`).
    pub fn resolve_nav_node(&self, pos: Vec3) -> Option<(u32, Vec3)> {
        let geo = self.server.systems.ai.geodata.get(self.template.id)?;
        let zone = self.zone_at(pos.x, pos.y).unwrap_or(self.zone_key);
        geo.resolve_nav_node(zone, pos)
    }

    /// Is there `.bai` navigation data at `pos`?
    pub fn bai_by_pos(&self, pos: Vec3) -> bool {
        self.server
            .systems
            .ai
            .geodata
            .get(self.template.id)
            .is_some_and(|geo| matches!(geo.bai_by_pos(pos), Ok(Some(_))))
    }

    /// `AiGeoData::reduce_path` (C# `ReducePath(foundPath, maxNodeSkipCount)`).
    pub fn reduce_path(&self, points: &[Vec3], max_node_skip: usize) -> Option<Vec<Vec3>> {
        let geo = self.server.systems.ai.geodata.get(self.template.id)?;
        geo.reduce_path(points, max_node_skip)
            .ok()
            .map(|r| r.into_iter().collect())
    }

    /// `AiGeoData::stick_to_floor`: the point lowered to the terrain when it is above it.
    pub fn stick_to_floor(&self, point: Vec3) -> Vec3 {
        match self.server.systems.ai.geodata.get(self.template.id) {
            Some(geo) => geo.stick_to_floor(point),
            None => point,
        }
    }

    /// A route from `from` to `to` through the navmesh: the smoothed funnel route
    /// (`find_smooth_path`) or, when that cannot give one, the C# pipeline
    /// (`PathNode::find_path` with the actor positions as ends, then `reduce_path`).
    /// The first point is `from`, the last `to`; `None` when the `.bai` data gives no route.
    pub fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        let geo = self.server.systems.ai.geodata.get(self.template.id)?;
        let zone = self.zone_at(from.x, from.y).unwrap_or(self.zone_key);
        let mut node = PathNode {
            zone_key: zone,
            ..PathNode::default()
        };
        if let Ok(Some(smooth)) = node.find_smooth_path(&geo, from, to, DEFAULT_AGENT_RADIUS) {
            if smooth.points.len() >= 2 {
                return Some(smooth.points);
            }
        }
        let mut points = node.find_path(&geo, from, to).ok()?;
        if points.is_empty() {
            return None;
        }
        points[0] = from;
        match points.last_mut() {
            Some(last) => *last = to,
            None => points.push(to),
        }
        let reduced = geo.reduce_path(&points, 10).ok()?;
        Some(reduced.into_iter().collect())
    }
}

/// The unit's current `MoveSpeedMul` (`CalculateWithBonuses(1000, MoveSpeedMul) / 1000`):
/// slows, trade packs and buffs included.
pub fn movement_speed_mul(obj: &WorldObject) -> f32 {
    speed_mul(obj, UnitAttribute::MoveSpeedMul)
}

/// The unit's current `SwimSpeedMul` (`CalculateWithBonuses(1000, SwimSpeedMul) / 1000`).
pub fn swim_speed_mul(obj: &WorldObject) -> f32 {
    speed_mul(obj, UnitAttribute::SwimSpeedMul)
}

fn speed_mul(obj: &WorldObject, attribute: UnitAttribute) -> f32 {
    let now = now_ms();
    obj.with_unit(|u| (u.unit().calculate_with_bonuses(1000.0, attribute, now) as f32) / 1000.0)
        .unwrap_or(1.0)
}

/// The client `fallVel` (u16) of a drop of `height` metres, as the zone's `ZWUnitFell`
/// computes it (`sqrt(2 * 20 * h)` in client units).
pub fn fall_velocity(height: f32) -> u16 {
    (collision::fall::height_velocity(height) * collision::fall::FALL_VEL_UNITS_PER_MPS)
        .min(f32::from(u16::MAX)) as u16
}

/// Would the zone's movement verifier (`ZWReportMoveHack` reason 15/16/17) classify a move of
/// `distance` metres in `dt_s` seconds as a teleport? Packet builders use this to keep the
/// sample rate and step length a real client's.
pub fn move_verify_flags_teleport(distance: f32, dt_s: f32) -> bool {
    collision::move_verify::teleport_class(distance, dt_s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fall_velocity_matches_the_zone_curve() {
        // 9 m: sqrt(2 * 20 * 9) * units/mps, the client's own value for a damage-free drop.
        let v = fall_velocity(9.0);
        let want = (360.0f32.sqrt() * collision::fall::FALL_VEL_UNITS_PER_MPS) as u16;
        assert_eq!(v, want);
        assert_eq!(fall_velocity(0.0), 0);
    }

    #[test]
    fn a_run_step_is_not_a_teleport_but_a_jump_across_the_map_is() {
        // 0.54 m per 100 ms (run speed 5.4 m/s).
        assert!(!move_verify_flags_teleport(0.54, 0.1));
        assert!(move_verify_flags_teleport(100.0, 0.1));
    }
}

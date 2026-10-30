//! Places a bot learned to stay away from: where it was attacked by a monster far above its
//! level ([`DangerMark`]), and where such monsters live ([`DangerIndex`]).
//!
//! This is the port of the old `aaemu-game/src/systems/bots/danger.rs` with its spawn-data index
//! builder removed: the authored-spawn index needs the NPC-role / spawn knowledge hook
//! (BOTS-PLAN hook 4), so P2 builds the index from explicit threats ([`DangerIndex::from_monsters`])
//! and leaves [`DangerIndex::add`] for that hook. The travel planner consumes the same
//! [`DangerView`] / [`DangerAvoid`] world view as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::controllers::Vec3;
use aaemu_game::systems::skills::relation::faction_relation;
use aaemu_game::systems::skills::types::RelationState;

use super::{NavWorld, Obstacle};

/// The radius of the area marked around an attacker.
pub const MARK_RADIUS_M: f32 = 40.0;
/// Size of an index cell (metres).
pub const CELL_M: f32 = 16.0;
/// A segment is checked for danger every this many metres.
const SAMPLE_M: f32 = 8.0;
/// Danger within this distance of a route's start or target does not count (the bot has to get
/// out of, or into, the place it is going to).
pub const FREE_RADIUS_M: f32 = 40.0;

/// An area held by monsters too strong for the bot, learned when one attacked it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DangerMark {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    /// The level of the monster that attacked there.
    pub level: u8,
}

impl DangerMark {
    /// A mark stays until the bot has grown into the monster: it is dropped once the monster is
    /// no more than `gap` levels above the bot.
    pub fn is_live(&self, bot_level: u8, gap: u8) -> bool {
        self.level > bot_level.saturating_add(gap)
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        (self.x - x).powi(2) + (self.y - y).powi(2) <= self.radius * self.radius
    }
}

/// Cell -> highest level of the aggressive monsters (hostile to one faction) authored there.
type Grid = HashMap<(i32, i32), u8>;
/// Cell -> `(monster faction, highest level)`.
type FactionCells = HashMap<(i32, i32), Vec<(u32, u8)>>;

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    ((x / CELL_M).floor() as i32, (y / CELL_M).floor() as i32)
}

/// The known aggressive monsters of every world, by cell and faction.
#[derive(Debug, Default)]
pub struct DangerIndex {
    /// world -> cell -> `(monster faction, highest level)`.
    cells: HashMap<String, FactionCells>,
    /// `(world, bot faction)` -> the grid of monsters hostile to that faction.
    grids: Mutex<HashMap<(String, u32), Arc<Grid>>>,
}

impl DangerIndex {
    /// Adds one threat at `(x, y)` in `world`.
    pub fn add(&mut self, world: &str, x: f32, y: f32, faction: u32, level: u8) {
        let cell = self
            .cells
            .entry(world.to_string())
            .or_default()
            .entry(cell_of(x, y))
            .or_default();
        match cell.iter_mut().find(|(f, _)| *f == faction) {
            Some((_, l)) => *l = (*l).max(level),
            None => cell.push((faction, level)),
        }
    }

    /// An index of explicit `(world, x, y, faction, level)` monsters (tests, and the NPC-role
    /// hook that fills it from the authored spawns in a later phase).
    pub fn from_monsters(monsters: &[(&str, f32, f32, u32, u8)]) -> Self {
        let mut index = Self::default();
        for (world, x, y, faction, level) in monsters {
            index.add(world, *x, *y, *faction, *level);
        }
        index
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The grid of the monsters of `world` that are hostile to `faction`.
    fn grid(&self, server: &GameServer, world: &str, faction: u32) -> Arc<Grid> {
        let key = (world.to_string(), faction);
        if let Some(grid) = self.grids.lock().expect("danger grids poisoned").get(&key) {
            return grid.clone();
        }
        let mut hostile: HashMap<u32, bool> = HashMap::new();
        let mut grid = Grid::new();
        for (cell, monsters) in self.cells.get(world).into_iter().flatten() {
            for (monster_faction, level) in monsters {
                let is_hostile = *hostile.entry(*monster_faction).or_insert_with(|| {
                    faction_relation(server, faction, *monster_faction) == RelationState::Hostile
                });
                if is_hostile {
                    let entry = grid.entry(*cell).or_insert(0);
                    *entry = (*entry).max(*level);
                }
            }
        }
        let grid = Arc::new(grid);
        self.grids
            .lock()
            .expect("danger grids poisoned")
            .insert(key, grid.clone());
        grid
    }

    /// What a bot of `bot_level` and `faction` in `world` avoids: the monsters more than `gap`
    /// levels above it plus its own marks.
    pub fn view(
        &self,
        server: &GameServer,
        world: &str,
        faction: u32,
        bot_level: u8,
        gap: u8,
        marks: Vec<DangerMark>,
    ) -> DangerView {
        DangerView {
            grid: self.grid(server, world, faction),
            marks,
            threshold: bot_level.saturating_add(gap),
        }
    }
}

/// The dangerous places of one bot at its current level.
#[derive(Debug, Clone)]
pub struct DangerView {
    grid: Arc<Grid>,
    marks: Vec<DangerMark>,
    /// Monsters above this level are dangerous.
    threshold: u8,
}

impl DangerView {
    /// A view of explicit cells (tests).
    pub fn of(cells: &[((i32, i32), u8)], marks: Vec<DangerMark>, threshold: u8) -> Self {
        Self {
            grid: Arc::new(cells.iter().copied().collect()),
            marks,
            threshold,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.grid.is_empty() && self.marks.is_empty()
    }

    /// Is `(x, y)` within reach of a monster above the threshold (its own 16 m cell or a
    /// neighbouring one) or inside a mark?
    pub fn danger_at(&self, x: f32, y: f32) -> bool {
        if self.marks.iter().any(|m| m.contains(x, y)) {
            return true;
        }
        let (cx, cy) = cell_of(x, y);
        (-1..=1).any(|dx| {
            (-1..=1).any(|dy| {
                self.grid
                    .get(&(cx + dx, cy + dy))
                    .is_some_and(|level| *level > self.threshold)
            })
        })
    }

    /// Does the straight segment pass through danger outside the `free` circles
    /// `(x, y, radius)`?
    pub fn segment_danger(&self, a: (f32, f32), b: (f32, f32), free: &[(f32, f32, f32)]) -> bool {
        if self.is_empty() {
            return false;
        }
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length = (dx * dx + dy * dy).sqrt();
        let n = (length / SAMPLE_M).ceil().max(1.0) as usize;
        (0..=n).any(|i| {
            let t = i as f32 / n as f32;
            let (x, y) = (a.0 + dx * t, a.1 + dy * t);
            !free
                .iter()
                .any(|(fx, fy, r)| (fx - x).powi(2) + (fy - y).powi(2) <= r * r)
                && self.danger_at(x, y)
        })
    }
}

/// A world view that refuses to cross danger (outside the free circles around the route's ends),
/// so the planners find a way around it or none.
pub struct DangerAvoid<'a> {
    pub world: &'a dyn NavWorld,
    pub view: &'a DangerView,
    pub free: Vec<(f32, f32, f32)>,
}

impl<'a> DangerAvoid<'a> {
    /// Danger around the start and the target of `from` -> `to` does not count.
    pub fn new(world: &'a dyn NavWorld, view: &'a DangerView, from: Vec3, to: Vec3) -> Self {
        Self {
            world,
            view,
            free: vec![(from.x, from.y, FREE_RADIUS_M), (to.x, to.y, FREE_RADIUS_M)],
        }
    }
}

impl NavWorld for DangerAvoid<'_> {
    fn height(&self, x: f32, y: f32) -> Option<f32> {
        self.world.height(x, y)
    }

    fn water_depth(&self, x: f32, y: f32, ground: f32) -> f32 {
        self.world.water_depth(x, y, ground)
    }

    fn nav_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.world.nav_route(from, to)
    }

    fn road_route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.world.road_route(from, to)
    }

    fn obstacle(&self, from: Vec3, to: Vec3) -> Option<Obstacle> {
        if self
            .view
            .segment_danger((from.x, from.y), (to.x, to.y), &self.free)
        {
            return Some(Obstacle::High(FREE_RADIUS_M));
        }
        self.world.obstacle(from, to)
    }

    fn zone_at(&self, x: f32, y: f32) -> Option<u32> {
        self.world.zone_at(x, y)
    }
}

/// A hostile ground zone: a boss area attack or an area trigger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hazard {
    Circle {
        x: f32,
        y: f32,
        radius: f32,
    },
    /// A rotated box: centre, half extents along its own axes and the rotation (radians).
    Box {
        x: f32,
        y: f32,
        half_x: f32,
        half_y: f32,
        rotation: f32,
    },
}

impl Hazard {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        match *self {
            Hazard::Circle { x, y, radius } => (px - x).hypot(py - y) <= radius,
            Hazard::Box {
                x,
                y,
                half_x,
                half_y,
                rotation,
            } => {
                let (lx, ly) = to_local(px - x, py - y, rotation);
                lx.abs() <= half_x && ly.abs() <= half_y
            }
        }
    }

    /// The shortest way out of the zone: the point on its edge nearest to `(px, py)`, pushed
    /// `margin` metres beyond it (straight out of a circle, through the nearest face of a box).
    /// A bot outside the zone stays where it is.
    pub fn exit_point(&self, px: f32, py: f32, margin: f32) -> (f32, f32) {
        if !self.contains(px, py) {
            return (px, py);
        }
        match *self {
            Hazard::Circle { x, y, radius } => {
                let (dx, dy) = (px - x, py - y);
                let d = dx.hypot(dy);
                // At the very centre every direction is as short: go east.
                let (ux, uy) = if d < 1e-3 {
                    (1.0, 0.0)
                } else {
                    (dx / d, dy / d)
                };
                (x + ux * (radius + margin), y + uy * (radius + margin))
            }
            Hazard::Box {
                x,
                y,
                half_x,
                half_y,
                rotation,
            } => {
                let (lx, ly) = to_local(px - x, py - y, rotation);
                // Distance to each face; leave through the nearest one.
                let faces = [
                    (half_x - lx, (1.0, 0.0)),
                    (half_x + lx, (-1.0, 0.0)),
                    (half_y - ly, (0.0, 1.0)),
                    (half_y + ly, (0.0, -1.0)),
                ];
                let (_, (nx, ny)) = faces
                    .iter()
                    .copied()
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .expect("four faces");
                let (ex, ey) = if nx != 0.0 {
                    (nx * (half_x + margin), ly)
                } else {
                    (lx, ny * (half_y + margin))
                };
                let (wx, wy) = from_local(ex, ey, rotation);
                (x + wx, y + wy)
            }
        }
    }
}

fn to_local(dx: f32, dy: f32, rotation: f32) -> (f32, f32) {
    let (s, c) = rotation.sin_cos();
    (dx * c + dy * s, -dx * s + dy * c)
}

fn from_local(lx: f32, ly: f32, rotation: f32) -> (f32, f32) {
    let (s, c) = rotation.sin_cos();
    (lx * c - ly * s, lx * s + ly * c)
}

/// The point to step to so as to leave every hazard the bot stands in: the exit of the first
/// zone it is in, repeated until the bot is outside all of them (a zone inside another is left
/// through the outer one).
pub fn escape_hazards(hazards: &[Hazard], mut px: f32, mut py: f32, margin: f32) -> (f32, f32) {
    for _ in 0..hazards.len().max(1) {
        let Some(zone) = hazards.iter().find(|h| h.contains(px, py)) else {
            break;
        };
        (px, py) = zone.exit_point(px, py, margin);
    }
    (px, py)
}

#[cfg(test)]
mod hazard_tests {
    use super::*;

    #[test]
    fn a_circle_is_left_straight_out_by_the_shortest_way() {
        let h = Hazard::Circle {
            x: 100.0,
            y: 100.0,
            radius: 10.0,
        };
        // 3 m east of the centre: the edge is 7 m away to the east.
        let (x, y) = h.exit_point(103.0, 100.0, 1.0);
        assert!(
            (x - 111.0).abs() < 0.01 && (y - 100.0).abs() < 0.01,
            "{x} {y}"
        );
        assert!(!h.contains(x, y));
        // Outside: unchanged.
        assert_eq!(h.exit_point(120.0, 100.0, 1.0), (120.0, 100.0));
    }

    #[test]
    fn a_box_is_left_through_its_nearest_face() {
        let h = Hazard::Box {
            x: 0.0,
            y: 0.0,
            half_x: 10.0,
            half_y: 2.0,
            rotation: 0.0,
        };
        // Near the north face of a long thin box.
        let (x, y) = h.exit_point(5.0, 1.5, 0.5);
        assert!((x - 5.0).abs() < 0.01 && (y - 2.5).abs() < 0.01, "{x} {y}");
        // The same box turned a quarter circle: its long side runs north-south.
        let turned = Hazard::Box {
            x: 0.0,
            y: 0.0,
            half_x: 10.0,
            half_y: 2.0,
            rotation: std::f32::consts::FRAC_PI_2,
        };
        let (x, y) = turned.exit_point(1.5, 5.0, 0.5);
        assert!((x - 2.5).abs() < 0.05 && (y - 5.0).abs() < 0.05, "{x} {y}");
        assert!(!turned.contains(x, y));
    }

    #[test]
    fn nested_zones_are_left_through_all_of_them() {
        let zones = [
            Hazard::Circle {
                x: 0.0,
                y: 0.0,
                radius: 5.0,
            },
            Hazard::Circle {
                x: 0.0,
                y: 0.0,
                radius: 12.0,
            },
        ];
        let (x, y) = escape_hazards(&zones, 1.0, 0.0, 1.0);
        assert!(zones.iter().all(|z| !z.contains(x, y)), "{x} {y}");
    }
}

//! Long-range routing over the transfer roads the server already loads.
//!
//! The 1.2 client's `transfer_path.xml` files hold the road polylines the carriages and
//! airships follow; `systems/vehicles/data.rs` loads them into
//! `game_data.systems.vehicles.transfer_templates[*].transfer_roads`. A route between two far
//! points walks the road network (A* with a Euclidean heuristic, edge weight = 3D length) and
//! leaves the first and last leg to the normal `.bai` / height-grid planner (see
//! [`super::plan`]).
//!
//! Join / validation rules (BOTS-PLAN "Long-range routing"):
//! * road endpoints within 2 m planar and 1.5 m vertical are joined (one graph node);
//! * a segment of one polyline is rejected when its gap is over 100 m, it steps vertically
//!   more than 15 m, or its slope is over 2.0;
//! * every edge is walkable in both directions (the roads are carriage routes, a player can
//!   walk them either way).

use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};

use aaemu_game::server::GameServer;
use aaemu_game::systems::skills::controllers::Vec3;

/// Endpoints this close (planar) and this close vertically are joined.
pub const JOIN_PLANAR_M: f32 = 2.0;
pub const JOIN_VERTICAL_M: f32 = 1.5;
/// A gap between two consecutive road points larger than this is rejected.
pub const MAX_GAP_M: f32 = 100.0;
/// A vertical step larger than this is rejected.
pub const MAX_VERTICAL_STEP_M: f32 = 15.0;
/// A segment steeper than this (`|dz| / planar`) is rejected.
pub const MAX_SLOPE: f32 = 2.0;
/// How far a bot may be from the road network and still use it: both access legs must be
/// short relative to the trip, otherwise the "road" is a kilometre detour.
pub const MAX_SNAP_M: f32 = 150.0;
/// Node expansions one road A* may spend.
pub const MAX_EXPANSIONS: usize = 200_000;

/// The road graph of one world template.
#[derive(Debug, Default)]
pub struct RoadGraph {
    points: Vec<Vec3>,
    /// `(node, 3D length)` per node.
    adjacent: Vec<Vec<(u32, f32)>>,
    /// Connected component id per node (a route only joins nodes of the same component).
    component: Vec<u32>,
}

impl RoadGraph {
    /// Builds the graph from road polylines. Consecutive points of one polyline become nodes
    /// and edges when they pass the rules; endpoints within the join tolerance share a node.
    pub fn from_roads<I, P>(roads: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: IntoIterator<Item = Vec3>,
    {
        let mut graph = RoadGraph::default();
        // Endpoint nodes by 2 m cell, to merge joins without an O(roads²) scan.
        let mut endpoint_cells: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for road in roads {
            let polyline: Vec<Vec3> = road.into_iter().collect();
            let mut chain: Vec<u32> = Vec::with_capacity(polyline.len());
            let last = polyline.len().saturating_sub(1);
            for (i, p) in polyline.iter().copied().enumerate() {
                if i > 0 && dist(p, polyline[i - 1]) < 0.01 {
                    continue;
                }
                let is_endpoint = i == 0 || i == last;
                let node = if is_endpoint {
                    let cell = (
                        (p.x / JOIN_PLANAR_M).floor() as i32,
                        (p.y / JOIN_PLANAR_M).floor() as i32,
                    );
                    let mut found = None;
                    'cells: for dx in -1..=1 {
                        for dy in -1..=1 {
                            for &n in endpoint_cells
                                .get(&(cell.0 + dx, cell.1 + dy))
                                .into_iter()
                                .flatten()
                            {
                                let q = graph.points[n as usize];
                                let planar = ((q.x - p.x).powi(2) + (q.y - p.y).powi(2)).sqrt();
                                if planar <= JOIN_PLANAR_M && (q.z - p.z).abs() <= JOIN_VERTICAL_M {
                                    found = Some(n);
                                    break 'cells;
                                }
                            }
                        }
                    }
                    match found {
                        Some(n) => n,
                        None => {
                            let n = graph.add_node(p);
                            endpoint_cells.entry(cell).or_default().push(n);
                            n
                        }
                    }
                } else {
                    graph.add_node(p)
                };
                chain.push(node);
            }
            for pair in chain.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                if a == b {
                    continue;
                }
                if let Some(len) = edge_length(graph.points[a as usize], graph.points[b as usize]) {
                    graph.adjacent[a as usize].push((b, len));
                    graph.adjacent[b as usize].push((a, len));
                }
            }
        }
        graph.compute_components();
        graph
    }

    fn compute_components(&mut self) {
        self.component = vec![u32::MAX; self.points.len()];
        let mut next = 0u32;
        let mut stack = Vec::new();
        for start in 0..self.points.len() {
            if self.component[start] != u32::MAX {
                continue;
            }
            stack.push(start as u32);
            self.component[start] = next;
            while let Some(node) = stack.pop() {
                for &(other, _) in &self.adjacent[node as usize] {
                    if self.component[other as usize] == u32::MAX {
                        self.component[other as usize] = next;
                        stack.push(other);
                    }
                }
            }
            next += 1;
        }
    }

    /// The road graph of a world template from the loaded transfer templates. `None` when the
    /// server has no road data for that template (no `transfer_path.xml` / no paths in the DB).
    pub fn from_server(server: &GameServer, template_id: u32) -> Option<Self> {
        let mut roads: Vec<Vec<Vec3>> = Vec::new();
        for template in server
            .game_data
            .systems
            .vehicles
            .transfer_templates
            .values()
        {
            // Airships / boats / floating platforms are not walkable ground routes.
            let ground = server
                .game_data
                .systems
                .vehicles
                .vehicle_model(template.model_id)
                .is_some_and(|model| model.ground);
            if !ground {
                continue;
            }
            for road in &template.transfer_roads {
                let points: Vec<Vec3> = road
                    .pos
                    .iter()
                    .filter(|p| p.world_id == template_id || p.world_id == 0)
                    .map(|p| Vec3::new(p.x, p.y, p.z))
                    .collect();
                if points.len() >= 2 {
                    roads.push(points);
                }
            }
        }
        if roads.is_empty() {
            return None;
        }
        Some(Self::from_roads(roads))
    }

    fn add_node(&mut self, p: Vec3) -> u32 {
        let n = self.points.len() as u32;
        self.points.push(p);
        self.adjacent.push(Vec::new());
        n
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn edges(&self) -> usize {
        self.adjacent.iter().map(Vec::len).sum::<usize>() / 2
    }

    pub fn points(&self) -> &[Vec3] {
        &self.points
    }

    /// Any two positions joined by an edge (real-data smoke checks / diagnostics).
    pub fn any_edge(&self) -> Option<(Vec3, Vec3)> {
        self.adjacent.iter().enumerate().find_map(|(a, edges)| {
            edges
                .first()
                .map(|&(b, _)| (self.points[a], self.points[b as usize]))
        })
    }

    /// The graph node nearest to `p` and its planar distance.
    pub fn nearest(&self, p: Vec3) -> Option<(u32, f32)> {
        self.points
            .iter()
            .enumerate()
            .map(|(i, q)| (i as u32, ((q.x - p.x).powi(2) + (q.y - p.y).powi(2)).sqrt()))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// The nearest node to `p` in every connected component, with its planar distance.
    fn nearest_by_component(&self, p: Vec3) -> HashMap<u32, (u32, f32)> {
        let mut best: HashMap<u32, (u32, f32)> = HashMap::new();
        for (i, q) in self.points.iter().enumerate() {
            let d = ((q.x - p.x).powi(2) + (q.y - p.y).powi(2)).sqrt();
            let component = self.component[i];
            let entry = best.entry(component).or_insert((i as u32, d));
            if d < entry.1 {
                *entry = (i as u32, d);
            }
        }
        best
    }

    /// The pair of access nodes (one near `from`, one near `to`) in the same connected
    /// component with the shortest total access, both within [`MAX_SNAP_M`].
    fn access_nodes(&self, from: Vec3, to: Vec3) -> Option<(u32, u32)> {
        let from_nodes = self.nearest_by_component(from);
        let to_nodes = self.nearest_by_component(to);
        from_nodes
            .iter()
            .filter(|(_, (_, d))| *d <= MAX_SNAP_M)
            .filter_map(|(component, (start, d_start))| {
                let (goal, d_goal) = *to_nodes.get(component)?;
                (d_goal <= MAX_SNAP_M).then_some((*start, goal, d_start + d_goal))
            })
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(start, goal, _)| (start, goal))
    }

    /// A route between the access nodes nearest to `from` and `to` in one component: the road
    /// node positions, both ends included. `None` when the network cannot connect them (or
    /// the ends are too far from any road).
    pub fn route(&self, from: Vec3, to: Vec3) -> Option<Vec<Vec3>> {
        self.route_budget(from, to, &mut super::PlanBudget::new())
    }

    /// [`RoadGraph::route`] charged against the plan budget.
    pub fn route_budget(
        &self,
        from: Vec3,
        to: Vec3,
        budget: &mut super::PlanBudget,
    ) -> Option<Vec<Vec3>> {
        self.route_avoiding(from, to, &[], budget)
    }

    /// [`RoadGraph::route_budget`] without the `banned` edges (pairs of node positions, either
    /// direction): the planner drops the edges its terrain check rejected and asks again.
    pub fn route_avoiding(
        &self,
        from: Vec3,
        to: Vec3,
        banned: &[(Vec3, Vec3)],
        budget: &mut super::PlanBudget,
    ) -> Option<Vec<Vec3>> {
        if self.is_empty() {
            return None;
        }
        let (start, goal) = self.access_nodes(from, to)?;
        let (start, goal) = (start as usize, goal as usize);
        if start == goal {
            // Both ends snap to the same road node: the road is no help, walking straight is.
            return None;
        }
        let goal_pos = self.points[goal];
        let heuristic = |i: usize| dist(self.points[i], goal_pos);
        let mut g = vec![f32::INFINITY; self.points.len()];
        let mut parent = vec![u32::MAX; self.points.len()];
        let mut closed = vec![false; self.points.len()];
        let mut open: BinaryHeap<(std::cmp::Reverse<u64>, u32)> = BinaryHeap::new();
        g[start] = 0.0;
        open.push((
            std::cmp::Reverse((heuristic(start) * 100.0) as u64),
            start as u32,
        ));
        let mut expansions = 0usize;
        while let Some((_, current)) = open.pop() {
            if !budget.tick() {
                return None;
            }
            let current = current as usize;
            if closed[current] {
                continue;
            }
            if current == goal {
                let mut route = vec![self.points[current]];
                let mut node = current;
                while parent[node] != u32::MAX {
                    node = parent[node] as usize;
                    route.push(self.points[node]);
                }
                route.reverse();
                return Some(route);
            }
            closed[current] = true;
            expansions += 1;
            if expansions > MAX_EXPANSIONS {
                return None;
            }
            for &(next, len) in &self.adjacent[current] {
                let next = next as usize;
                if closed[next] {
                    continue;
                }
                if !banned.is_empty() && is_banned(banned, self.points[current], self.points[next])
                {
                    continue;
                }
                let ng = g[current] + len;
                if ng + 1e-4 < g[next] {
                    g[next] = ng;
                    parent[next] = current as u32;
                    open.push((
                        std::cmp::Reverse(((ng + heuristic(next)) * 100.0) as u64),
                        next as u32,
                    ));
                }
            }
        }
        None
    }
}

/// The road graphs of one server, one per world template, shared by that server's bots: a
/// clone is another handle on the same graphs, and a server (the bots host, or a bot built on
/// its own) makes its own cache, so a graph is never taken from another server's game data
/// (the cache is not a process global). A graph is built the first time a bot needs its
/// template; a template without road data is not cached, so it is asked again.
#[derive(Debug, Clone, Default)]
pub struct RoadGraphCache {
    graphs: Arc<Mutex<HashMap<u32, Arc<RoadGraph>>>>,
}

impl RoadGraphCache {
    /// The graph of `template_id`, built once from the server's loaded road data (`None`
    /// without road data for the template).
    pub fn get(&self, server: &GameServer, template_id: u32) -> Option<Arc<RoadGraph>> {
        let mut graphs = self.graphs.lock().expect("road graph cache poisoned");
        if let Some(graph) = graphs.get(&template_id) {
            return Some(graph.clone());
        }
        let graph = Arc::new(RoadGraph::from_server(server, template_id)?);
        graphs.insert(template_id, graph.clone());
        Some(graph)
    }
}

/// Is the edge `a`-`b` one of the `banned` ones (node positions come from this graph, so they
/// compare exactly)?
fn is_banned(banned: &[(Vec3, Vec3)], a: Vec3, b: Vec3) -> bool {
    banned
        .iter()
        .any(|&(x, y)| (x == a && y == b) || (x == b && y == a))
}

/// A road edge between two consecutive polyline points, or why it is rejected.
fn edge_length(a: Vec3, b: Vec3) -> Option<f32> {
    let planar = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
    let dz = (b.z - a.z).abs();
    if planar > MAX_GAP_M || dz > MAX_VERTICAL_STEP_M {
        return None;
    }
    if planar > 0.01 && dz / planar > MAX_SLOPE {
        return None;
    }
    Some((planar * planar + dz * dz).sqrt())
}

fn dist(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3::new(x, y, z)
    }

    #[test]
    fn two_roads_sharing_an_endpoint_connect() {
        let graph = RoadGraph::from_roads([
            vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)],
            vec![at(100.0, 0.0, 100.0), at(100.0, 100.0, 100.0)],
        ]);
        assert_eq!(graph.len(), 3, "the shared endpoint is one node");
        let route = graph
            .route(at(0.0, 0.0, 100.0), at(100.0, 100.0, 100.0))
            .expect("route");
        assert_eq!(route.first(), Some(&at(0.0, 0.0, 100.0)));
        assert_eq!(route.last(), Some(&at(100.0, 100.0, 100.0)));
        assert_eq!(route.len(), 3);
    }

    #[test]
    fn endpoints_join_within_the_tolerance_only() {
        let near = RoadGraph::from_roads([
            vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)],
            // 1.5 m east, 1 m up: within 2 m planar / 1.5 m vertical.
            vec![at(101.0, 0.0, 101.0), at(101.0, 99.5, 101.0)],
        ]);
        assert_eq!(near.len(), 3, "joined");
        assert!(near
            .route(at(0.0, 0.0, 100.0), at(101.0, 99.5, 101.0))
            .is_some());
        let far = RoadGraph::from_roads([
            vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)],
            // 3 m east: too far to join.
            vec![at(103.0, 0.0, 100.0), at(103.0, 100.0, 100.0)],
        ]);
        assert_eq!(far.len(), 4);
        // 103 did not join: the route snaps to the nearest node of the other end's component.
        let route = far
            .route(at(0.0, 0.0, 100.0), at(103.0, 100.0, 100.0))
            .expect("route through the joined component");
        assert_eq!(route, vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)]);
    }

    #[test]
    fn a_gap_over_100_m_breaks_the_road() {
        let graph = RoadGraph::from_roads([vec![
            at(0.0, 0.0, 100.0),
            at(120.0, 0.0, 100.0),
            at(220.0, 0.0, 100.0),
        ]]);
        assert_eq!(graph.edges(), 1, "the 120 m gap is rejected");
        // The gap splits the graph; the route reaches the far component by its access leg.
        let route = graph
            .route(at(0.0, 0.0, 100.0), at(220.0, 0.0, 100.0))
            .expect("route to the reachable component");
        assert_eq!(route, vec![at(120.0, 0.0, 100.0), at(220.0, 0.0, 100.0)]);
    }

    #[test]
    fn vertical_steps_and_slopes_are_rejected() {
        // 20 m step over 50 m planar: a cliff, rejected.
        let step = RoadGraph::from_roads([vec![
            at(0.0, 0.0, 100.0),
            at(50.0, 0.0, 120.0),
            at(100.0, 0.0, 120.0),
        ]]);
        assert_eq!(step.edges(), 1);
        let route = step
            .route(at(0.0, 0.0, 100.0), at(100.0, 0.0, 120.0))
            .expect("route through the flat part");
        assert_eq!(route.first(), Some(&at(50.0, 0.0, 120.0)));
        // 10 m up over 4 m planar: slope 2.5, rejected.
        let steep = RoadGraph::from_roads([vec![
            at(0.0, 0.0, 100.0),
            at(4.0, 0.0, 110.0),
            at(100.0, 0.0, 110.0),
        ]]);
        assert_eq!(steep.edges(), 1);
        let route = steep
            .route(at(0.0, 0.0, 100.0), at(100.0, 0.0, 110.0))
            .expect("route through the flat part");
        assert_eq!(route.first(), Some(&at(4.0, 0.0, 110.0)));
    }

    #[test]
    fn a_star_prefers_the_shorter_way_round() {
        // Two ways from (0,0) to (100,0): a 200 m detour north and a 100 m direct road.
        let graph = RoadGraph::from_roads([
            vec![
                at(0.0, 0.0, 100.0),
                at(0.0, 100.0, 100.0),
                at(100.0, 100.0, 100.0),
                at(100.0, 0.0, 100.0),
            ],
            vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)],
        ]);
        let route = graph
            .route(at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0))
            .expect("route");
        assert_eq!(route, vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)]);
    }

    /// The world template of the test road.
    const ROAD_TEMPLATE: u32 = 5;

    /// A server whose game data holds one ground transfer road in [`ROAD_TEMPLATE`] (or none).
    fn server_with_roads(with_road: bool) -> GameServer {
        use aaemu_game::data::world_templates::WorldSpawnPosition;
        use aaemu_game::data::GameData;
        use aaemu_game::systems::vehicles::data::{TransferRoad, TransferTemplate, VehicleModel};

        let mut data = GameData::empty();
        if with_road {
            let vehicles = &mut data.systems.vehicles;
            vehicles.model_types.insert(7, (7, "VehicleModel".into()));
            vehicles.vehicle_models.insert(
                7,
                VehicleModel {
                    id: 7,
                    ground: true,
                    ..VehicleModel::default()
                },
            );
            let point = |x: f32| WorldSpawnPosition {
                world_id: ROAD_TEMPLATE,
                x,
                y: 10.0,
                z: 100.0,
                ..WorldSpawnPosition::default()
            };
            vehicles.transfer_templates.insert(
                1,
                TransferTemplate {
                    id: 1,
                    model_id: 7,
                    transfer_roads: vec![TransferRoad {
                        pos: vec![point(0.0), point(50.0), point(100.0)],
                        ..TransferRoad::default()
                    }],
                    ..TransferTemplate::default()
                },
            );
        }
        aaemu_game::test_support::test_server_with(data)
    }

    /// The cache is per server, not a process global: a template without road data is not
    /// cached (a server that has the data gets its graph from the same cache), and a cache
    /// never hands out the graph another cache built from another server's data.
    #[tokio::test]
    async fn the_road_cache_belongs_to_its_server_and_does_not_keep_a_missing_graph() {
        let without = server_with_roads(false);
        let with = server_with_roads(true);

        let shared = RoadGraphCache::default();
        assert!(
            shared.get(&without, ROAD_TEMPLATE).is_none(),
            "no road data"
        );
        let graph = shared
            .get(&with, ROAD_TEMPLATE)
            .expect("the earlier miss was not cached");
        assert_eq!(graph.len(), 3);
        // A clone is another handle on the same graphs ...
        let again = shared.clone().get(&with, ROAD_TEMPLATE).expect("cached");
        assert!(Arc::ptr_eq(&graph, &again));
        // ... a new cache (another server / host / bot) builds its own.
        let other = RoadGraphCache::default();
        assert!(other.get(&without, ROAD_TEMPLATE).is_none());
        let other_graph = other
            .get(&with, ROAD_TEMPLATE)
            .expect("built from its own server");
        assert!(!Arc::ptr_eq(&graph, &other_graph));
        // Another template of the same server has no roads.
        assert!(shared.get(&with, 0).is_none());
    }

    #[test]
    fn a_bot_far_from_any_road_gets_no_road_route() {
        let graph = RoadGraph::from_roads([vec![at(0.0, 0.0, 100.0), at(100.0, 0.0, 100.0)]]);
        assert!(graph
            .route(at(5_000.0, 0.0, 100.0), at(100.0, 0.0, 100.0))
            .is_none());
    }
}

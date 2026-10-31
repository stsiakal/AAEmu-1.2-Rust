//! House placement through `CSCreateHouse` (`ops::build`) on a copy of the original
//! `w_solzreed_3` area 338 polygon (design 171 / item 21165).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aaemu_physics::math::Vec3;

use super::data::{HousingGroup, HousingItemHousings, HousingTemplate, Taxation};
use super::hooks::{slot_type, HouseConstructionError, HouseItem, HousingEconomyHooks};
use super::manager;
use super::ops;
use super::packets::CSCreateHousePacket;
use crate::config::AppConfig;
use crate::data::items::ItemTemplate;
use crate::data::world_data::Zone;
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::ai::collision::terrain::tests::Plane;
use crate::systems::ai::collision::world::CellContent;
use crate::systems::npcs::heightmap::{
    HeightmapConfig, HeightmapWorlds, WorldHeightmap, CELL_HMAP_RESOLUTION,
};
use crate::systems::npcs::subzone::Area;
use crate::systems::skills::data::BuffTemplate;
use crate::test_support::*;
use crate::world::PlayerRef;
use sqlx::mysql::MySqlPoolOptions;

const DESIGN: u32 = 171;
const DESIGN_ITEM: u32 = 21165;
const AREA: u32 = 338;
const GROUP: u32 = 15;
/// The client's own `housing_area.xml` polygon for area 338 (`w_solzreed_3`) in world
/// coordinates (capture `2026-10-31-capture-housing-transport`).
const AREA_338: [[f32; 3]; 7] = [
    [15341.25, 14645.31, 0.],
    [15357.68, 14644.92, 0.],
    [15357.28, 14626.86, 0.],
    [15342.19, 14626.77, 0.],
    [15342.62, 14618.79, 0.],
    [15314.22, 14618.63, 0.],
    [15314.44, 14645.04, 0.],
];
/// The captured spot the original client refuses (24 m garden footprint does not fit).
const REFUSED: (f32, f32) = (15338.53, 14632.33);
/// A spot that fits the footprint with ~1 m margin.
const ACCEPTED: (f32, f32) = (15328.4, 14631.8);

struct TestHooks;

impl HousingEconomyHooks for TestHooks {
    fn pay_house_construction(
        &self,
        _server: &crate::server::GameServer,
        _player: &PlayerRef,
        _design: HouseItem,
        _total: i32,
        _tax_item: bool,
    ) -> Result<(), HouseConstructionError> {
        Ok(())
    }

    fn inventory_item(
        &self,
        _server: &crate::server::GameServer,
        _player: &PlayerRef,
        item_id: u64,
    ) -> Option<HouseItem> {
        Some(HouseItem {
            item_id,
            template_id: DESIGN_ITEM,
            owner_id: 1,
            count: 1,
            max_count: 1,
            slot_type: slot_type::INVENTORY,
            ..Default::default()
        })
    }
}

fn data() -> GameData {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            id: 42,
            zone_key: TEST_ZONE,
            group_id: 25,
            ..Default::default()
        },
    );
    data.systems.housing.add_template(HousingTemplate {
        id: DESIGN,
        name: "Thatched Farmhouse".into(),
        category_id: 18,
        main_model_id: 1083,
        hp: 1000,
        garden_radius: 11.0,
        taxation: Some(Taxation {
            id: 1,
            tax: 0,
            show: true,
        }),
        ..Default::default()
    });
    data.systems.housing.add_item_housing(HousingItemHousings {
        id: 1,
        item_id: DESIGN_ITEM,
        design_id: DESIGN,
    });
    data.items
        .templates
        .insert(DESIGN_ITEM, ItemTemplate::plain(DESIGN_ITEM, 1));
    data.systems.housing.areas.insert(AREA, GROUP);
    data.systems.housing.groups.insert(
        GROUP,
        HousingGroup {
            id: GROUP,
            ..Default::default()
        },
    );
    data.systems
        .housing
        .group_categories
        .insert(GROUP, vec![18, 10, 17]);
    data.systems
        .npcs
        .subzone
        .housing_zones
        .entry(0)
        .or_default()
        .insert(
            42,
            vec![Area {
                id: AREA,
                name: "w_solzreed_3".into(),
                points: AREA_338.to_vec(),
            }],
        );
    for id in [manager::BUFF_UNTOUCHABLE, manager::BUFF_REMOVAL_DEBUFF] {
        data.systems.skills.add_buff(BuffTemplate {
            id,
            ..Default::default()
        });
    }
    data
}

struct Fixture {
    server: crate::server::GameServer,
    client: TestClient,
    player: PlayerRef,
}

/// [`test_server_with`] with a 16x16-cell world so the real `w_solzreed_3` coordinates are
/// inside the world template's zone map.
fn test_server_large(mut data: GameData) -> GameServer {
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let db = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(0, "main_world", 16, 16, TEST_ZONE)],
        Vec::new(),
    )
    .expect("synthetic world");
    GameServer::new(config, db, data)
}

fn fixture(data: GameData) -> Fixture {
    let server = test_server_large(data);
    server.systems.housing.hooks.install(Arc::new(TestHooks));
    let client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Owner", ACCEPTED.0, ACCEPTED.1),
        10,
    );
    Fixture {
        server,
        client,
        player,
    }
}

fn packet(x: f32, y: f32) -> CSCreateHousePacket {
    CSCreateHousePacket {
        design_id: DESIGN,
        x,
        y,
        z: 100.0,
        z_rot: 0.0,
        item_id: 1,
        money_amount: 0,
        ht: 0,
        auto_use_aa_point: false,
    }
}

/// Drives `CSCreateHouse` through `ops::build` and returns the refusal code (if any). The
/// refused design must not create a house.
fn build_error(f: &mut Fixture, x: f32, y: f32) -> Option<i16> {
    let before = f.server.systems.housing.manager.count();
    ops::build(&f.server, &f.player, 1001, &packet(x, y));
    assert_eq!(
        f.server.systems.housing.manager.count(),
        before,
        "a refused placement must not create a house"
    );
    f.client.drain().into_iter().find_map(|(op, body)| {
        (op == sc::SCErrorMsgPacket).then(|| i16::from_le_bytes([body[0], body[1]]))
    })
}

fn install_ramp_heightmap(server: &crate::server::GameServer) {
    const COEF: f64 = 65535.0 / (4096.0 / 4.0);
    const CELL_POINTS: usize = (CELL_HMAP_RESOLUTION * CELL_HMAP_RESOLUTION) as usize;
    let map = WorldHeightmap::with_size(0, "main_world", 16, 16, COEF);
    let mut heights = vec![0u16; CELL_POINTS];
    for sx in 0..CELL_HMAP_RESOLUTION {
        for sy in 0..CELL_HMAP_RESOLUTION {
            heights[(sx * CELL_HMAP_RESOLUTION + sy) as usize] = (sx * 40) as u16;
        }
    }
    // The accepted footprint (15316..15340, 14620..14644) lies in world cell (14, 14).
    assert!(map.insert_cell(14, 14, heights));
    server
        .systems
        .npcs
        .heightmaps
        .install(Arc::new(HeightmapWorlds::from_parts(
            Arc::new(crate::data::client_files::ClientFileManager::new()),
            vec![map],
            HashMap::from([(TEST_ZONE, 0)]),
            HeightmapConfig {
                heightmaps_enable: true,
                ..Default::default()
            },
        )));
}

/// A physical unit collider in the middle of the accepted footprint.
fn install_unit_collider(server: &crate::server::GameServer) {
    server
        .systems
        .ai
        .collision
        .test_files
        .set(Arc::new(HashMap::new()))
        .ok();
    server
        .systems
        .ai
        .collision
        .test_terrain
        .set(Arc::new(Plane {
            a: 100.0,
            b: 0.0,
            c: 0.0,
            n: 1024,
        }))
        .ok();
    let physics = crate::systems::ai::collision::phys_world(server, 0).expect("physics world");
    physics.lock().insert_cell(CellContent {
        cell: (14, 14),
        ..Default::default()
    });
    assert!(physics
        .lock()
        .sync_living(
            500,
            &crate::systems::ai::collision::data::LivingParams::default(),
            Vec3::new(ACCEPTED.0, ACCEPTED.1, 100.0),
        )
        .is_some());
}

#[tokio::test]
async fn area_338_captured_spot_is_refused_and_the_fitting_spot_is_accepted() {
    let mut f = fixture(data());
    assert_eq!(
        build_error(&mut f, REFUSED.0, REFUSED.1),
        Some(crate::systems::economy::error_message::HOUSE_CANNOT_LOCATE_INVALID_AREA)
    );
    ops::build(&f.server, &f.player, 1001, &packet(ACCEPTED.0, ACCEPTED.1));
    let houses = f.server.systems.housing.manager.all();
    assert_eq!(houses.len(), 1, "the fitting footprint is accepted");
    let pos = houses[0].data().unit.transform.local();
    assert_eq!((pos.x, pos.y), ACCEPTED);
}

#[tokio::test]
async fn a_six_metre_terrain_spread_is_refused_with_terrain_too_low() {
    let mut f = fixture(data());
    install_ramp_heightmap(&f.server);
    assert_eq!(
        build_error(&mut f, ACCEPTED.0, ACCEPTED.1),
        Some(crate::systems::economy::error_message::HOUSE_CANNOT_LOCATE_TERRAIN_TOO_LOW)
    );
}

#[tokio::test]
async fn a_unit_inside_the_footprint_is_refused_with_overlap_unit() {
    let mut f = fixture(data());
    install_unit_collider(&f.server);
    assert_eq!(
        build_error(&mut f, ACCEPTED.0, ACCEPTED.1),
        Some(crate::systems::economy::error_message::HOUSE_CANNOT_LOCATE_OVERLAP_UNIT)
    );
}

#[tokio::test]
async fn a_rotated_house_uses_the_clients_snapped_axis_aligned_box() {
    let f = fixture(data());
    let world = f.server.world.world(0).unwrap();
    let house = manager::create(&f.server, DESIGN, 101, &world, 0, 0).unwrap();
    {
        let mut h = house.data();
        h.id = f.server.ids.housing.get_next_id().unwrap();
        // 25 m east and 45 degrees: the turned 24 m box would reach the spot (old SAT
        // refused), but the client's mode-2 house box is the snapped axis-aligned square
        // (x 15340..15364), which only touches the new footprint at x = 15340.
        h.unit.scale = 1.0;
        h.unit
            .transform
            .set_xyz(ACCEPTED.0 + 25.0, ACCEPTED.1, 100.0);
        h.unit.transform.rot_z = std::f32::consts::FRAC_PI_4;
    }
    assert!(f.server.systems.housing.manager.insert(&house));
    ops::build(&f.server, &f.player, 1001, &packet(ACCEPTED.0, ACCEPTED.1));
    assert_eq!(
        f.server.systems.housing.manager.count(),
        2,
        "the client accepts this spot against the snapped axis-aligned box"
    );

    let mut f = fixture(data());
    let world = f.server.world.world(0).unwrap();
    let house = manager::create(&f.server, DESIGN, 101, &world, 0, 0).unwrap();
    {
        let mut h = house.data();
        h.id = f.server.ids.housing.get_next_id().unwrap();
        // 20 m east: the snapped box (x 15336..15360) overlaps the new footprint.
        h.unit.scale = 1.0;
        h.unit
            .transform
            .set_xyz(ACCEPTED.0 + 20.0, ACCEPTED.1, 100.0);
        h.unit.transform.rot_z = std::f32::consts::FRAC_PI_4;
    }
    assert!(f.server.systems.housing.manager.insert(&house));
    assert_eq!(
        build_error(&mut f, ACCEPTED.0, ACCEPTED.1),
        Some(crate::systems::economy::error_message::HOUSE_CANNOT_LOCATE_OVERLAP_HOUSE)
    );
}

//! Server-side placement checks using the original 1.2 housing polygons and eligibility data.

use super::data::{HousingData, HousingTemplate};
use crate::server::GameServer;
use crate::systems::economy::error_message as err;
use crate::systems::npcs::subzone::{self, Area};
use crate::data::GameData;
use crate::world::{PlayerRef, WorldInstance};

/// `FUN_393312a0` / `FUN_398971c0`: the placement cells are 4 m squares.
const FOOTPRINT_CELL: f32 = 4.0;
/// `FUN_39897170`: 12 or more cells is logged as "house too big" and yields no footprint.
const FOOTPRINT_MAX_CELLS: i32 = 12;
/// `FUN_39332180` / `FUN_39337fb0`: refuse when the terrain spread exceeds 6 m.
const MAX_TERRAIN_SPREAD: f32 = 6.0;

/// `FUN_39897170`: `n = int((garden_radius * 2 + 3) * 0.25)`; `n >= 12` has no footprint.
fn footprint_cells(garden_radius: f32) -> i32 {
    if !garden_radius.is_finite() {
        return 0;
    }
    let n = ((garden_radius * 2.0 + 3.0) * 0.25) as i32;
    if n >= FOOTPRINT_MAX_CELLS {
        return 0;
    }
    n.max(0)
}

/// `FUN_39326980` / `FUN_393312a0`: the grid origin is snapped to the world's 4 m grid.
fn footprint_origin(x: f32, y: f32, cells: i32) -> (f32, f32) {
    let half = cells as f32 * FOOTPRINT_CELL * 0.5;
    let snap = |v: f32| ((v - half + FOOTPRINT_CELL * 0.5) * 0.25) as i32 as f32 * FOOTPRINT_CELL;
    (snap(x), snap(y))
}

/// Every housing polygon of a world template, as the client's housing-area manager sees them.
fn housing_areas(data: &GameData, world_template_id: u32) -> Vec<&Area> {
    data.systems
        .npcs
        .subzone
        .housing_zones
        .get(&world_template_id)
        .map(|zones| zones.values().flatten().collect())
        .unwrap_or_default()
}

/// `FUN_39331ba0`: all four corners of one 4 m cell must lie in the same housing polygon.
fn cell_inside_one_area(areas: &[&Area], x: f32, y: f32) -> bool {
    let corners = [
        (x, y),
        (x + FOOTPRINT_CELL, y),
        (x, y + FOOTPRINT_CELL),
        (x + FOOTPRINT_CELL, y + FOOTPRINT_CELL),
    ];
    areas.iter().any(|area| {
        corners
            .iter()
            .all(|&(px, py)| subzone::is_inside(&area.points, [px, py, 0.0]))
    })
}

fn validate_groups(
    data: &HousingData,
    areas: &[u32],
    category: u32,
    owned_categories: &[u32],
) -> Result<(), i16> {
    let mut error = err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA;
    for group in areas
        .iter()
        .filter_map(|id| data.areas.get(id))
        .filter_map(|id| data.groups.get(id))
    {
        if !data
            .group_categories
            .get(&group.id)
            .is_some_and(|categories| categories.contains(&category))
        {
            continue;
        }
        if group.houseless && !owned_categories.is_empty() {
            error = err::HOUSE_CANNOT_OWN_MORE_HOUSELESS_CONDITION;
            continue;
        }
        if group.existing_category_id != 0 && owned_categories.contains(&group.existing_category_id)
        {
            error = err::HOUSE_CANNOT_OWN_MORE_EXISTING_CATEGORY_CONDITION;
            continue;
        }
        return Ok(());
    }
    Err(error)
}

/// `FUN_39337fb0` / `FUN_39332180`: the 6 m terrain spread over the placement square.
fn validate_terrain(
    server: &GameServer,
    world: &WorldInstance,
    x0: f32,
    y0: f32,
    side: f32,
) -> Result<(), i16> {
    let Some(maps) = server.systems.npcs.heightmaps.get() else {
        return Ok(());
    };
    if !maps.config.heightmaps_enable {
        return Ok(());
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for ix in 0..3 {
        for iy in 0..3 {
            let x = x0 + side * ix as f32 * 0.5;
            let y = y0 + side * iy as f32 * 0.5;
            let Ok(height) = maps.world_height(world.template.id, x, y) else {
                return Ok(());
            };
            min = min.min(height);
            max = max.max(height);
        }
    }
    if max - min > MAX_TERRAIN_SPREAD {
        return Err(err::HOUSE_CANNOT_LOCATE_TERRAIN_TOO_LOW);
    }
    Ok(())
}

/// Validate before reserving a design, money, or tax certificates. Accounts without static
/// placement tables (synthetic flow fixtures) still receive world and overlap checks.
pub fn validate_house(
    server: &GameServer,
    player: &PlayerRef,
    template: &HousingTemplate,
    x: f32,
    y: f32,
    z: f32,
) -> Result<(), i16> {
    if ![x, y, z].into_iter().all(f32::is_finite) {
        return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
    }
    let (instance, account) = {
        let c = player.data();
        (c.unit.transform.instance_id, c.account_id)
    };
    let world = server
        .world
        .world(instance)
        .ok_or(err::HOUSE_CANNOT_LOCATE_INVALID_AREA)?;
    if world.template.zone_id_at(x, y) == 0 {
        return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
    }
    let data = &server.game_data.systems.housing;
    let fort = super::dominion::fort::placement_owner(server, player, template, x, y)?;
    if fort.is_none() {
        super::dominion::rights::placement_allowed(
            server,
            player,
            instance,
            x,
            y,
            template.garden_radius,
        )?;
    }
    if fort.is_none() && !data.areas.is_empty() {
        let areas = subzone::get_housing_zone_by_position(&server.game_data, &world.template, x, y)
            .map_err(|_| err::HOUSE_CANNOT_LOCATE_INVALID_AREA)?;
        if areas.is_empty() {
            return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
        }
        let owned_categories = server
            .systems
            .housing
            .manager
            .by_account(account)
            .iter()
            .filter_map(|house| {
                data.template(house.data().unit.template_id)
                    .map(|t| t.category_id)
            })
            .collect::<Vec<_>>();
        validate_groups(data, &areas, template.category_id, &owned_categories)?;
    }
    let cells = footprint_cells(template.garden_radius);
    if cells > 0 && fort.is_none() && !data.areas.is_empty() {
        let areas = housing_areas(&server.game_data, world.template.id);
        let (x0, y0) = footprint_origin(x, y, cells);
        for ix in 0..cells {
            for iy in 0..cells {
                let cx = x0 + ix as f32 * FOOTPRINT_CELL;
                let cy = y0 + iy as f32 * FOOTPRINT_CELL;
                if !cell_inside_one_area(&areas, cx, cy) {
                    return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
                }
            }
        }
    }
    for house in server.systems.housing.manager.all() {
        let (house_instance, id, position, scale) = {
            let h = house.data();
            (
                h.unit.transform.instance_id,
                h.unit.template_id,
                h.unit.transform.local(),
                h.unit.scale,
            )
        };
        if house_instance != instance {
            continue;
        }
        let Some(other) = data.template(id) else {
            continue;
        };
        // Garden ownership already uses axis-aligned squares throughout the 1.2 server.
        let extent = template.garden_radius + other.garden_radius * scale;
        if (x - position.x).abs() < extent && (y - position.y).abs() < extent {
            return Err(err::HOUSE_CANNOT_LOCATE_OVERLAP_HOUSE);
        }
    }
    if cells > 0 {
        let (x0, y0) = footprint_origin(x, y, cells);
        validate_terrain(server, &world, x0, y0, cells as f32 * FOOTPRINT_CELL)?;
    }
    Ok(())
}

/// Beginner plots in the original data (`housing_groups.can_extend = false`) expire rather
/// than accepting another weekly payment. Ordinary or unclassified plots retain normal taxes.
pub fn can_extend_house(server: &GameServer, house: &crate::world::HouseRef) -> bool {
    let transform = house.data().unit.transform;
    let Some(world) = server.world.world(transform.instance_id) else {
        return false;
    };
    let data = &server.game_data.systems.housing;
    let pos = transform.local();
    let Ok(areas) =
        subzone::get_housing_zone_by_position(&server.game_data, &world.template, pos.x, pos.y)
    else {
        return false;
    };
    !areas
        .iter()
        .filter_map(|id| data.areas.get(id))
        .filter_map(|id| data.groups.get(id))
        .any(|group| !group.can_extend)
}

#[cfg(test)]
mod tests {
    use super::super::data::HousingGroup;
    use super::*;

    #[tokio::test]
    async fn placement_resolves_zone_ids_and_rejects_invalid_land_and_overlap() {
        use crate::data::{world_data::Zone, GameData};
        use crate::models::house::House;
        use crate::systems::npcs::subzone::Area;
        use crate::test_support::*;
        use crate::world::Entity;
        let mut data = GameData::empty();
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                id: 42,
                zone_key: TEST_ZONE,
                ..Default::default()
            },
        );
        data.systems
            .npcs
            .subzone
            .housing_zones
            .entry(0)
            .or_default()
            .insert(
                42,
                vec![Area {
                    id: 500,
                    name: "Housing".into(),
                    points: vec![
                        [100., 100., 0.],
                        [900., 100., 0.],
                        [900., 900., 0.],
                        [100., 900., 0.],
                    ],
                }],
            );
        let template = HousingTemplate {
            id: 1,
            category_id: 17,
            garden_radius: 8.,
            ..Default::default()
        };
        data.systems.housing.add_template(template.clone());
        data.systems.housing.areas.insert(500, 1);
        data.systems.housing.groups.insert(
            1,
            HousingGroup {
                id: 1,
                ..Default::default()
            },
        );
        data.systems.housing.group_categories.insert(1, vec![17]);
        let server = test_server_with(data);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(&server, &client, test_character(1, "Owner", 500., 500.), 10);
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Ok(())
        );
        assert_eq!(
            validate_house(&server, &player, &template, 50., 50., 100.),
            Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA)
        );
        assert_eq!(
            validate_house(&server, &player, &template, 2500., 500., 100.),
            Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA)
        );
        let mut h = House::new(7, 1);
        h.unit.tl_id = 7;
        h.unit.transform.set_xyz(510., 510., 100.);
        let house = Entity::new(20, h);
        assert!(server.systems.housing.manager.insert(&house));
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Err(err::HOUSE_CANNOT_LOCATE_OVERLAP_HOUSE)
        );
        house.data().unit.transform.instance_id = 1;
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Ok(())
        );
    }

    #[test]
    fn placement_groups_enforce_categories_and_account_eligibility() {
        let mut data = HousingData::default();
        data.areas.insert(100, 9);
        data.groups.insert(
            9,
            HousingGroup {
                id: 9,
                houseless: false,
                existing_category_id: 17,
                ..Default::default()
            },
        );
        data.group_categories.insert(9, vec![17]);
        assert_eq!(validate_groups(&data, &[100], 17, &[]), Ok(()));
        assert_eq!(
            validate_groups(&data, &[100], 1, &[]),
            Err(err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA)
        );
        assert_eq!(
            validate_groups(&data, &[100], 17, &[17]),
            Err(err::HOUSE_CANNOT_OWN_MORE_EXISTING_CATEGORY_CONDITION)
        );
        data.groups.get_mut(&9).unwrap().houseless = true;
        assert_eq!(
            validate_groups(&data, &[100], 17, &[1]),
            Err(err::HOUSE_CANNOT_OWN_MORE_HOUSELESS_CONDITION)
        );
        assert!(validate_groups(&data, &[999], 17, &[]).is_err());
    }
}

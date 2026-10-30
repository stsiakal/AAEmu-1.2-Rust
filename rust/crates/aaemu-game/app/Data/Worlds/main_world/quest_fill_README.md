# Quest spawn fill (`doodad_spawns_quest_fill.json`)

One entity: doodad **1137 "Unknown Crate"**, the giver of quest **869 "Treasure of the
Towers"** (Mahadevi, level 16, open zone, not test/dummy/event content).

## Position provenance

| Source | Position |
| --- | --- |
| 10.x client `game/worlds/main_world/level_design/cells/018_008/doodad.g` (`type 1137`) | 19065.793, 8919.587, 352.82 |
| 10.x client `map_data/doodad_map/e_mahadevi.dat` (record 1137) | 19065.793, 8919.587, 352.82 (0.0 m from the above) |
| 1.2 client `map_data/doodad_map/e_mahadevi.dat` (record 1137) | 19065.793, 8919.587, 352.82 |

Yaw 66.9998° is the 10.x `doodad.g` quaternion `(0, 0, 0.551936, 0.833887)` (the loader reads
degrees and converts). The position lies in zone key **196 (e_mahadevi_2)**, the quest's zone;
its `world.xml` origin (18, 7) is identical in 1.2 and 10.x, and the zone is open (`closed =
'f'`).

## Height

The crate is on the tower floor, not on the raw terrain: the 1.2 heightmap at (19065.793,
8919.587) is 241.78 m, while the 1.2 client navmesh (the live server's `GeoDataMode` height
path, `WorldManager.GetHeight` -> `GeoData.GetHeight`) gives **352.81982 m**, 0.0002 m from the
spawn Z. The ignored test `real_data_quest_fill_spawns_on_terrain` checks exactly that path
(`HeightmapWorlds::get_height` with the world's `AiGeoData` installed).

## What was excluded

- 22 NPCs whose 10.x spawner placement and 10.x `npc_map` record agree within 0.5 m but whose
  quests are in zones closed in 1.2 (Airain Rock 192/193, Sunbite Wilds 157/190, Cradle of
  Genesis 328, Golden Ruins 281/203, Mistmerrow 307) or are test/dummy quests (Maisky 13877,
  Sephvell 14147).
- The open-zone missing NPCs without a position in both sources (Tame Yata 3450, Asam 1266,
  Freeblood Maurauder 1772, Trafficker Pirate 12509 and the other one-source/no-source NPCs).
- Library doodads 7992/7993/7994 (level 51-55 later-patch content) and the remaining missing
  doodads (1082, 2770, 6197, 7404, 4380): no `doodad.g` + `doodad_map` pair.
- All missing entities with no 10.x position at all (the majority of the 179 candidates).

The spawn-file loader ignores unknown keys (`spawner.rs::parse_spawner_base`), so `Title` is a
plain source note; this README is the full provenance.

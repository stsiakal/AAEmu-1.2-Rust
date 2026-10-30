//! Real-data tests of the combat (`#[ignore]`d; they need the class research and the compact
//! database on this machine):
//!
//! ```sh
//! AAEMU_CLASS_DATA=D:/aaemu-client/handoff/2026-10-27/research/classes-12 \
//! AAEMU_COMPACT_DB=rust/crates/aaemu-game/app/Data/compact.sqlite3 \
//!   cargo test -p aaemu-bots -- --ignored combat_real_data
//! ```
//!
//! They prove that the shipped research files parse as our `serde` types, that the three
//! end-to-end archetypes (melee / ranged / healer) load, and that every skill a rotation names
//! exists in the 1.2 compact database.

use std::path::Path;

use super::rotation::ClassData;

fn class_data_dir() -> std::path::PathBuf {
    std::env::var_os("AAEMU_CLASS_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            Path::new("D:/aaemu-client/handoff/2026-10-27/research/classes-12").to_path_buf()
        })
}

#[test]
#[ignore = "needs the classes-12 research files (AAEMU_CLASS_DATA)"]
fn the_research_files_load_as_the_bots_class_data() {
    let dir = class_data_dir();
    let archetypes = dir.join("archetypes.json");
    let rotations = dir.join("rotations");
    let class = ClassData::load(
        archetypes.to_string_lossy().as_ref(),
        rotations.to_string_lossy().as_ref(),
        "darkrunner",
    )
    .expect("the research files parse");
    assert_eq!(class.archetypes().len(), 19, "19 researched archetypes");
    let with_rotation = class
        .archetypes()
        .iter()
        .filter(|a| class.rotation_for(a).is_some())
        .count();
    assert_eq!(with_rotation, 19, "a rotation per archetype");
    for key in ["darkrunner", "daggerspell", "hierophant"] {
        let archetype = class.get(key).unwrap_or_else(|| panic!("{key} archetype"));
        assert_eq!(archetype.creation_ability(), archetype.abilities()[0]);
        let rotation = class.rotation_for(archetype).expect("rotation");
        assert!(!rotation.fillers.is_empty(), "{key} has fillers");
    }
}

#[test]
#[ignore = "needs compact.sqlite3 (AAEMU_COMPACT_DB)"]
fn every_rotation_skill_exists_in_the_compact_database() {
    let dir = class_data_dir();
    let class = ClassData::load(
        dir.join("archetypes.json").to_string_lossy().as_ref(),
        dir.join("rotations").to_string_lossy().as_ref(),
        "darkrunner",
    )
    .expect("the research files parse");
    let db = std::env::var("AAEMU_COMPACT_DB")
        .expect("set AAEMU_COMPACT_DB to the 1.2 compact.sqlite3");
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("the compact database opens");
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for archetype in class.archetypes() {
        let Some(rotation) = class.rotation_for(archetype) else {
            continue;
        };
        let ids = rotation
            .rules
            .iter()
            .flat_map(|r| r.then.iter())
            .chain(rotation.fillers.iter())
            .map(|a| a.skill);
        for id in ids {
            checked += 1;
            let exists: i64 = conn
                .query_row("SELECT COUNT(*) FROM skills WHERE id = ?1", [id], |r| r.get(0))
                .expect("skills query");
            if exists == 0 {
                missing.push((archetype.key.clone(), id));
            }
        }
    }
    assert!(checked > 100, "the rotations name many skills ({checked})");
    assert!(missing.is_empty(), "unknown skill ids: {missing:?}");
}

#[test]
#[ignore = "needs compact.sqlite3 (AAEMU_COMPACT_DB)"]
fn three_archetypes_have_a_level_one_button_and_a_damage_skill() {
    let db = std::env::var("AAEMU_COMPACT_DB")
        .expect("set AAEMU_COMPACT_DB to the 1.2 compact.sqlite3");
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("the compact database opens");
    // Darkrunner Triple Slash (Battlerage), Daggerspell Flamebolt (Sorcery), Hierophant
    // Antithesis (Vitalism): each is learnable at level 1.
    for (name, skill, ability) in [
        ("Darkrunner", 18132u32, 1u8),
        ("Daggerspell", 10752, 7),
        ("Hierophant", 10534, 10),
    ] {
        let (ability_id, ability_level): (u8, i32) = conn
            .query_row(
                "SELECT ability_id, ability_level FROM skills WHERE id = ?1",
                [skill],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or_else(|e| panic!("{name} skill {skill}: {e}"));
        assert_eq!(ability_id, ability, "{name}");
        assert_eq!(ability_level, 1, "{name} is a level 1 skill");
        // A learnable button may carry its damage in a plot (Triple Slash 18132 does) instead
        // of direct effects; either way the row is a real, castable level-1 skill.
        let (effects, plot): (i64, Option<u32>) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM skill_effects WHERE skill_id = s.id), s.plot_id
                   FROM skills s WHERE s.id = ?1",
                [skill],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("effect / plot columns");
        assert!(effects > 0 || plot.is_some(), "{name} skill {skill} has effects");
    }
}

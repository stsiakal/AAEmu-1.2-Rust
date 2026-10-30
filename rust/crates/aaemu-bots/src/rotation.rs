//! Class data of the P2 combat: the archetypes (class trio + learn order) and their rotation
//! rules, as the `classes-12` research wrote them (`archetypes.json`, `rotations/<key>.json`).
//!
//! The files are research output, not client data: they carry skill **ids** plus the names the
//! research used to read them, so the bot loads them from the paths configured in `Bots`
//! (`Bots.ArchetypesFile`, `Bots.RotationDir`) instead of committing copies into the crate.
//! Both are our own `serde` types (D5: no stringly-typed engine): a rule is
//! `{ when: Condition, then: Vec<Action>, relevance }` and `Condition` is an enum covering the
//! condition keys of all 19 rotation files (`all` / `any` / `not`, tags, range, cooldowns, HP
//! and mana, enemies, buffs). Unknown JSON keys are ignored so the research can grow fields.
//!
//! The rules are draft data ("rotation is low confidence everywhere", `archetypes-notes.md`):
//! the evaluator treats them as priorities, and the server's own cast checks have the last word.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

/// One `then` / filler action: the skill id the research chose (the name is for people).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Action {
    pub skill: u32,
    #[serde(default)]
    pub name: String,
}

/// A rotation rule condition, one variant per JSON object key used by the research files.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    All {
        all: Vec<Condition>,
    },
    Any {
        any: Vec<Condition>,
    },
    Not {
        not: Box<Condition>,
    },
    /// The target carries a buff tag (`tagged_buffs`), e.g. Tripped 107.
    TargetHasTag {
        target_has_tag: u32,
    },
    /// The bot itself carries a buff tag (proposed condition, needs a live check).
    SelfHasTag {
        self_has_tag: u32,
    },
    /// The skill's own cooldown (individual and tag cooldown) is ready.
    CooldownReady {
        cooldown_ready: u32,
    },
    /// Distance to the target within `[min, max]` metres.
    Range {
        range: (f32, f32),
    },
    /// At least `count` hostile monsters within `radius` metres of the bot.
    EnemiesWithin {
        enemies_within: (u32, u32),
    },
    /// An ally within `radius` metres is below `hp_percent` (solo: the bot itself).
    AllyHpBelow {
        ally_hp_below: (u32, u32),
    },
    /// The bot is below `self_hp_below` percent hit points.
    SelfHpBelow {
        self_hp_below: u32,
    },
    /// The bot is below `self_mana_below` percent mana (proposed condition).
    SelfManaBelow {
        self_mana_below: u32,
    },
    /// The bot does not have the buff (template id) active.
    SelfMissingBuff {
        self_missing_buff: u32,
    },
    /// The target is casting / channelling a skill.
    TargetCasting {
        target_casting: bool,
    },
    /// An ally is dead (solo: never true).
    AllyDead {
        ally_dead: bool,
    },
    /// Out of combat (`Unit.IsInBattle` false).
    OutOfCombat {
        out_of_combat: bool,
    },
}

/// One rotation rule.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub id: String,
    pub when: Condition,
    pub then: Vec<Action>,
    #[serde(default)]
    pub relevance: i32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub note: String,
    /// `true` for rules using one of the proposed conditions (`self_has_tag`,
    /// `self_mana_below`, `ally_dead`, `out_of_combat`).
    #[serde(default)]
    pub ext: bool,
}

/// `rotations/<archetype>.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Rotation {
    pub archetype: String,
    pub role: String,
    #[serde(default)]
    pub trees: Vec<String>,
    #[serde(default)]
    pub condition_notes: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub fillers: Vec<Action>,
    #[serde(default)]
    pub openers: Vec<Vec<Action>>,
    #[serde(default)]
    pub defensives: Vec<Action>,
}

impl Rotation {
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let rotation: Rotation = serde_json::from_str(json)?;
        rotation.validate()?;
        Ok(rotation)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading rotation {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("rotation {}: {e}", path.display()))
    }

    /// The structural sanity a startup validator checks: ids are nonzero, every rule has a
    /// relevance band and at least one action, and the condition tree is not empty.
    fn validate(&self) -> anyhow::Result<()> {
        if self.archetype.is_empty() || self.role.is_empty() {
            anyhow::bail!("rotation without an archetype or role");
        }
        for rule in &self.rules {
            if rule.then.is_empty() {
                anyhow::bail!("rule '{}' has no action", rule.id);
            }
            if rule.then.iter().any(|a| a.skill == 0) {
                anyhow::bail!("rule '{}' names skill 0", rule.id);
            }
            if !(0..=100).contains(&rule.relevance) {
                anyhow::bail!("rule '{}' has relevance {}", rule.id, rule.relevance);
            }
            condition_is_valid(&rule.when)
                .map_err(|why| anyhow::anyhow!("rule '{}': {why}", rule.id))?;
        }
        Ok(())
    }
}

fn condition_is_valid(condition: &Condition) -> Result<(), String> {
    match condition {
        Condition::All { all } => {
            if all.is_empty() {
                return Err("an all/any condition is empty".into());
            }
            for c in all {
                condition_is_valid(c)?;
            }
        }
        Condition::Any { any } => {
            if any.is_empty() {
                return Err("an all/any condition is empty".into());
            }
            for c in any {
                condition_is_valid(c)?;
            }
        }
        Condition::Not { not } => condition_is_valid(not)?,
        Condition::Range { range } => {
            if range.0 > range.1 || range.1 <= 0.0 {
                return Err(format!("bad range {range:?}"));
            }
        }
        Condition::EnemiesWithin { enemies_within } => {
            if enemies_within.1 == 0 {
                return Err("enemies_within wants at least one enemy".into());
            }
        }
        Condition::AllyHpBelow { ally_hp_below } => {
            if ally_hp_below.0 == 0 || ally_hp_below.0 > 100 {
                return Err(format!("bad ally hp percent {}", ally_hp_below.0));
            }
        }
        Condition::SelfHpBelow { self_hp_below } if *self_hp_below == 0 || *self_hp_below > 100 => {
            return Err("bad self hp percent".into());
        }
        Condition::SelfManaBelow { self_mana_below }
            if *self_mana_below == 0 || *self_mana_below > 100 =>
        {
            return Err("bad self mana percent".into());
        }
        _ => {}
    }
    Ok(())
}

/// One step of `archetypes.json`'s `learn_order`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LearnStep {
    pub level: u8,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub skill_id: u32,
    #[serde(default)]
    pub buff_id: u32,
    #[serde(default)]
    pub ability_level: i32,
    #[serde(default)]
    pub req_points: i32,
}

/// One archetype of `archetypes.json`: the class trio and the per-level learn order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Archetype {
    pub key: String,
    pub name: String,
    pub class_abilities: [u8; 3],
    #[serde(default)]
    pub trees: Vec<ArchetypeTree>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub leveling_tree_order: LevelingTrees,
    #[serde(default)]
    pub learn_order: Vec<LearnStep>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ArchetypeTree {
    pub id: u8,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LevelingTrees {
    #[serde(default)]
    pub creation: String,
    #[serde(default)]
    pub char_level_5: String,
    #[serde(default)]
    pub char_level_10: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ArchetypesFile {
    #[serde(default)]
    archetypes: Vec<Archetype>,
}

impl Archetype {
    /// The skillsets in the order the bot takes them (creation first): the leveling order names
    /// trees, so map them back to ids; `class_abilities` is the file's authoritative trio.
    pub fn abilities(&self) -> [u8; 3] {
        let name_id = |name: &str| {
            self.trees
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(name))
                .map(|t| t.id)
        };
        let mut ids = [0u8; 3];
        ids[0] = name_id(&self.leveling_tree_order.creation).unwrap_or(self.class_abilities[0]);
        ids[1] = name_id(&self.leveling_tree_order.char_level_5).unwrap_or(self.class_abilities[1]);
        ids[2] =
            name_id(&self.leveling_tree_order.char_level_10).unwrap_or(self.class_abilities[2]);
        // A missing / inconsistent leveling order: keep the trio but move the creation tree
        // first so the created character always starts on a tree of its own class.
        if !ids.contains(&self.class_abilities[0]) || ids.contains(&0) {
            ids = self.class_abilities;
        }
        ids
    }

    /// The tree the character is created with.
    pub fn creation_ability(&self) -> u8 {
        self.abilities()[0]
    }

    pub fn skills(&self) -> impl Iterator<Item = (u8, &LearnStep)> {
        self.learn_order
            .iter()
            .filter(|s| s.kind == "skill" && s.skill_id != 0)
            .map(|s| (s.level, s))
    }

    pub fn passives(&self) -> impl Iterator<Item = (u8, &LearnStep)> {
        self.learn_order
            .iter()
            .filter(|s| s.kind == "passive" && s.buff_id != 0)
            .map(|s| (s.level, s))
    }
}

/// The class data of a server: the archetypes and the rotation of each.
#[derive(Debug, Default, Clone)]
pub struct ClassData {
    archetypes: Vec<Arc<Archetype>>,
    rotations: HashMap<String, Arc<Rotation>>,
    default_key: String,
}

impl ClassData {
    /// Loads `archetypes_file` and every `*.json` of `rotation_dir` (both optional). A missing
    /// or unreadable file is an error: a configured class data path should work or say why.
    pub fn load(
        archetypes_file: &str,
        rotation_dir: &str,
        default_key: &str,
    ) -> anyhow::Result<Self> {
        let mut data = ClassData {
            default_key: default_key.trim().to_lowercase(),
            ..Default::default()
        };
        if !archetypes_file.trim().is_empty() {
            data.archetypes = load_archetypes(Path::new(archetypes_file.trim()))?;
        }
        if !rotation_dir.trim().is_empty() {
            data.rotations = load_rotations(Path::new(rotation_dir.trim()))?;
        }
        if data.archetypes.is_empty() && !data.rotations.is_empty() {
            anyhow::bail!("rotation files loaded without an archetypes file");
        }
        if !data.archetypes.is_empty() && data.rotations.is_empty() {
            tracing::warn!(
                "Bots: {} archetypes loaded but Bots.RotationDir has no rotations; combat uses the data scoring",
                data.archetypes.len()
            );
        }
        Ok(data)
    }

    pub fn is_empty(&self) -> bool {
        self.archetypes.is_empty()
    }

    pub fn archetypes(&self) -> &[Arc<Archetype>] {
        &self.archetypes
    }

    pub fn get(&self, key: &str) -> Option<&Arc<Archetype>> {
        let key = key.trim().to_lowercase();
        self.archetypes
            .iter()
            .find(|a| a.key.eq_ignore_ascii_case(&key))
    }

    /// The archetype whose creation tree is `ability`, else the first that lists it at all.
    pub fn for_ability(&self, ability: u8) -> Option<&Arc<Archetype>> {
        self.archetypes
            .iter()
            .find(|a| a.creation_ability() == ability)
            .or_else(|| {
                self.archetypes
                    .iter()
                    .find(|a| a.class_abilities.contains(&ability))
            })
    }

    /// The archetype `/bot add <name>` creates when none is named.
    pub fn default_archetype(&self) -> Option<&Arc<Archetype>> {
        self.get(&self.default_key)
            .or_else(|| self.archetypes.first())
    }

    pub fn rotation_for(&self, archetype: &Archetype) -> Option<Arc<Rotation>> {
        self.rotations.get(&archetype.key.to_lowercase()).cloned()
    }

    pub fn rotation(&self, key: &str) -> Option<Arc<Rotation>> {
        self.rotations.get(&key.trim().to_lowercase()).cloned()
    }

    /// In-memory data for tests (a rotation file per `(key, json)`).
    #[cfg(test)]
    pub(crate) fn from_parts(archetypes: Vec<Archetype>, rotations: Vec<(&str, &str)>) -> Self {
        let mut data = ClassData {
            archetypes: archetypes.into_iter().map(Arc::new).collect(),
            default_key: "darkrunner".into(),
            ..Default::default()
        };
        for (key, json) in rotations {
            data.rotations.insert(
                key.to_lowercase(),
                Arc::new(Rotation::parse(json).expect("test rotation")),
            );
        }
        data
    }
}

fn load_archetypes(path: &Path) -> anyhow::Result<Vec<Arc<Archetype>>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading archetypes {}: {e}", path.display()))?;
    let file: ArchetypesFile = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("archetypes {}: {e}", path.display()))?;
    if file.archetypes.is_empty() {
        anyhow::bail!("archetypes {}: no archetypes", path.display());
    }
    for a in &file.archetypes {
        if a.key.is_empty() || a.name.is_empty() || a.class_abilities.contains(&0) {
            anyhow::bail!("archetype '{}' has no key, name or class trio", a.key);
        }
    }
    Ok(file.archetypes.into_iter().map(Arc::new).collect())
}

fn load_rotations(dir: &Path) -> anyhow::Result<HashMap<String, Arc<Rotation>>> {
    let mut rotations = HashMap::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("listing rotations {}: {e}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let rotation = Arc::new(Rotation::load(&path)?);
        let key = rotation.archetype.to_lowercase().replace(' ', "_");
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            rotations.insert(stem.to_lowercase(), rotation.clone());
        }
        rotations.insert(key, rotation);
    }
    Ok(rotations)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CONDITIONS: &str = r#"{
        "archetype": "Test", "role": "melee_dps", "trees": ["Battlerage"],
        "rules": [
            {"id": "r1", "when": {"all": [
                {"target_has_tag": 107},
                {"not": {"self_missing_buff": 1417}},
                {"range": [0, 4]},
                {"cooldown_ready": 18132},
                {"enemies_within": [8, 1]},
                {"any": [{"self_hp_below": 50}, {"ally_hp_below": [90, 30]}]}
            ]}, "then": [{"skill": 18132, "name": "Triple Slash"}], "relevance": 20},
            {"id": "r2", "when": {"self_mana_below": 25}, "then": [{"skill": 10104, "name": "Leech"}], "relevance": 90},
            {"id": "r3", "when": {"target_casting": true}, "then": [{"skill": 11353}], "relevance": 40},
            {"id": "r4", "when": {"all": [{"self_has_tag": 10}, {"out_of_combat": true}]}, "then": [{"skill": 10152}], "relevance": 5},
            {"id": "r5", "when": {"ally_dead": true}, "then": [{"skill": 10546}], "relevance": 5}
        ],
        "fillers": [{"skill": 3, "name": "Melee Attack"}]
    }"#;

    #[test]
    fn every_research_condition_parses() {
        let r = Rotation::parse(ALL_CONDITIONS).expect("all conditions parse");
        assert_eq!(r.rules.len(), 5);
        assert_eq!(r.fillers.len(), 1);
        assert!(matches!(r.rules[0].when, Condition::All { .. }));
        assert!(matches!(r.rules[1].when, Condition::SelfManaBelow { .. }));
        assert!(matches!(
            r.rules[2].when,
            Condition::TargetCasting {
                target_casting: true
            }
        ));
        assert!(matches!(r.rules[3].when, Condition::All { .. }));
        assert!(matches!(
            r.rules[4].when,
            Condition::AllyDead { ally_dead: true }
        ));
    }

    #[test]
    fn a_rotation_without_actions_is_refused() {
        let error = Rotation::parse(
            r#"{"archetype":"X","role":"melee_dps","rules":[{"id":"r","when":{"self_hp_below":50},"then":[]}]}"#,
        )
        .expect_err("an empty chain is a data error");
        assert!(error.to_string().contains("no action"), "{error}");
    }

    #[test]
    fn the_creation_tree_comes_first() {
        let archetype: Archetype = serde_json::from_str(
            r#"{"key":"cleric","name":"Cleric","class_abilities":[10,9,4],"role":"healer",
                "trees":[{"id":10,"name":"Vitalism"},{"id":9,"name":"Songcraft"},{"id":4,"name":"Auramancy"}],
                "leveling_tree_order":{"creation":"Vitalism","char_level_5":"Songcraft","char_level_10":"Auramancy"},
                "learn_order":[{"level":1,"type":"skill","tree":"Vitalism","skill_id":10534,"ability_level":1}]}"#,
        )
        .expect("archetype parses");
        assert_eq!(archetype.abilities(), [10, 9, 4]);
        assert_eq!(archetype.creation_ability(), 10);
        assert_eq!(archetype.skills().count(), 1);
        assert_eq!(archetype.passives().count(), 0);
    }

    #[test]
    fn class_data_finds_archetypes_by_key_and_first_tree() {
        let darkrunner: Archetype = serde_json::from_str(
            r#"{"key":"darkrunner","name":"Darkrunner","class_abilities":[1,8,4],
                "trees":[{"id":1,"name":"Battlerage"},{"id":8,"name":"Shadowplay"},{"id":4,"name":"Auramancy"}],
                "leveling_tree_order":{"creation":"Battlerage","char_level_5":"Shadowplay","char_level_10":"Auramancy"}}"#,
        )
        .unwrap();
        let data = ClassData::from_parts(vec![darkrunner], vec![("darkrunner", ALL_CONDITIONS)]);
        assert_eq!(data.get("Darkrunner").unwrap().key, "darkrunner");
        assert_eq!(data.for_ability(8).unwrap().key, "darkrunner");
        assert_eq!(data.default_archetype().unwrap().key, "darkrunner");
        assert!(data.rotation_for(data.get("darkrunner").unwrap()).is_some());
    }
}

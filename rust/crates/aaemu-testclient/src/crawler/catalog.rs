//! Quest catalog built from `compact.sqlite3` and the world spawn JSON.
//!
//! Everything is derived data: the file is written under the crawler's output directory and is
//! never committed. The crawler uses it to know how a quest starts, which objectives it has,
//! where their targets stand, and what the turn-in is.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aaemu_game::data::sqlite::for_each_row;
use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::Serialize;

use super::{CrawlConfig, Race};

/// `QuestComponentKind`.
pub mod kind {
    pub const START: u8 = 2;
    pub const PROGRESS: u8 = 4;
    pub const READY: u8 = 6;
    pub const REWARD: u8 = 8;
}

/// Objective act classes that count as objectives (`QuestActTemplate::CountsAsAnObjective`).
fn counts_as_objective(ty: &str) -> bool {
    matches!(
        ty,
        "QuestActConAcceptBuff"
            | "QuestActConAcceptItemEquip"
            | "QuestActConFail"
            | "QuestActObjAbilityLevel"
            | "QuestActObjAggro"
            | "QuestActObjCinema"
            | "QuestActObjCompleteQuest"
            | "QuestActObjCraft"
            | "QuestActObjDistance"
            | "QuestActObjExpressFire"
            | "QuestActObjInteraction"
            | "QuestActObjItemGather"
            | "QuestActObjItemGroupGather"
            | "QuestActObjItemGroupUse"
            | "QuestActObjItemUse"
            | "QuestActObjLevel"
            | "QuestActObjMateLevel"
            | "QuestActObjMonsterGroupHunt"
            | "QuestActObjMonsterHunt"
            | "QuestActObjSphere"
            | "QuestActObjTalkNpcGroup"
            | "QuestActObjTalk"
            | "QuestActObjZoneKill"
            | "QuestActObjZoneMonsterHunt"
    )
}

/// `QuestActObjAlias` is not resolved here: the 1.2 client/server treat the act columns as the
/// real ids (the alias table only holds text).

#[derive(Debug, Clone, Serialize)]
pub struct Starter {
    /// `npc`, `doodad`, `item`, `item_gain`, `auto`, `sphere` or `none`.
    pub kind: String,
    pub npc: u32,
    pub doodad: u32,
    pub item: u32,
    pub count: i32,
    pub sphere: u32,
    /// The accept act class, for the report.
    pub act: String,
}

impl Starter {
    fn none() -> Self {
        Self {
            kind: "none".into(),
            npc: 0,
            doodad: 0,
            item: 0,
            count: 0,
            sphere: 0,
            act: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Turnin {
    /// `npc`, `doodad`, `auto` or `none`.
    pub kind: String,
    pub npc: u32,
    pub doodad: u32,
    pub act: String,
}

impl Turnin {
    fn none() -> Self {
        Self {
            kind: "none".into(),
            npc: 0,
            doodad: 0,
            act: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Objective {
    /// The detail class (`QuestActObjMonsterHunt`, ...).
    pub class: String,
    pub component: u32,
    /// `quest_acts.id` (the client's `questActId` for `CSQuestTalkMade`).
    pub act_id: u32,
    /// `QuestActTemplate::ObjectiveIndex` (`0xff` = not an objective).
    pub objective_index: u8,
    /// The count `MaxObjective` asks for.
    pub required: i32,
    /// `ObjTalk` / `ObjMonsterHunt` / `ObjSphere` npc.
    pub npc: u32,
    /// `ObjMonsterGroupHunt` / `ObjTalkNpcGroup` members.
    pub npcs: Vec<u32>,
    pub doodad: u32,
    pub highlight_doodad: u32,
    pub item: u32,
    pub items: Vec<u32>,
    pub sphere: u32,
    pub level: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct RewardItem {
    pub item: u32,
    pub count: i32,
    /// 1-based selective choice; 0 for fixed rewards.
    pub choice: i32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Rewards {
    pub exp: i64,
    pub copper: i64,
    pub items: Vec<RewardItem>,
    pub choices: Vec<RewardItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gate {
    /// `unit_reqs.kind_id`.
    pub kind: u32,
    pub value1: u32,
    pub value2: u32,
    pub supported: bool,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Quest {
    pub id: u32,
    pub name: String,
    pub level: u8,
    pub zone_id: u32,
    pub zone: String,
    pub category_id: u32,
    pub detail_id: u32,
    pub repeatable: bool,
    pub let_it_done: bool,
    pub score: i32,
    pub start_component: u32,
    pub starter: Starter,
    pub objectives: Vec<Objective>,
    pub turnin: Turnin,
    pub rewards: Rewards,
    pub gates: Vec<Gate>,
    /// Items an `OWN_ITEM` gate needs before the quest can be accepted.
    pub gate_items: Vec<u32>,
    /// `Some` when the crawler cannot play this quest (yet), with the reason.
    pub unsupported: Option<String>,
    pub instance: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemInfo {
    pub name: String,
    pub use_skill: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct DoodadSkills {
    /// Template -> the func group the level data spawns it with.
    pub template_group: HashMap<u32, u32>,
    /// Func group -> a skill a player can cast on it.
    pub group_skill: HashMap<u32, u32>,
    /// Template -> the skill of its default func group.
    pub template_skill: HashMap<u32, u32>,
    /// Templates with at least one func group that offers a player skill.
    pub templates_with_skill: HashSet<u32>,
}

type TemplateRow = (u32, String, u8, u32, u32, u32, bool, bool, i32);

#[derive(Debug, Default, Serialize)]
pub struct Catalog {
    pub quests: Vec<Quest>,
    pub npc_spawns: HashMap<u32, Vec<[f32; 3]>>,
    pub doodad_spawns: HashMap<u32, Vec<[f32; 3]>>,
    pub npc_groups: HashMap<u32, Vec<u32>>,
    pub item_groups: HashMap<u32, Vec<u32>>,
    pub items: HashMap<u32, ItemInfo>,
    pub doodad_skills: DoodadSkills,
    pub zones: HashMap<u32, String>,
}

pub struct Summary {
    pub playable: usize,
    pub unsupported: usize,
}

#[derive(Default)]
struct Comp {
    kind: u8,
}

#[derive(Clone, Default)]
struct RawAct {
    id: u32,
    component: u32,
    detail_id: u32,
    ty: String,
}

#[derive(Clone, Default)]
struct Detail {
    values: HashMap<String, i64>,
}

impl Detail {
    fn v(&self, key: &str) -> u32 {
        self.values.get(key).copied().unwrap_or(0) as u32
    }
    fn i(&self, key: &str) -> i32 {
        self.values.get(key).copied().unwrap_or(0) as i32
    }
}

fn camel_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `QuestActObjTalk` -> `quest_act_obj_talks` (the detail table of a class).
fn detail_table(class: &str) -> String {
    let rest = class.strip_prefix("QuestAct").unwrap_or(class);
    format!("quest_act_{}s", camel_to_snake(rest))
}

fn read_detail(conn: &Connection, class: &str, id: u32) -> Result<Option<Detail>> {
    let table = detail_table(class);
    let sql = format!("SELECT * FROM {table} WHERE id = ?1");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut d = Detail::default();
    for (idx, name) in names.iter().enumerate() {
        if name == "id" {
            continue;
        }
        let value = match row.get_ref(idx)? {
            ValueRef::Null => continue,
            ValueRef::Integer(v) => v,
            ValueRef::Real(v) => v as i64,
            ValueRef::Text(t) | ValueRef::Blob(t) => {
                if t == b"t" {
                    1
                } else if t == b"f" {
                    0
                } else {
                    String::from_utf8_lossy(t)
                        .trim()
                        .parse::<i64>()
                        .unwrap_or(0)
                }
            }
        };
        d.values.insert(name.clone(), value);
    }
    Ok(Some(d))
}

fn load_zones(conn: &Connection) -> Result<HashMap<u32, String>> {
    let mut zones = HashMap::new();
    for_each_row(conn, "SELECT id, name FROM zones", [], |r| {
        zones.insert(r.u32("id")?, r.string("name")?);
        Ok(())
    })?;
    Ok(zones)
}

fn load_groups(
    conn: &Connection,
    table: &str,
    key: &str,
    value: &str,
) -> Result<HashMap<u32, Vec<u32>>> {
    let mut groups: HashMap<u32, Vec<u32>> = HashMap::new();
    for_each_row(conn, &format!("SELECT * FROM {table}"), [], |r| {
        groups.entry(r.u32(key)?).or_default().push(r.u32(value)?);
        Ok(())
    })?;
    Ok(groups)
}

fn load_spawns(path: &Path) -> Result<HashMap<u32, Vec<[f32; 3]>>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading spawn file {}", path.display()))?;
    let value: serde_json::Value = aaemu_game::data::world_data::parse_lenient_json(&text)
        .with_context(|| format!("parsing spawn file {}", path.display()))?;
    let mut out: HashMap<u32, Vec<[f32; 3]>> = HashMap::new();
    let Some(list) = value.as_array() else {
        return Ok(out);
    };
    for entry in list {
        let Some(id) = entry.get("UnitId").and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(p) = entry.get("Position") else {
            continue;
        };
        let x = p.get("X").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let y = p.get("Y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        let z = p.get("Z").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        out.entry(id as u32).or_default().push([x, y, z]);
    }
    Ok(out)
}

impl Catalog {
    pub fn load(compact: &Path, data_dir: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(compact, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", compact.display()))?;
        let mut cat = Catalog {
            zones: load_zones(&conn)?,
            ..Catalog::default()
        };
        cat.npc_groups = load_groups(
            &conn,
            "quest_monster_npcs",
            "quest_monster_group_id",
            "npc_id",
        )?;
        cat.item_groups = load_groups(
            &conn,
            "quest_item_group_items",
            "quest_item_group_id",
            "item_id",
        )?;

        // ---- quest templates / components / acts --------------------------------
        let mut templates: Vec<TemplateRow> = Vec::new();
        for_each_row(
            conn_ordered(&conn)?,
            "SELECT * FROM quest_contexts ORDER BY id",
            [],
            |r| {
                let category = r.u32("category_id")?;
                if category == 45 {
                    return Ok(());
                }
                templates.push((
                    r.u32("id")?,
                    r.string("name")?,
                    r.u8_or("LEVEL", 0)?,
                    r.u32("zone_id")?,
                    category,
                    r.u32("detail_id")?,
                    r.bool_str("REPEATABLE")?,
                    r.bool_str("let_it_done")?,
                    r.i32_or("score", 0)?,
                ));
                Ok(())
            },
        )?;

        let mut comps: HashMap<u32, Comp> = HashMap::new();
        let mut comp_order: HashMap<u32, Vec<u32>> = HashMap::new();
        for_each_row(
            conn_ordered(&conn)?,
            "SELECT * FROM quest_components ORDER BY quest_context_id, component_kind_id, id",
            [],
            |r| {
                let id = r.u32("id")?;
                let quest = r.u32("quest_context_id")?;
                let comp = Comp {
                    kind: r.u8("component_kind_id")?,
                };
                comp_order.entry(quest).or_default().push(id);
                comps.insert(id, comp);
                Ok(())
            },
        )?;

        let mut acts_by_comp: HashMap<u32, Vec<RawAct>> = HashMap::new();
        let mut seen_detail: HashSet<(String, u32)> = HashSet::new();
        for_each_row(
            conn_ordered(&conn)?,
            "SELECT * FROM quest_acts ORDER BY quest_component_id, id",
            [],
            |r| {
                let comp = r.u32("quest_component_id")?;
                if !comps.contains_key(&comp) {
                    return Ok(());
                }
                let ty = r.string("act_detail_type")?;
                let detail_id = r.u32("act_detail_id")?;
                if !seen_detail.insert((ty.clone(), detail_id)) {
                    return Ok(());
                }
                acts_by_comp.entry(comp).or_default().push(RawAct {
                    id: r.u32("id")?,
                    component: comp,
                    detail_id,
                    ty,
                });
                Ok(())
            },
        )?;

        let mut reqs: HashMap<u32, Vec<Gate>> = HashMap::new();
        for_each_row(
            conn_ordered(&conn)?,
            "SELECT * FROM unit_reqs WHERE owner_type = 'QuestComponent'",
            [],
            |r| {
                let owner = r.u32("owner_id")?;
                let kind = r.u32("kind_id")?;
                let v1 = r.u32("value1")?;
                let v2 = r.u32("value2")?;
                let (supported, text) = gate_text(kind, v1, v2);
                reqs.entry(owner).or_default().push(Gate {
                    kind,
                    value1: v1,
                    value2: v2,
                    supported,
                    text,
                });
                Ok(())
            },
        )?;

        let mut detail_cache: HashMap<(String, u32), Option<Detail>> = HashMap::new();
        let mut detail = |conn: &Connection, class: &str, id: u32| -> Result<Detail> {
            let key = (class.to_string(), id);
            if let Some(d) = detail_cache.get(&key) {
                return Ok(d.clone().unwrap_or_default());
            }
            let d = read_detail(conn, class, id)?;
            detail_cache.insert(key, d.clone());
            Ok(d.unwrap_or_default())
        };

        // ---- quests -------------------------------------------------------------
        let mut item_ids: HashSet<u32> = HashSet::new();
        let mut npc_ids: HashSet<u32> = HashSet::new();
        let mut doodad_ids: HashSet<u32> = HashSet::new();
        for (id, name, level, zone_id, category, detail_id, repeatable, let_it_done, score) in
            templates
        {
            let quest_comps: Vec<u32> = comp_order.get(&id).cloned().unwrap_or_default();
            let zone = cat.zones.get(&zone_id).cloned().unwrap_or_default();
            let instance = zone.starts_with("instance") || zone.starts_with("arche_mall");

            let mut objectives = Vec::new();
            let mut gates = Vec::new();
            let mut gate_items = Vec::new();
            let mut unsupported = None;
            let mut act_index: u8 = 0;
            let mut last_kind: u8 = 0xff;
            let mut selective_index: i32 = 0;
            let mut rewards = Rewards::default();
            let mut starter = Starter::none();
            let mut start_component = 0;
            let mut turnin = Turnin::none();

            for cid in &quest_comps {
                let Some(comp) = comps.get(cid) else { continue };
                if comp.kind != last_kind {
                    act_index = 0;
                    last_kind = comp.kind;
                }
                if let Some(list) = reqs.get(cid) {
                    for g in list {
                        if !g.supported && unsupported.is_none() {
                            unsupported = Some(format!("gate {}", g.text));
                        }
                        if g.kind == 10 {
                            gate_items.push(g.value1);
                        }
                        gates.push(g.clone());
                    }
                }
                let acts = acts_by_comp.get(cid).cloned().unwrap_or_default();
                for act in acts {
                    let d = detail(&conn, &act.ty, act.detail_id)?;
                    let counts = counts_as_objective(&act.ty);
                    let objective_index = if counts {
                        let i = act_index;
                        act_index = act_index.wrapping_add(1);
                        i
                    } else {
                        0xff
                    };
                    match comp.kind {
                        kind::START => {
                            if starter.kind == "none" || starter.kind == "auto" {
                                match act.ty.as_str() {
                                    "QuestActConAcceptNpc"
                                    | "QuestActConAcceptNpcKill"
                                    | "QuestActConAcceptNpcEmotion" => {
                                        let npc = d.v("npc_id");
                                        starter = Starter {
                                            kind: "npc".into(),
                                            npc,
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                        npc_ids.insert(npc);
                                        if npc == 0 {
                                            starter.kind = "none".into();
                                        }
                                    }
                                    "QuestActConAcceptDoodad" => {
                                        let doodad = d.v("doodad_id");
                                        starter = Starter {
                                            kind: "doodad".into(),
                                            doodad,
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                        doodad_ids.insert(doodad);
                                    }
                                    "QuestActConAcceptItem" => {
                                        let item = d.v("item_id");
                                        starter = Starter {
                                            kind: "item".into(),
                                            item,
                                            count: 1,
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                        item_ids.insert(item);
                                    }
                                    "QuestActConAcceptItemGain" => {
                                        let item = d.v("item_id");
                                        starter = Starter {
                                            kind: "item_gain".into(),
                                            item,
                                            count: d.i("count").max(1),
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                        item_ids.insert(item);
                                    }
                                    "QuestActConAcceptSphere" => {
                                        let sphere = d.v("sphere_id");
                                        starter = Starter {
                                            kind: "sphere".into(),
                                            sphere,
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                    }
                                    "QuestActConAcceptLevelUp"
                                    | "QuestActConAutoComplete"
                                    | "QuestActConAcceptComponent" => {
                                        starter = Starter {
                                            kind: "auto".into(),
                                            ..Starter::none()
                                        };
                                        starter.act = act.ty.clone();
                                        start_component = *cid;
                                    }
                                    "QuestActConAcceptItemEquip"
                                    | "QuestActConAcceptSkill"
                                    | "QuestActConAcceptBuff"
                                        if unsupported.is_none() =>
                                    {
                                        unsupported = Some(format!("starter {}", act.ty));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        kind::PROGRESS => {
                            if !counts {
                                continue;
                            }
                            match act.ty.as_str() {
                                "QuestActObjTalk" => {
                                    let npc = d.v("npc_id");
                                    npc_ids.insert(npc);
                                    objectives.push(objective(
                                        &act,
                                        objective_index,
                                        &d,
                                        npc,
                                        Vec::new(),
                                        score,
                                        let_it_done,
                                    ));
                                }
                                "QuestActObjTalkNpcGroup" => {
                                    let group = d.v("npc_group_id");
                                    let npcs =
                                        cat.npc_groups.get(&group).cloned().unwrap_or_default();
                                    npc_ids.extend(npcs.iter().copied());
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs,
                                        doodad: 0,
                                        highlight_doodad: 0,
                                        item: 0,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjMonsterHunt" => {
                                    let npc = d.v("npc_id");
                                    npc_ids.insert(npc);
                                    objectives.push(objective(
                                        &act,
                                        objective_index,
                                        &d,
                                        npc,
                                        Vec::new(),
                                        score,
                                        let_it_done,
                                    ));
                                }
                                "QuestActObjMonsterGroupHunt" => {
                                    let group = d.v("quest_monster_group_id");
                                    let npcs =
                                        cat.npc_groups.get(&group).cloned().unwrap_or_default();
                                    npc_ids.extend(npcs.iter().copied());
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs,
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item: 0,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjItemGather" => {
                                    let item = d.v("item_id");
                                    item_ids.insert(item);
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjItemGroupGather" => {
                                    let group = d.v("item_group_id");
                                    let items =
                                        cat.item_groups.get(&group).cloned().unwrap_or_default();
                                    item_ids.extend(items.iter().copied());
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item: 0,
                                        items,
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjItemUse" => {
                                    let item = d.v("item_id");
                                    item_ids.insert(item);
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjItemGroupUse" => {
                                    let group = d.v("item_group_id");
                                    let items =
                                        cat.item_groups.get(&group).cloned().unwrap_or_default();
                                    item_ids.extend(items.iter().copied());
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item: 0,
                                        items,
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjInteraction" => {
                                    let doodad = d.v("doodad_id");
                                    doodad_ids.insert(doodad);
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item: 0,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: 0,
                                    });
                                }
                                "QuestActObjSphere" => {
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: d.v("npc_id"),
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: d.v("highlight_doodad_id"),
                                        item: 0,
                                        items: Vec::new(),
                                        sphere: d.v("sphere_id"),
                                        level: 0,
                                    });
                                }
                                "QuestActObjLevel" => {
                                    objectives.push(Objective {
                                        class: act.ty.clone(),
                                        component: *cid,
                                        act_id: act.id,
                                        objective_index,
                                        required: required_count(score, let_it_done, &d),
                                        npc: 0,
                                        npcs: Vec::new(),
                                        doodad: 0,
                                        highlight_doodad: 0,
                                        item: 0,
                                        items: Vec::new(),
                                        sphere: 0,
                                        level: d.v("level") as u8,
                                    });
                                }
                                other => {
                                    if unsupported.is_none() {
                                        unsupported = Some(format!("objective {other}"));
                                    }
                                }
                            }
                        }
                        kind::READY => match act.ty.as_str() {
                            "QuestActConReportNpc" => {
                                if turnin.kind != "npc" {
                                    let npc = d.v("npc_id");
                                    npc_ids.insert(npc);
                                    turnin = Turnin {
                                        kind: "npc".into(),
                                        npc,
                                        doodad: 0,
                                        act: act.ty.clone(),
                                    };
                                }
                            }
                            "QuestActConReportDoodad" => {
                                if turnin.kind == "none" {
                                    let doodad = d.v("doodad_id");
                                    doodad_ids.insert(doodad);
                                    turnin = Turnin {
                                        kind: "doodad".into(),
                                        npc: 0,
                                        doodad,
                                        act: act.ty.clone(),
                                    };
                                }
                            }
                            "QuestActConReportJournal" | "QuestActConAutoComplete"
                                if turnin.kind == "none" =>
                            {
                                turnin = Turnin {
                                    kind: "auto".into(),
                                    npc: 0,
                                    doodad: 0,
                                    act: act.ty.clone(),
                                };
                            }
                            _ => {}
                        },
                        kind::REWARD => match act.ty.as_str() {
                            "QuestActSupplyExp" => rewards.exp += i64::from(d.i("exp")),
                            "QuestActSupplyCopper" => rewards.copper += i64::from(d.i("amount")),
                            "QuestActSupplyItem" => rewards.items.push(RewardItem {
                                item: d.v("item_id"),
                                count: d.i("count").max(1),
                                choice: 0,
                            }),
                            "QuestActSupplySelectiveItem" => {
                                selective_index += 1;
                                let item = RewardItem {
                                    item: d.v("item_id"),
                                    count: 1,
                                    choice: selective_index,
                                };
                                item_ids.insert(item.item);
                                rewards.items.push(item.clone());
                                rewards.choices.push(item);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
            }

            // A quest with no acts at all is a placeholder row of the quest tables.
            let empty = quest_comps
                .iter()
                .all(|cid| acts_by_comp.get(cid).map(|a| a.is_empty()).unwrap_or(true));
            if empty && unsupported.is_none() {
                unsupported = Some("empty template".into());
            }
            if starter.kind == "none" && unsupported.is_none() {
                unsupported = Some("no usable start act".into());
            }
            if starter.kind == "sphere" && unsupported.is_none() {
                unsupported = Some("starter is a quest sphere".into());
            }
            if turnin.kind == "none" {
                // No report act anywhere: the engine walks Ready -> Reward -> Completed by
                // itself once the objectives are done.
                turnin = Turnin {
                    kind: "auto".into(),
                    npc: 0,
                    doodad: 0,
                    act: String::new(),
                };
            }

            cat.quests.push(Quest {
                id,
                name,
                level,
                zone_id,
                zone,
                category_id: category,
                detail_id,
                repeatable,
                let_it_done,
                score,
                start_component,
                starter,
                objectives,
                turnin,
                rewards,
                gates,
                gate_items,
                unsupported,
                instance,
            });
        }

        // ---- spawn positions and item use skills --------------------------------
        let spawn_dir = data_dir.join("Worlds").join("main_world");
        let npc_all = load_spawns(&spawn_dir.join("npc_spawns.json"))?;
        let doodad_all = load_spawns(&spawn_dir.join("doodad_spawns.json"))?;
        for id in npc_ids {
            if let Some(p) = npc_all.get(&id) {
                cat.npc_spawns.insert(id, p.clone());
            }
        }
        for id in doodad_ids {
            if let Some(p) = doodad_all.get(&id) {
                cat.doodad_spawns.insert(id, p.clone());
            }
        }
        for id in item_ids {
            let mut info = ItemInfo {
                name: String::new(),
                use_skill: 0,
            };
            for_each_row(&conn, "SELECT * FROM items WHERE id = ?1", [id], |r| {
                info.name = r.string_or("name", "")?;
                info.use_skill = r.u32_or("use_skill_id", 0)?;
                Ok(())
            })?;
            cat.items.insert(id, info);
        }

        cat.npc_groups = load_groups(
            &conn,
            "quest_monster_npcs",
            "quest_monster_group_id",
            "npc_id",
        )?;
        cat.item_groups = load_groups(
            &conn,
            "quest_item_group_items",
            "quest_item_group_id",
            "item_id",
        )?;
        cat.load_doodad_skills(&conn)?;
        // Doodad objectives whose template offers no skill in any func group cannot be played
        // through a real packet; report them as unsupported instead of failing at runtime.
        for q in &mut cat.quests {
            if q.unsupported.is_some() {
                continue;
            }
            for o in &q.objectives {
                let doodad = if o.class == "QuestActObjInteraction" {
                    o.doodad
                } else if o.class.contains("ItemGather") || o.class.contains("ItemUse") {
                    o.highlight_doodad
                } else {
                    0
                };
                if doodad != 0 && !cat.doodad_skills.templates_with_skill.contains(&doodad) {
                    q.unsupported = Some(format!(
                        "doodad {doodad} has no interaction skill in the data"
                    ));
                    break;
                }
            }
        }
        Ok(cat)
    }

    /// Doodad templates' initial func groups and the skill each group offers.
    fn load_doodad_skills(&mut self, conn: &Connection) -> Result<()> {
        let mut group_template: HashMap<u32, u32> = HashMap::new();
        for_each_row(
            conn,
            "SELECT * FROM doodad_func_groups ORDER BY id",
            [],
            |r| {
                let id = r.u32("id")?;
                let template = r.u32("doodad_almighty_id")?;
                group_template.entry(id).or_insert(template);
                Ok(())
            },
        )?;
        for_each_row(
            conn,
            "SELECT * FROM doodad_funcs ORDER BY doodad_func_group_id, actual_func_id",
            [],
            |r| {
                let group = r.u32("doodad_func_group_id")?;
                let skill = r.u32_or("func_skill_id", 0)?;
                let ty = r.string("actual_func_type")?;
                let detail_id = r.u32("actual_func_id")?;
                if skill > 0 {
                    self.doodad_skills.group_skill.entry(group).or_insert(skill);
                } else if ty.starts_with("DoodadFunc") {
                    if let Ok(Some(d)) = read_detail_doodad(conn, &ty, detail_id) {
                        for key in ["fake_skill_id", "skill_id", "use_skill_id"] {
                            let v = d.v(key);
                            if v > 0 {
                                self.doodad_skills.group_skill.entry(group).or_insert(v);
                                break;
                            }
                        }
                    }
                }
                Ok(())
            },
        )?;
        for (group, template) in group_template {
            self.doodad_skills
                .template_group
                .entry(template)
                .or_insert(group);
            if let Some(skill) = self.doodad_skills.group_skill.get(&group).copied() {
                self.doodad_skills
                    .template_skill
                    .entry(template)
                    .or_insert(skill);
            }
        }
        Ok(())
    }

    pub fn summary(&self, cfg: &CrawlConfig) -> Summary {
        let mut playable = 0;
        let mut unsupported = 0;
        for q in &self.quests {
            if q.level < cfg.levels.0 || q.level > cfg.levels.1 {
                continue;
            }
            if q.unsupported.is_some() {
                unsupported += 1;
            } else {
                playable += 1;
            }
        }
        Summary {
            playable,
            unsupported,
        }
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Nearest authored spawn of `template` to `from`.
    pub fn nearest_npc(&self, template: u32, from: (f32, f32, f32)) -> Option<[f32; 3]> {
        nearest(self.npc_spawns.get(&template), from)
    }

    pub fn nearest_doodad(&self, template: u32, from: (f32, f32, f32)) -> Option<[f32; 3]> {
        nearest(self.doodad_spawns.get(&template), from)
    }

    /// The race/faction gate verdict for a quest.
    pub fn gate_verdict(
        &self,
        quest: &Quest,
        race: Race,
        level: u8,
        completed: &HashSet<u32>,
    ) -> GateVerdict {
        let mut items = Vec::new();
        for g in &quest.gates {
            if !g.supported {
                return GateVerdict::Skip(g.text.clone());
            }
            match g.kind {
                1 => {
                    if u32::from(level) < g.value1 || (g.value2 != 0 && u32::from(level) > g.value2)
                    {
                        return GateVerdict::Skip(format!("level {}-{}", g.value1, g.value2));
                    }
                }
                3 => {
                    if u32::from(race.id()) != g.value1 {
                        return GateVerdict::Skip(format!("race {}", g.value1));
                    }
                }
                4 => {
                    if g.value1 != 1 {
                        return GateVerdict::Skip(format!("gender {}", g.value1));
                    }
                }
                10 => items.push(g.value1),
                31 => {
                    if !completed.contains(&g.value1) {
                        return GateVerdict::Skip(format!("quest {} not completed", g.value1));
                    }
                }
                36 => {
                    if completed.contains(&g.value1) {
                        return GateVerdict::Skip(format!("quest {} completed", g.value1));
                    }
                }
                40 | 55 => {
                    if race.faction_id() != g.value1 && race.mother_faction_id() != g.value1 {
                        return GateVerdict::Skip(format!("faction {}", g.value1));
                    }
                }
                42 | 56 => {
                    if race.mother_faction_id() != g.value1 {
                        return GateVerdict::Skip(format!("mother faction {}", g.value1));
                    }
                }
                45 => {
                    if g.value1 > 0 || g.value2 > 0 {
                        return GateVerdict::Skip("honor points".into());
                    }
                }
                58 if race.faction_id() == g.value1 => {
                    return GateVerdict::Skip(format!("faction {} excluded", g.value1));
                }
                59 if race.mother_faction_id() == g.value1 => {
                    return GateVerdict::Skip(format!("mother faction {} excluded", g.value1));
                }
                69 if u32::from(level) > g.value1 => {
                    return GateVerdict::Skip(format!("max level {}", g.value1));
                }
                _ => {}
            }
        }
        GateVerdict::Play { items }
    }
}

pub enum GateVerdict {
    Play { items: Vec<u32> },
    Skip(String),
}

fn nearest(points: Option<&Vec<[f32; 3]>>, from: (f32, f32, f32)) -> Option<[f32; 3]> {
    let points = points?;
    let d2 = |p: &[f32; 3]| {
        (p[0] - from.0) * (p[0] - from.0)
            + (p[1] - from.1) * (p[1] - from.1)
            + (p[2] - from.2) * (p[2] - from.2)
    };
    points
        .iter()
        .copied()
        .min_by(|a, b| d2(a).total_cmp(&d2(b)))
}

fn objective(
    act: &RawAct,
    objective_index: u8,
    d: &Detail,
    npc: u32,
    npcs: Vec<u32>,
    score: i32,
    let_it_done: bool,
) -> Objective {
    Objective {
        class: act.ty.clone(),
        component: act.component,
        act_id: act.id,
        objective_index,
        required: required_count(score, let_it_done, d),
        npc,
        npcs,
        doodad: 0,
        highlight_doodad: d.v("highlight_doodad_id"),
        item: 0,
        items: Vec::new(),
        sphere: 0,
        level: 0,
    }
}

fn required_count(score: i32, let_it_done: bool, d: &Detail) -> i32 {
    let count = d.i("count");
    let mut val = count;
    if score > 0 && count > 0 {
        val = score / count + 1;
    }
    if val > 0 {
        if let_it_done {
            (val as f32 * 3.0 / 2.0).ceil() as i32
        } else {
            val
        }
    } else {
        1
    }
}

fn gate_text(kind: u32, v1: u32, v2: u32) -> (bool, String) {
    match kind {
        1 => (
            true,
            if v2 == 0 {
                format!("level >= {v1}")
            } else {
                format!("level {v1}-{v2}")
            },
        ),
        2 => (false, format!("ability {v1} level {v2}")),
        3 => (true, format!("race {v1}")),
        4 => (true, format!("gender {v1}")),
        8 => (false, format!("equip slot {v1}")),
        9 => (false, format!("equip item {v1}")),
        10 => (true, format!("own item {v1}")),
        11 => (false, format!("trained skill {v1}")),
        12 => (false, "not in combat".into()),
        15 => (false, format!("buff {v1}")),
        16 => (false, format!("target buff {v1}")),
        26 => (false, format!("target health {v1}-{v2}")),
        31 => (true, format!("quest {v1} completed")),
        32 => (false, format!("quest {v1} in progress")),
        33 => (false, format!("quest {v1} ready")),
        36 => (true, format!("quest {v1} not completed")),
        37 => (false, format!("quest {v1} active and not completed")),
        40 | 55 => (true, format!("faction {v1}")),
        42 | 56 => (true, format!("mother faction {v1}")),
        45 => (true, format!("honor {v1}-{v2}")),
        51 => (false, format!("in zone {v1}")),
        52 => (false, format!("out of zone {v1}")),
        58 => (true, format!("faction {v1} excluded")),
        59 => (true, format!("mother faction {v1} excluded")),
        69 => (true, format!("max level {v1}")),
        other => (false, format!("requirement kind {other} ({v1}, {v2})")),
    }
}

fn read_detail_doodad(conn: &Connection, class: &str, id: u32) -> Result<Option<Detail>> {
    let rest = class.strip_prefix("DoodadFunc").unwrap_or(class);
    let table = format!("doodad_func_{}s", camel_to_snake(rest));
    let sql = format!("SELECT * FROM {table} WHERE id = ?1");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut d = Detail::default();
    for (idx, name) in names.iter().enumerate() {
        if name == "id" {
            continue;
        }
        let value = match row.get_ref(idx)? {
            ValueRef::Null => continue,
            ValueRef::Integer(v) => v,
            ValueRef::Real(v) => v as i64,
            ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t)
                .trim()
                .parse::<i64>()
                .unwrap_or(0),
        };
        d.values.insert(name.clone(), value);
    }
    Ok(Some(d))
}

/// Borrow-free helper so the closures above do not fight over `conn` mutability.
fn conn_ordered(conn: &Connection) -> Result<&Connection> {
    Ok(conn)
}

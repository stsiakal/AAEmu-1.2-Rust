//! Play quests of one race through the real client packets, one quest at a time.
//!
//! GM commands are only setup: `/level`, `/move`, `/item add`, `/spawn npc|doodad`, `/revive`
//! and the read-only `/sphere list`. Every quest is accepted, progressed and turned in with the
//! same packets the 1.2 client sends; objective counters are read from the quest packets.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, pp, sc};
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use super::catalog::{Catalog, GateVerdict, Objective, Quest};
use super::CrawlConfig;
use crate::game::{
    AddedStack, BagStack, DoodadInfo, GameClient, GameState, Packet, UnitInfo, LEVEL_PROXY,
};
use crate::login::LoginClient;
use crate::record::Recorder;
use crate::stream::StreamClient;

const QUIET: Duration = Duration::from_millis(400);
const CAST_ANSWER: Duration = Duration::from_secs(8);

/// One quest's verdict, appended to `<race>-<character>.jsonl` for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestResult {
    pub quest: u32,
    pub name: String,
    pub level: u8,
    pub zone: String,
    pub race: String,
    /// The character that played it. Absent in old per-race result files, which the dashboard
    /// ignores.
    #[serde(default)]
    pub character: String,
    /// `passed`, `daily-limit`, `failed`, `unsupported` or `skipped`.
    pub status: String,
    /// `accept`, `objective 2`, `turn-in`, ...
    pub step: String,
    pub reason: String,
    /// The quest passed because a GM command completed one of its objectives (item hand-over or
    /// level), so its objective was not really exercised.
    #[serde(default)]
    pub gm_assisted: bool,
    /// The objectives completed with a GM command (`objective 1 QuestActObjItemGather`, ...).
    #[serde(default)]
    pub gm_assisted_steps: Vec<String>,
    pub act_kinds: Vec<String>,
    pub objectives: Vec<ObjectiveResult>,
    pub duration_ms: u64,
    pub warnings: Vec<String>,
    pub reports: Vec<String>,
    pub account_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectiveResult {
    pub index: u8,
    pub class: String,
    pub required: i32,
    pub reached: i32,
}

/// A failed step of one quest (never aborts the crawler).
#[derive(Debug)]
struct QuestFailure {
    step: String,
    reason: String,
}

impl QuestFailure {
    fn new(step: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for QuestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.reason)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct QuestState {
    status: u8,
    objectives: [i32; 5],
}

#[derive(Default)]
struct QuestTracker {
    states: HashMap<u32, QuestState>,
}

impl QuestTracker {
    /// Copies the quest state `GameClient` collected from every accepted packet; packets read
    /// by `wait_for` never reach a drain, so this is the reliable source.
    fn sync(&mut self, state: &GameState) {
        for (quest, (status, objectives)) in &state.quest_states {
            self.states.insert(
                *quest,
                QuestState {
                    status: *status,
                    objectives: *objectives,
                },
            );
        }
    }

    fn observe(&mut self, packets: &[Packet]) {
        for p in packets {
            if p.is_sc(sc::SCQuestContextStartedPacket) || p.is_sc(sc::SCQuestContextUpdatedPacket)
            {
                if let Some((quest, state)) = quest_state(p) {
                    self.states.insert(quest, state);
                }
            } else if p.is_sc(sc::SCQuestContextCompletedPacket) {
                let mut s = p.stream();
                if let Ok(quest) = s.read_u32() {
                    let e = self.states.entry(quest).or_default();
                    e.status = 5;
                }
            } else if p.is_sc(sc::SCQuestContextFailedPacket) {
                // The server never sends this; a real failure is an Updated with status 2.
                let mut s = p.stream();
                if let Ok(quest) = s.read_u32() {
                    let e = self.states.entry(quest).or_default();
                    e.status = 2;
                }
            }
        }
    }

    fn state(&self, quest: u32) -> Option<QuestState> {
        self.states.get(&quest).copied()
    }

    fn progress(&self, quest: u32, index: u8) -> i32 {
        self.state(quest)
            .and_then(|s| s.objectives.get(index as usize).copied())
            .unwrap_or(0)
    }

    fn status(&self, quest: u32) -> u8 {
        self.state(quest).map_or(0, |s| s.status)
    }
}

/// Objectives the GM setup had already satisfied when the quest started: a `QuestActObjLevel`
/// act reads the setup level and an item gather act counts the bag setup filled. They are
/// complete at accept, so the objective loop never calls `play_objective` for them and cannot
/// notice the GM shortcut itself.
fn setup_assisted_steps(
    state: Option<QuestState>,
    quest: &Quest,
    setup_level: u8,
    setup_items: &[u32],
) -> Vec<String> {
    let Some(state) = state else {
        return Vec::new();
    };
    quest
        .objectives
        .iter()
        .enumerate()
        .filter_map(|(i, obj)| {
            let reached = state
                .objectives
                .get(obj.objective_index as usize)
                .copied()
                .unwrap_or(0);
            if reached < obj.required {
                return None;
            }
            let by_setup = match obj.class.as_str() {
                "QuestActObjLevel" => setup_level >= obj.level.max(2),
                "QuestActObjItemGather" => obj.item != 0 && setup_items.contains(&obj.item),
                "QuestActObjItemGroupGather" => {
                    obj.items.iter().any(|item| setup_items.contains(item))
                }
                _ => false,
            };
            by_setup.then(|| format!("objective {} {}", i + 1, obj.class))
        })
        .collect()
}

/// The quest id of an `SCQuestContextCompletedPacket` (its first field).
fn completed_quest_id(p: &Packet) -> Option<u32> {
    if !p.is_sc(sc::SCQuestContextCompletedPacket) {
        return None;
    }
    p.stream().read_u32().ok()
}

fn quest_state(p: &Packet) -> Option<(u32, QuestState)> {
    let mut s = p.stream();
    s.read_i64().ok()?;
    let quest = s.read_u32().ok()?;
    let status = s.read_u8().ok()?;
    let mut objectives = [0; 5];
    for o in &mut objectives {
        *o = s.read_i32().ok()?;
    }
    Some((quest, QuestState { status, objectives }))
}

struct Session {
    game: GameClient,
    _stream: StreamClient,
    _login: LoginClient,
    tracker: QuestTracker,
    level: u8,
    account_id: u32,
    /// NPC/doodad objects spawned by the crawler, removed after the quest.
    spawned_npcs: Vec<u32>,
    spawned_doodads: Vec<u32>,
}

impl Session {
    async fn pump(&mut self) -> Result<Vec<Packet>> {
        let packets = self.game.drain(QUIET, Duration::from_millis(1500)).await?;
        self.tracker.observe(&packets);
        self.tracker.sync(&self.game.state);
        for p in &packets {
            if p.is_sc(sc::SCLevelChangedPacket) {
                let mut s = p.stream();
                if s.read_bc().is_ok() {
                    if let Ok(level) = s.read_u8() {
                        self.level = level;
                    }
                }
            }
        }
        ensure!(!self.game.state.disconnected, "disconnected");
        Ok(packets)
    }

    async fn sleep_pump(&mut self, ms: u64) -> Result<Vec<Packet>> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        self.pump().await
    }

    async fn pump_fast(&mut self, max_ms: u64) -> Result<Vec<Packet>> {
        let packets = self
            .game
            .drain(Duration::from_millis(120), Duration::from_millis(max_ms))
            .await?;
        self.tracker.observe(&packets);
        self.tracker.sync(&self.game.state);
        ensure!(!self.game.state.disconnected, "disconnected");
        Ok(packets)
    }
}

/// Entry point: connect, then play the quests of `cfg.race`.
pub async fn run(cfg: &CrawlConfig, catalog: &Catalog) -> Result<()> {
    let result_path = cfg.result_path();
    let failed = if cfg.skip_failed {
        load_failed(&result_path)
    } else {
        HashSet::new()
    };
    let mut results = ResultsFile::open(&result_path)?;

    let mut session = connect(cfg, catalog).await?;
    session.gm_bootstrap().await?;
    let account_id = session.account_id;
    // Gates and skips come from the server's completed-quest list of THIS character, not from
    // the result file: two characters of the same race must never inherit each other's state.
    let mut passed: HashSet<u32> = session.game.state.completed_quests.clone();
    let mut seeded: Vec<u32> = passed.iter().copied().collect();
    seeded.sort_unstable();
    println!(
        "[crawl] {} completed quest(s) seeded from the character: {}",
        seeded.len(),
        seeded
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut quests: Vec<&Quest> = catalog
        .quests
        .iter()
        .filter(|q| q.level >= cfg.levels.0 && q.level <= cfg.levels.1)
        .filter(|q| cfg.from_quest.is_none_or(|from| q.id >= from))
        .collect();
    quests.sort_by_key(|q| (q.level, q.id));
    let attemptable = quests
        .iter()
        .filter(|q| !failed.contains(&q.id) && is_attemptable(cfg, catalog, q, &passed))
        .count();
    let budget = cfg.limit.map(|n| n.min(attemptable));
    println!(
        "[crawl] {}: {} quests to attempt ({attemptable} attemptable, {} already completed on this \
         character)",
        cfg.race.name(),
        budget.map_or_else(|| "all".to_string(), |b| b.to_string()),
        passed.len()
    );

    let mut visited = 0usize;
    for quest in quests {
        if budget.is_some_and(|b| visited >= b) {
            break;
        }
        if passed.contains(&quest.id) || failed.contains(&quest.id) {
            continue;
        }
        let attempt = is_attemptable(cfg, catalog, quest, &passed);
        if attempt {
            visited += 1;
        }
        let outcome = crawl_quest(&mut session, cfg, catalog, quest, &passed).await;
        let result = finish_result(cfg, quest, outcome, account_id);
        println!(
            "[{}/{}] {} quest {:<5} {:<24} L{:<2} {:<30} {}",
            visited,
            budget.map_or_else(|| "all".to_string(), |b| b.to_string()),
            match result.status.as_str() {
                "passed" if result.gm_assisted => "okGM",
                "passed" => "ok  ",
                "daily-limit" => "limit",
                "skipped" => "skip",
                "unsupported" => "UNSP",
                _ => "FAIL",
            },
            result.quest,
            result.name,
            result.level,
            result.step,
            result.reason
        );
        // A quest that passed unlocks the quests gated on it, so later quests in the same run
        // can be attempted without a second pass. Error 621 (daily limit) does not unlock.
        if result.status == "passed" {
            passed.insert(result.quest);
        }
        results.append(&result)?;
        if attempt && visited.is_multiple_of(10) {
            if let Err(e) = super::dashboard::write(&cfg.out) {
                println!("[crawl] dashboard: {e:#}");
            }
        }
        if !attempt {
            continue;
        }
        // Questing kills the bot now and then; GM revive is setup for the next quest.
        if !session.game.state.disconnected {
            let _ = session.pump().await;
            if session
                .game
                .state
                .dead_units
                .contains(&session.game.state.self_obj)
            {
                let _ = gm_command(&mut session.game, "revive").await;
            }
        } else {
            // Reconnect and keep going: a disconnect is not a quest verdict.
            match connect(cfg, catalog).await {
                Ok(mut fresh) => {
                    fresh.gm_bootstrap().await?;
                    fresh.tracker = session.tracker;
                    // The new connection rebuilt the bag from its inventory snapshot. Stacks
                    // this crawler created whole are still provably ours; merged stacks may
                    // have lost the crawler's units while disconnected, so forget those.
                    fresh.game.state.added_items =
                        std::mem::take(&mut session.game.state.added_items);
                    for (id, units) in
                        forget_merged_after_reconnect(&mut fresh.game.state.added_items)
                    {
                        println!(
                            "[crawl] cleanup: not destroying {units} merged unit(s) of stack {id} \
                             after the reconnect"
                        );
                    }
                    session = fresh;
                }
                Err(e) => {
                    println!("[crawl] reconnect failed: {e:#}");
                    break;
                }
            }
        }
    }

    let _ = session.sleep_pump(500).await;
    cleanup_added_items(&mut session).await;
    let _ = session.game.leave_world(0).await;
    let _ = session.pump().await;
    Ok(())
}

struct QuestOutcome {
    outcome: std::result::Result<(), QuestFailure>,
    warnings: Vec<String>,
    reports: Vec<String>,
    objectives: Vec<ObjectiveResult>,
    gm_assisted_steps: Vec<String>,
    duration_ms: u64,
}

fn finish_result(
    cfg: &CrawlConfig,
    quest: &Quest,
    out: QuestOutcome,
    account_id: u32,
) -> QuestResult {
    let mut r = QuestResult {
        quest: quest.id,
        name: quest.name.clone(),
        level: quest.level,
        zone: quest.zone.clone(),
        race: cfg.race.name().to_string(),
        character: cfg.character_name(),
        status: "failed".into(),
        step: String::new(),
        reason: String::new(),
        act_kinds: quest.objectives.iter().map(|o| o.class.clone()).collect(),
        objectives: out.objectives,
        duration_ms: out.duration_ms,
        warnings: out.warnings,
        reports: out.reports,
        gm_assisted: false,
        gm_assisted_steps: out.gm_assisted_steps,
        account_id,
    };
    r.gm_assisted = !r.gm_assisted_steps.is_empty();
    match out.outcome {
        Ok(()) => {
            r.status = "passed".into();
            r.step = "complete".into();
            r.reason = "quest completed".into();
        }
        Err(f) => {
            if f.step == "unsupported" {
                r.status = "unsupported".into();
            } else if f.step == "accept" && f.reason.contains("daily limit") {
                // Error 621 (`QuestDailyLimit`). It is not a completion: only the retry marker.
                r.status = "daily-limit".into();
            } else if f.step == "gate" {
                r.status = "skipped".into();
            }
            r.step = f.step;
            r.reason = f.reason;
        }
    }
    r
}

fn load_failed(path: &Path) -> HashSet<u32> {
    load_with_status(path, "failed")
}

fn load_with_status(path: &Path, status: &str) -> HashSet<u32> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<QuestResult>(line).ok())
        .filter(|r| r.status == status)
        .map(|r| r.quest)
        .collect()
}

struct ResultsFile {
    file: std::fs::File,
}

impl ResultsFile {
    fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self { file })
    }

    fn append(&mut self, result: &QuestResult) -> Result<()> {
        use std::io::Write;
        writeln!(self.file, "{}", serde_json::to_string(result)?)?;
        self.file.flush()?;
        Ok(())
    }
}

/// Connect and place one character of the crawler's race in the world.
async fn connect(cfg: &CrawlConfig, _catalog: &Catalog) -> Result<Session> {
    let rec = Recorder::new(false);
    let mut login = LoginClient::connect(&cfg.login, rec.clone()).await?;
    let account_id = login.auth_trion(&cfg.user, &cfg.password).await?;
    ensure!(account_id != 0, "account id 0");

    // The character inherits the account's access level at creation, so grant it before the
    // first character exists (and repair an existing one).
    if !cfg.no_grant_gm {
        if let Err(e) = grant_gm(cfg, account_id) {
            println!("[crawl] GM grant skipped: {e:#}");
        }
    }

    let worlds = login.list_worlds().await?;
    let world = worlds
        .worlds
        .iter()
        .find(|w| w.id == 1)
        .or_else(|| worlds.worlds.first())
        .context("no game server in ACWorldList")?;
    ensure!(world.active, "world {} is not active", world.name);
    let cookie = login.enter_world(world.id).await?;

    let addr = format!("{}:{}", cookie.ip, cookie.port);
    let mut game = GameClient::connect(&addr, rec.clone()).await?;
    game.enter_world(account_id, cookie.cookie).await?;
    game.wait_sc(sc::X2EnterWorldResponsePacket).await?;
    game.wait_for("ChangeStatePacket(0)", |p| {
        p.is(LEVEL_PROXY, pp::ChangeStatePacket)
    })
    .await?;
    for state in 0..=7 {
        game.finish_state(state).await?;
        if state < 7 {
            game.wait_for("ChangeStatePacket", |p| {
                p.is(LEVEL_PROXY, pp::ChangeStatePacket)
            })
            .await?;
        } else {
            game.wait_sc(sc::SCUpdatePremiumPointPacket).await?;
        }
    }
    let stream_addr = format!("{}:{}", cookie.ip, game.state.stream_port);
    let mut stream = StreamClient::connect(&stream_addr, rec.clone()).await?;
    let joined = stream.join(account_id, game.state.token).await?;
    ensure!(joined == 0, "TCJoinResponse {joined}");

    game.list_characters().await?;
    game.wait_for("last SCCharacterListPacket", |p| {
        p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
    })
    .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;

    let name = if cfg.character.is_empty() {
        cfg.race.character_name().to_string()
    } else {
        cfg.character.clone()
    };
    let existing = game
        .state
        .characters
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&name))
        .cloned();
    let character = match existing {
        Some(c) => c,
        None => {
            game.create_character_as(&name, cfg.race.id(), 1, 1).await?;
            let p = game
                .wait_for("character creation result", |p| {
                    p.is_sc(sc::SCCreateCharacterResponsePacket)
                        || p.is_sc(sc::SCCharacterCreationFailedPacket)
                })
                .await?;
            if p.is_sc(sc::SCCharacterCreationFailedPacket) {
                bail!(
                    "SCCharacterCreationFailed reason {:?} for race {}",
                    p.body.first(),
                    cfg.race.id()
                );
            }
            let mut s = p.stream();
            let id = s.read_u32()?;
            crate::game::CharInfo { id, name }
        }
    };

    game.select_character(character.id).await?;
    game.wait_sc(sc::SCCharacterStatePacket).await?;
    game.drain(QUIET, Duration::from_secs(10)).await?;
    game.spawn_character().await?;
    game.wait_for("own SCUnitStatePacket", |p| p.is_sc(sc::SCUnitStatePacket))
        .await?;
    game.notify_in_game().await?;
    game.notify_in_game_completed().await?;
    game.drain(QUIET, Duration::from_secs(15)).await?;
    ensure!(game.state.self_obj != 0, "own unit state not received");
    // Quest targets are often neutral to the race; the real client force-attacks them.
    game.set_force_attack(true).await?;
    // A previous run may have been stopped mid-quest (timeout, crash); start from a clean log.
    let active: Vec<u32> = game.state.active_quests.keys().copied().collect();
    for quest in active {
        let _ = drop_quest(&mut game, quest).await;
    }
    if !game.state.active_quests.is_empty() {
        game.state.active_quests.clear();
        game.drain(QUIET, Duration::from_secs(4)).await?;
    }
    let level = 1;
    Ok(Session {
        game,
        _stream: stream,
        _login: login,
        tracker: QuestTracker::default(),
        level,
        account_id,
        spawned_npcs: Vec::new(),
        spawned_doodads: Vec::new(),
    })
}

impl Session {
    async fn gm_bootstrap(&mut self) -> Result<()> {
        let reply = gm_command(&mut self.game, "quest list").await?;
        let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
        ensure!(
            !texts.iter().any(|t| t.contains("Insufficient privileges")),
            "GM commands refused for account {}: grant access level 100 in the game database \
             (the crawler does it with --game-config; see rust/LIVE-TESTING.md)",
            self.account_id
        );
        ensure!(
            !texts.is_empty(),
            "no reply to /quest list (GM commands not loaded?)"
        );
        gm_command(&mut self.game, "revive").await?;
        Ok(())
    }
}

async fn crawl_quest(
    session: &mut Session,
    cfg: &CrawlConfig,
    catalog: &Catalog,
    quest: &Quest,
    completed: &HashSet<u32>,
) -> QuestOutcome {
    let mut evidence = QuestOutcome {
        outcome: Ok(()),
        warnings: Vec::new(),
        reports: Vec::new(),
        objectives: Vec::new(),
        gm_assisted_steps: Vec::new(),
        duration_ms: 0,
    };
    if let Some(reason) = &quest.unsupported {
        evidence.outcome = Err(QuestFailure::new("unsupported", reason));
        return evidence;
    }
    let verdict = catalog.gate_verdict(
        quest,
        cfg.race,
        quest.level.max(cfg.min_level).max(2),
        completed,
    );
    let gate_items = match verdict {
        GateVerdict::Skip(reason) => {
            evidence.outcome = Err(QuestFailure::new("gate", reason));
            return evidence;
        }
        GateVerdict::Play { items } => items,
    };
    if quest.instance {
        evidence.outcome = Err(QuestFailure::new("gate", "instance world"));
        return evidence;
    }
    let Some(anchor) = anchor_of(catalog, quest) else {
        evidence.outcome = Err(QuestFailure::new(
            "gate",
            "no authored position for its targets",
        ));
        return evidence;
    };

    let started = Instant::now();
    let deadline = started + cfg.quest_timeout;
    let log = LogWindow::new(cfg.server_log.as_deref());
    let reports = ReportWindow::new(cfg.bugreports.as_deref());

    // A reward or hand-over that trailed the previous quest is still in the ledger.
    cleanup_added_items(session).await;
    let body = run_quest_body(session, cfg, catalog, quest, anchor, &gate_items, deadline).await;
    if let Ok(steps) = &body {
        evidence.gm_assisted_steps = steps.clone();
    }
    evidence.outcome = body.map(|_| ());
    evidence.objectives = quest
        .objectives
        .iter()
        .map(|o| ObjectiveResult {
            index: o.objective_index,
            class: o.class.clone(),
            required: o.required,
            reached: session.tracker.progress(quest.id, o.objective_index),
        })
        .collect();
    evidence.warnings = log.warnings();
    evidence.reports = reports.new_reports();
    evidence.duration_ms = started.elapsed().as_millis() as u64;
    if let Err(f) = &mut evidence.outcome {
        if !evidence.warnings.is_empty() {
            f.reason = format!("{} [warn: {}]", f.reason, evidence.warnings.join(" | "));
        }
        if !evidence.reports.is_empty() {
            f.reason = format!("{} [bugreport: {}]", f.reason, evidence.reports.join(", "));
        }
    }
    // Clean up GM-spawned setup objects.
    let npcs = std::mem::take(&mut session.spawned_npcs);
    for obj in npcs {
        let _ = gm_command(&mut session.game, &format!("despawn npc {obj}")).await;
    }
    let doodads = std::mem::take(&mut session.spawned_doodads);
    for obj in doodads {
        let _ = gm_command(&mut session.game, &format!("spawn remove {obj}")).await;
    }
    if evidence.outcome.is_err() {
        let _ = drop_quest(&mut session.game, quest.id).await;
    }
    // Rewards and turn-in item tasks can trail the completed packet.
    let _ = session.sleep_pump(800).await;
    cleanup_added_items(session).await;
    evidence
}

/// Drops the responsibility for merged stacks after a reconnect: their crawler units may have
/// been consumed while the client was away, and destroying `min(owed, current count)` could hit
/// units that existed before the crawler added to the stack. Returns the skipped units to log.
fn forget_merged_after_reconnect(added: &mut HashMap<u64, AddedStack>) -> Vec<(u64, i32)> {
    let mut skipped = Vec::new();
    added.retain(|id, stack| {
        if stack.whole {
            return true;
        }
        if stack.owed > 0 {
            skipped.push((*id, stack.owed));
        }
        false
    });
    skipped.sort_unstable();
    skipped
}

/// The destroys that remove exactly what the crawler's ledger says it added: `(item id, slot
/// type, slot, count)`. Only units still in the bag and still owed are destroyed, so a stack the
/// crawler never touched is never touched by cleanup.
fn items_to_destroy(
    added: &HashMap<u64, AddedStack>,
    bag: &HashMap<u64, BagStack>,
) -> Vec<(u64, u8, u8, i32)> {
    let mut out = Vec::new();
    for (id, added) in added {
        let owed = added.owed;
        if owed <= 0 {
            continue;
        }
        let Some(stack) = bag.get(id) else {
            continue;
        };
        if stack.slot_type != 2 {
            continue;
        }
        let count = owed.min(stack.count);
        if count <= 0 {
            continue;
        }
        out.push((*id, stack.slot_type, stack.slot, count));
    }
    out.sort_unstable();
    out
}

/// Destroys every bag unit the crawler added (GM items, quest rewards, leftover objective items)
/// that is still there, so the bag does not fill up over a run. The ledger survives reconnects
/// while the bag is rebuilt from the new connection's inventory snapshot.
async fn cleanup_added_items(session: &mut Session) {
    let doomed = items_to_destroy(&session.game.state.added_items, &session.game.state.bag);
    for (id, slot_type, slot, count) in &doomed {
        let mut s = PacketStream::new();
        s.write_u64(*id)
            .write_u8(*slot_type)
            .write_u8(*slot)
            .write_i32(*count);
        let _ = session
            .game
            .send_cs(cs::CSDestroyItemPacket, s.as_bytes())
            .await;
    }
    if !doomed.is_empty() {
        let _ = session.sleep_pump(600).await;
        for (id, _, _, count) in &doomed {
            let remove = match session.game.state.added_items.get_mut(id) {
                Some(added) => {
                    added.owed = (added.owed - *count).max(0);
                    added.owed == 0
                }
                None => false,
            };
            if remove {
                session.game.state.added_items.remove(id);
            }
        }
    }
}

async fn run_quest_body(
    session: &mut Session,
    cfg: &CrawlConfig,
    catalog: &Catalog,
    quest: &Quest,
    anchor: (f32, f32, f32),
    gate_items: &[u32],
    deadline: Instant,
) -> std::result::Result<Vec<String>, QuestFailure> {
    let mut gm_assisted = Vec::new();
    // ---- setup: level and gate items -----------------------------------------
    let target_level = quest.level.max(cfg.min_level).max(2);
    if session.level < target_level {
        gm_command(&mut session.game, &format!("level {target_level}"))
            .await
            .map_err(|e| QuestFailure::new("setup", format!("level: {e:#}")))?;
        session
            .pump()
            .await
            .map_err(|e| QuestFailure::new("setup", format!("level pump: {e:#}")))?;
        session.level = target_level;
    }
    let setup_level = session.level;
    // Items setup put in the bag before the accept (a gather act counts the bag when it starts).
    let mut setup_items: Vec<u32> = gate_items.to_vec();
    // A charge skill does direct damage; the auto attack alone cannot kill quest elites.
    let _ = session.game.learn_skill(11918).await;
    let _ = session.game.learn_skill(18131).await;
    for item in gate_items {
        let _ = ensure_item(session, *item, 1, anchor).await;
    }

    // ---- accept ---------------------------------------------------------------
    let accept = accept_quest(session, catalog, quest, anchor, deadline).await;
    setup_items.extend(accept.map_err(|e| QuestFailure::new("accept", e))?);
    // The server runs the acts when the quest starts: an ItemGather act counts the current bag
    // and an ObjLevel act reads the current level. Read that first state before the objective
    // loop can break on it, or a setup-satisfied objective would look like a real pass.
    session
        .pump()
        .await
        .map_err(|e| QuestFailure::new("accept", format!("pump: {e:#}")))?;
    gm_assisted.extend(setup_assisted_steps(
        session.tracker.state(quest.id),
        quest,
        setup_level,
        &setup_items,
    ));

    // ---- objectives -----------------------------------------------------------
    for (i, obj) in quest.objectives.iter().enumerate() {
        let step = format!("objective {} {}", i + 1, obj.class);
        let mut attempts = 0;
        loop {
            session
                .pump()
                .await
                .map_err(|e| QuestFailure::new(&step, format!("pump: {e:#}")))?;
            let state = session.tracker.state(quest.id).unwrap_or_default();
            if state.status == 2 {
                return Err(QuestFailure::new(
                    &step,
                    format!(
                        "quest failed on this objective: {}",
                        last_events(&session.game)
                    ),
                ));
            }
            // Call this on every completion path: an Updated that completes the objective may
            // arrive after the post-accept pump, and status 3/5 can break below.
            for step in setup_assisted_steps(Some(state), quest, setup_level, &setup_items) {
                if !gm_assisted.contains(&step) {
                    gm_assisted.push(step);
                }
            }
            if state.status == 3 || state.status == 5 {
                break;
            }
            if session.tracker.progress(quest.id, obj.objective_index) >= obj.required {
                break;
            }
            if Instant::now() >= deadline {
                return Err(QuestFailure::new(
                    &step,
                    format!(
                        "timeout at {}/{} (quest status {})",
                        session.tracker.progress(quest.id, obj.objective_index),
                        obj.required,
                        state.status
                    ),
                ));
            }
            if attempts >= 12 {
                return Err(QuestFailure::new(
                    &step,
                    format!(
                        "no progress after {attempts} attempts ({}/{}): {}",
                        session.tracker.progress(quest.id, obj.objective_index),
                        obj.required,
                        last_events(&session.game)
                    ),
                ));
            }
            attempts += 1;
            let assisted = play_objective(session, cfg, catalog, quest, obj, anchor, deadline)
                .await
                .map_err(|e| QuestFailure::new(&step, e))?;
            if assisted && !gm_assisted.contains(&step) {
                gm_assisted.push(step.clone());
            }
            let _ = session.sleep_pump(700).await;
        }
    }

    // A few quests have no Progress step at all: the Ready step turns in directly.
    session
        .pump()
        .await
        .map_err(|e| QuestFailure::new("turn-in", format!("pump: {e:#}")))?;
    if session.tracker.status(quest.id) == 5 {
        return Ok(gm_assisted);
    }

    // ---- turn-in --------------------------------------------------------------
    turn_in(session, catalog, quest, anchor, deadline)
        .await
        .map_err(|e| QuestFailure::new("turn-in", e))?;
    Ok(gm_assisted)
}

async fn accept_quest(
    session: &mut Session,
    catalog: &Catalog,
    quest: &Quest,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<Vec<u32>, String> {
    let starter = &quest.starter;
    // Items setup put in the bag: the starter item, whether it is used or gained.
    let starter_items = match starter.kind.as_str() {
        "item" | "item_gain" => vec![starter.item],
        _ => Vec::new(),
    };
    match starter.kind.as_str() {
        "npc" => {
            let obj = goto_npc(session, catalog, starter.npc, anchor, deadline)
                .await
                .map_err(|e| format!("starter npc {}: {e}", starter.npc))?;
            send_start_quest(&mut session.game, quest.id, obj, 0, 0)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        "doodad" => {
            let d = goto_doodad(session, catalog, starter.doodad, anchor, deadline)
                .await
                .map_err(|e| format!("starter doodad {}: {e}", starter.doodad))?;
            send_start_quest(&mut session.game, quest.id, 0, d.obj_id, 0)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        "item" | "item_gain" => {
            let count = starter.count.max(1) as u32;
            // An item-gain starter needs a new acquisition, so always add its count; a used
            // starter only needs the item present.
            let item = match starter.kind.as_str() {
                "item_gain" => gain_item(session, starter.item, count).await,
                _ => ensure_item(session, starter.item, count, anchor).await,
            }
            .map_err(|e| format!("starter item {}: {e}", starter.item))?;
            let _ = session.sleep_pump(800).await;
            if session.tracker.status(quest.id) != 0 {
                return Ok(starter_items);
            }
            if starter.kind == "item" {
                let use_skill = catalog.items.get(&starter.item).map_or(0, |i| i.use_skill);
                if use_skill == 0 {
                    return Err(format!("starter item {} has no use skill", starter.item));
                }
                let me = session.game.state.self_obj;
                session
                    .game
                    .start_item_skill(use_skill, item, starter.item, me)
                    .await
                    .map_err(|e| format!("using starter item: {e:#}"))?;
                let _ = wait_skill(session, use_skill, CAST_ANSWER).await;
            }
        }
        "auto" => {
            send_start_quest(&mut session.game, quest.id, 0, 0, 0)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        other => return Err(format!("starter '{other}' is not supported")),
    }
    // Quest context packets arrive on the next drains; wait for the start.
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until {
        let packets = session
            .pump()
            .await
            .map_err(|e| format!("accept pump: {e:#}"))?;
        let codes: Vec<i16> = packets
            .iter()
            .filter(|p| p.is_sc(sc::SCErrorMsgPacket))
            .filter_map(|p| p.stream().read_i16().ok())
            .collect();
        // 621 = QuestDailyLimit (the server also sends it for an already completed
        // non-repeatable quest). It is not a completion: later runs must retry.
        // 287 = CLOSED_ZONE, often a stale answer to the GM teleport that preceded the accept.
        if codes.contains(&621) {
            return Err("daily limit (error 621)".into());
        }
        if session.tracker.state(quest.id).is_some() {
            return Ok(starter_items);
        }
    }
    Err("no SCQuestContextStartedPacket (server refused or ignored the accept)".into())
}

/// What the quest packets and tracker say about a sent turn-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnInState {
    Pending,
    Completed,
    Failed,
}

fn turn_in_state(quest: u32, status: u8, packets: &[Packet]) -> TurnInState {
    if status == 2 {
        return TurnInState::Failed;
    }
    if status == 5 || packets.iter().any(|p| completed_quest_id(p) == Some(quest)) {
        TurnInState::Completed
    } else {
        TurnInState::Pending
    }
}

async fn turn_in(
    session: &mut Session,
    catalog: &Catalog,
    quest: &Quest,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<(), String> {
    let selected = quest.rewards.choices.first().map(|c| c.choice).unwrap_or(0);
    match quest.turnin.kind.as_str() {
        "npc" => {
            let obj = goto_npc(session, catalog, quest.turnin.npc, anchor, deadline)
                .await
                .map_err(|e| format!("turn-in npc {}: {e}", quest.turnin.npc))?;
            send_complete_quest(&mut session.game, quest.id, obj, 0, selected)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        "doodad" => {
            let d = goto_doodad(session, catalog, quest.turnin.doodad, anchor, deadline)
                .await
                .map_err(|e| format!("turn-in doodad {}: {e}", quest.turnin.doodad))?;
            send_complete_quest(&mut session.game, quest.id, 0, d.obj_id, selected)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        "auto" => {
            send_complete_quest(&mut session.game, quest.id, 0, 0, selected)
                .await
                .map_err(|e| format!("{e:#}"))?;
        }
        other => return Err(format!("turn-in '{other}' is not supported")),
    }
    let until = deadline.min(Instant::now() + Duration::from_secs(12));
    while Instant::now() < until {
        let packets = session
            .pump()
            .await
            .map_err(|e| format!("turn-in pump: {e:#}"))?;
        match turn_in_state(quest.id, session.tracker.status(quest.id), &packets) {
            TurnInState::Completed => return Ok(()),
            TurnInState::Failed => {
                return Err(format!(
                    "quest failed on turn-in: {}",
                    last_events(&session.game)
                ))
            }
            TurnInState::Pending => {}
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    // The deadline may already have passed when the complete packet was sent (the teleport to
    // the turn-in NPC uses it up), so the loop above never ran. Always give the answer the send
    // may produce one final drain before deciding the turn-in failed.
    let packets = session
        .pump()
        .await
        .map_err(|e| format!("turn-in pump: {e:#}"))?;
    match turn_in_state(quest.id, session.tracker.status(quest.id), &packets) {
        TurnInState::Completed => Ok(()),
        TurnInState::Failed => Err(format!(
            "quest failed on turn-in: {}",
            last_events(&session.game)
        )),
        TurnInState::Pending => Err(format!(
            "no SCQuestContextCompletedPacket (status {})",
            session.tracker.status(quest.id)
        )),
    }
}

async fn play_objective(
    session: &mut Session,
    _cfg: &CrawlConfig,
    catalog: &Catalog,
    quest: &Quest,
    obj: &Objective,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<bool, String> {
    // `true` when a GM command completed the objective instead of the client packets.
    let mut gm_assisted = false;
    let current = session.tracker.progress(quest.id, obj.objective_index);
    match obj.class.as_str() {
        "QuestActObjTalk" => {
            let obj_id = goto_npc(session, catalog, obj.npc, anchor, deadline)
                .await
                .map_err(|e| format!("talk npc {}: {e}", obj.npc))?;
            send_talk_made(
                &mut session.game,
                obj_id,
                quest.id,
                obj.component,
                obj.act_id,
            )
            .await
            .map_err(|e| format!("{e:#}"))?;
            let _ = session.sleep_pump(800).await;
        }
        "QuestActObjTalkNpcGroup" => {
            let member = best_member(session, catalog, &obj.npcs, anchor)
                .ok_or_else(|| format!("npc group has no member with a spawn: {:?}", obj.npcs))?;
            let obj_id = goto_npc(session, catalog, member, anchor, deadline)
                .await
                .map_err(|e| format!("talk npc {}: {e}", member))?;
            send_talk_made(
                &mut session.game,
                obj_id,
                quest.id,
                obj.component,
                obj.act_id,
            )
            .await
            .map_err(|e| format!("{e:#}"))?;
            let _ = session.sleep_pump(800).await;
        }
        "QuestActObjMonsterHunt" | "QuestActObjMonsterGroupHunt" => {
            let wanted: Vec<u32> = if obj.npcs.is_empty() {
                vec![obj.npc]
            } else {
                obj.npcs.clone()
            };
            hunt_once(session, catalog, &wanted, anchor, deadline)
                .await
                .map_err(|e| format!("kill {:?}: {e}", wanted))?;
        }
        "QuestActObjItemGather" | "QuestActObjItemGroupGather" => {
            if obj.highlight_doodad != 0 {
                interact_doodad(session, catalog, obj.highlight_doodad, anchor, deadline)
                    .await
                    .map_err(|e| format!("gather doodad {}: {e}", obj.highlight_doodad))?;
            } else {
                let item = if obj.item != 0 {
                    obj.item
                } else {
                    *obj.items
                        .first()
                        .ok_or_else(|| "item group has no members".to_string())?
                };
                let need = (obj.required - current).max(1) as u32;
                // GM hand-over: the loot path is not exercised, so the quest is GM-assisted.
                gm_command(&mut session.game, &format!("item add self {item} {need}"))
                    .await
                    .map_err(|e| format!("gather item {item}: {e:#}"))?;
                let _ = session.sleep_pump(800).await;
                gm_assisted = true;
            }
        }
        "QuestActObjItemUse" | "QuestActObjItemGroupUse" => {
            let item = if obj.item != 0 {
                obj.item
            } else {
                *obj.items
                    .first()
                    .ok_or_else(|| "item group has no members".to_string())?
            };
            let stack = ensure_item(session, item, 1, anchor)
                .await
                .map_err(|e| format!("item {item}: {e}"))?;
            let use_skill = catalog.items.get(&item).map_or(0, |i| i.use_skill);
            if use_skill == 0 {
                return Err(format!("item {item} has no use skill"));
            }
            if obj.highlight_doodad != 0 {
                let d = goto_doodad(session, catalog, obj.highlight_doodad, anchor, deadline)
                    .await
                    .map_err(|e| format!("use-item doodad {}: {e}", obj.highlight_doodad))?;
                session
                    .game
                    .start_item_skill_on_doodad(use_skill, stack, item, d.obj_id)
                    .await
                    .map_err(|e| format!("using item {item}: {e:#}"))?;
            } else {
                let target = match catalog.skill_targets.get(&use_skill) {
                    Some(t) if t.npc != 0 => goto_npc(session, catalog, t.npc, anchor, deadline)
                        .await
                        .map_err(|e| format!("use-item npc {}: {e}", t.npc))?,
                    Some(t) if t.npc_group != 0 => {
                        let members = catalog
                            .skill_npc_groups
                            .get(&t.npc_group)
                            .cloned()
                            .unwrap_or_default();
                        let npc =
                            best_member(session, catalog, &members, anchor).ok_or_else(|| {
                                format!("skill group {} has no spawned member", t.npc_group)
                            })?;
                        goto_npc(session, catalog, npc, anchor, deadline)
                            .await
                            .map_err(|e| format!("use-item npc {npc}: {e}"))?
                    }
                    _ => session.game.state.self_obj,
                };
                if target != session.game.state.self_obj
                    && catalog
                        .skill_targets
                        .get(&use_skill)
                        .is_some_and(|t| t.hp_max > 0)
                {
                    // Setup: the item only works on an injured target (< hp_max percent).
                    let _ = session.game.change_target(target).await;
                    let _ = gm_command(&mut session.game, "damage 60 %").await;
                    let _ = session.pump_fast(600).await;
                }
                session
                    .game
                    .start_item_skill(use_skill, stack, item, target)
                    .await
                    .map_err(|e| format!("using item {item}: {e:#}"))?;
            }
            let _ = wait_skill(session, use_skill, CAST_ANSWER).await;
            // Item skills often have a cast time; let the fired packet arrive before the next
            // attempt, or the retry replaces the pending effect.
            let _ = wait_skill_fired(session, use_skill, Duration::from_secs(6)).await;
            let _ = session.sleep_pump(600).await;
        }
        "QuestActObjInteraction" => {
            interact_doodad(session, catalog, obj.doodad, anchor, deadline)
                .await
                .map_err(|e| format!("doodad {}: {e}", obj.doodad))?;
        }
        "QuestActObjSphere" => {
            go_sphere(session, obj.sphere).await?;
        }
        "QuestActObjLevel" => {
            let level = obj.level.max(2);
            if session.level < level {
                gm_command(&mut session.game, &format!("level {level}"))
                    .await
                    .map_err(|e| format!("{e:#}"))?;
            }
            // The level (and the setup level before every quest) always comes from `/level`,
            // so a level objective is never exercised by gameplay.
            gm_assisted = true;
            let _ = session.sleep_pump(1000).await;
        }
        "QuestActObjCinema" => {
            let _ = session.game.send_cs(cs::CSStartedCinemaPacket, &[]).await;
            let _ = session.game.send_cs(cs::CSCompletedCinemaPacket, &[]).await;
            let _ = session.sleep_pump(800).await;
        }
        other => return Err(format!("unsupported objective act {other}")),
    }
    Ok(gm_assisted)
}

/// One kill attempt: find the target in sight, else teleport to its authored spawn, else GM
/// spawn one, then fight until it dies (or the step deadline).
async fn hunt_once(
    session: &mut Session,
    catalog: &Catalog,
    wanted: &[u32],
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<(), String> {
    let mut target = find_npc(session, wanted);
    if target.is_none() {
        let spawn = wanted
            .iter()
            .filter_map(|t| catalog.nearest_npc(*t, session.game.state.self_pos))
            .min_by(|a, b| {
                dist2((a[0], a[1], a[2]), anchor).total_cmp(&dist2((b[0], b[1], b[2]), anchor))
            });
        let pos = spawn.unwrap_or([anchor.0, anchor.1, anchor.2]);
        teleport(session, pos)
            .await
            .map_err(|e| format!("teleport: {e}"))?;
        let _ = session.sleep_pump(1500).await;
        target = find_npc(session, wanted);
    }
    if target.is_none() {
        // Setup: the level data has no live spawn near us.
        let template = wanted.first().copied().unwrap_or(0);
        if template == 0 {
            return Err("no target npc".into());
        }
        gm_command(&mut session.game, &format!("spawn npc {template}"))
            .await
            .map_err(|e| format!("gm spawn: {e:#}"))?;
        let _ = session.sleep_pump(1500).await;
        target = find_npc(session, wanted);
        if let Some(t) = &target {
            session.spawned_npcs.push(t.obj_id);
        }
    }
    let mut target = target.ok_or_else(|| format!("no live npc of {wanted:?} in sight"))?;
    // Teleport next to the live target: a movement packet can be refused (collision / closed
    // zone), which would leave the server-side position out of the auto attack range.
    let near = [target.pos.0 + 1.3, target.pos.1, target.pos.2];
    teleport(session, near)
        .await
        .map_err(|e| format!("teleport to target: {e}"))?;
    session
        .game
        .move_to_facing((near[0], near[1], near[2]), target.pos, 400)
        .await
        .map_err(|e| format!("face: {e:#}"))?;
    let _ = session.pump_fast(400).await;
    let until = deadline.min(Instant::now() + Duration::from_secs(60));
    // The auto attack is a scheduled skill on the server: cast it once and let it tick. Casting
    // it again before the attack delay replaces the scheduled hit and deals no damage.
    let mut auto_started = false;
    let mut last_cast = Instant::now();
    let mut last_hp = target_hp(session, target.obj_id);
    while Instant::now() < until {
        if session.game.state.dead_units.contains(&target.obj_id)
            || !session.game.state.units.contains_key(&target.obj_id)
        {
            return Ok(());
        }
        if session
            .game
            .state
            .dead_units
            .contains(&session.game.state.self_obj)
        {
            let _ = gm_command(&mut session.game, "revive").await;
            let _ = session.sleep_pump(500).await;
            auto_started = false;
        }
        if let Some(u) = session.game.state.units.get(&target.obj_id) {
            target = u.clone();
        } else {
            return Ok(());
        }
        let pos = target.pos;
        if dist2(session.game.state.self_pos, pos).sqrt() > 3.5 {
            teleport(session, [pos.0 + 1.3, pos.1, pos.2])
                .await
                .map_err(|e| format!("teleport: {e}"))?;
            session
                .game
                .move_to_facing((pos.0 + 1.3, pos.1, pos.2), pos, 400)
                .await
                .map_err(|e| format!("face: {e:#}"))?;
            let _ = session.pump_fast(400).await;
        }
        if !auto_started || last_cast.elapsed() > Duration::from_secs(8) {
            let _ = session.game.set_force_attack(true).await;
            session
                .game
                .change_target(target.obj_id)
                .await
                .map_err(|e| format!("target: {e:#}"))?;
            session
                .game
                .start_skill(2, target.obj_id)
                .await
                .map_err(|e| format!("skill 2: {e:#}"))?;
            let _ = wait_skill(session, 2, Duration::from_secs(3)).await;
            auto_started = true;
            last_cast = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(650)).await;
        let _ = session.pump_fast(650).await;
        let hp = target_hp(session, target.obj_id);
        if hp != last_hp {
            last_hp = hp;
            last_cast = Instant::now();
        }
    }
    if session.game.state.dead_units.contains(&target.obj_id) {
        return Ok(());
    }
    Err(format!("npc {} ({}) did not die", target.obj_id, target.id))
}

async fn interact_doodad(
    session: &mut Session,
    catalog: &Catalog,
    template: u32,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<(), String> {
    let d = goto_doodad(session, catalog, template, anchor, deadline).await?;
    let skill = session
        .game
        .state
        .doodads
        .get(&d.obj_id)
        .and_then(|cur| catalog.doodad_skills.group_skill.get(&cur.func_group_id))
        .copied()
        .or_else(|| catalog.doodad_skills.template_skill.get(&template).copied())
        .unwrap_or(0);
    if skill == 0 {
        return Err(format!(
            "doodad {template} offers no interaction skill in func group {}",
            d.func_group_id
        ));
    }
    session
        .game
        .start_doodad_skill(skill, d.obj_id)
        .await
        .map_err(|e| format!("cast {skill}: {e:#}"))?;
    let _ = wait_skill(session, skill, CAST_ANSWER).await;
    let _ = wait_skill_fired(session, skill, Duration::from_secs(8)).await;
    let _ = session.sleep_pump(1000).await;
    Ok(())
}

/// Bag stacks of `template` the client knows about: `(item id, count)`.
fn stacks_of(session: &Session, item: u32) -> Vec<(u64, i32)> {
    session
        .game
        .state
        .bag
        .iter()
        .filter(|(_, s)| s.template == item && s.slot_type == 2)
        .map(|(id, s)| (*id, s.count))
        .collect()
}

/// Ensures `count` units of `item` are in the bag, adding only the missing units, and returns
/// the id of a stack that holds them.
async fn ensure_item(
    session: &mut Session,
    item: u32,
    count: u32,
    _anchor: (f32, f32, f32),
) -> std::result::Result<u64, String> {
    let count = count.max(1) as i32;
    let mut stacks = stacks_of(session, item);
    let mut total: i32 = stacks.iter().map(|(_, c)| c).sum();
    if total >= count {
        return stacks
            .iter()
            .max_by_key(|(_, c)| *c)
            .map(|(id, _)| *id)
            .ok_or_else(|| format!("item {item} disappeared"));
    }
    while total < count {
        let add = (count - total).min(1000) as u32;
        let packets = gm_command(&mut session.game, &format!("item add self {item} {add}"))
            .await
            .map_err(|e| format!("{e:#}"))?;
        let _ = session.pump().await;
        let after = stacks_of(session, item);
        let now: i32 = after.iter().map(|(_, c)| c).sum();
        if now < total + add as i32 {
            // The GM reply may arrive split from the item task; accept its stack id as proof.
            if let Some(id) = stack_id(&packets, item) {
                return Ok(id);
            }
            return Err(format!(
                "item {item} was not added (have {total}, asked {add}, now {now})"
            ));
        }
        total = now;
        stacks = after;
    }
    stacks
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("item {item} was not added"))
}

/// Adds exactly `count` units of `item` (an acquisition event for item-gain starters) and
/// returns the id of the new or grown stack.
async fn gain_item(
    session: &mut Session,
    item: u32,
    count: u32,
) -> std::result::Result<u64, String> {
    let count = count.max(1);
    let before: i32 = stacks_of(session, item).iter().map(|(_, c)| c).sum();
    let packets = gm_command(&mut session.game, &format!("item add self {item} {count}"))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let _ = session.pump().await;
    let after = stacks_of(session, item);
    let gained: i32 = after.iter().map(|(_, c)| c).sum::<i32>() - before;
    if gained < count as i32 {
        if let Some(id) = stack_id(&packets, item) {
            return Ok(id);
        }
        return Err(format!("item {item} gain {gained} < {count} (bag full?)"));
    }
    after
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(id, _)| *id)
        .ok_or_else(|| format!("item {item} was not added"))
}

async fn goto_npc(
    session: &mut Session,
    catalog: &Catalog,
    template: u32,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<u32, String> {
    if let Some(npc) = find_npc(session, &[template]) {
        approach(session, npc.pos).await?;
        if let Some(npc) = find_npc(session, &[template]) {
            return Ok(npc.obj_id);
        }
    }
    let pos = catalog
        .nearest_npc(template, session.game.state.self_pos)
        .or_else(|| catalog.nearest_npc(template, anchor))
        .unwrap_or([anchor.0, anchor.1, anchor.2]);
    teleport(session, pos)
        .await
        .map_err(|e| format!("teleport: {e}"))?;
    let until = deadline.min(Instant::now() + Duration::from_secs(6));
    while Instant::now() < until {
        let _ = session.sleep_pump(500).await;
        if let Some(npc) = find_npc(session, &[template]) {
            approach(session, npc.pos).await?;
            if let Some(npc) = find_npc(session, &[template]) {
                return Ok(npc.obj_id);
            }
        }
    }
    // Setup: the level data did not place it (or it is dead / far away).
    gm_command(&mut session.game, &format!("spawn npc {template}"))
        .await
        .map_err(|e| format!("gm spawn: {e:#}"))?;
    let _ = session.sleep_pump(1500).await;
    if let Some(npc) = find_npc(session, &[template]) {
        session.spawned_npcs.push(npc.obj_id);
        approach(session, npc.pos).await?;
        return Ok(npc.obj_id);
    }
    Err("not spawned and GM /spawn npc did not produce it".into())
}

async fn goto_doodad(
    session: &mut Session,
    catalog: &Catalog,
    template: u32,
    anchor: (f32, f32, f32),
    deadline: Instant,
) -> std::result::Result<DoodadInfo, String> {
    if let Some(d) = find_doodad(session, template) {
        approach(session, d.pos).await?;
        if let Some(d) = find_doodad(session, template) {
            return Ok(d);
        }
    }
    let pos = catalog
        .nearest_doodad(template, session.game.state.self_pos)
        .or_else(|| catalog.nearest_doodad(template, anchor))
        .unwrap_or([anchor.0, anchor.1, anchor.2]);
    teleport(session, pos)
        .await
        .map_err(|e| format!("teleport: {e}"))?;
    let until = deadline.min(Instant::now() + Duration::from_secs(6));
    while Instant::now() < until {
        let _ = session.sleep_pump(500).await;
        if let Some(d) = find_doodad(session, template) {
            approach(session, d.pos).await?;
            if let Some(d) = find_doodad(session, template) {
                return Ok(d);
            }
        }
    }
    gm_command(&mut session.game, &format!("spawn doodad {template}"))
        .await
        .map_err(|e| format!("gm spawn: {e:#}"))?;
    let _ = session.sleep_pump(1500).await;
    if let Some(d) = find_doodad(session, template) {
        session.spawned_doodads.push(d.obj_id);
        approach(session, d.pos).await?;
        return Ok(d);
    }
    Err("not spawned and GM /spawn doodad did not produce it".into())
}

async fn approach(session: &mut Session, pos: (f32, f32, f32)) -> std::result::Result<(), String> {
    let near = (pos.0 + 1.2, pos.1, pos.2);
    session
        .game
        .move_to_facing(near, pos, 1500)
        .await
        .map_err(|e| format!("move: {e:#}"))?;
    let _ = session.sleep_pump(300).await;
    Ok(())
}

async fn teleport(session: &mut Session, pos: [f32; 3]) -> std::result::Result<(), String> {
    session
        .game
        .chat(0, &format!("/move {} {} {}", pos[0], pos[1], pos[2]))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let p = session
        .game
        .wait_for("SCTeleportUnitPacket", |p| {
            p.is_sc(sc::SCTeleportUnitPacket)
        })
        .await
        .map_err(|e| format!("{e:#}"))?;
    let _ = p;
    let at = session
        .game
        .state
        .last_teleport
        .unwrap_or((pos[0], pos[1], pos[2]));
    session
        .game
        .teleport_ended(at)
        .await
        .map_err(|e| format!("{e:#}"))?;
    session
        .game
        .move_to(at, 5000)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let _ = session.pump().await;
    Ok(())
}

/// Walk into a quest sphere: the server reports the triggered sphere in `/sphere list 0`.
async fn go_sphere(session: &mut Session, sphere_id: u32) -> std::result::Result<(), String> {
    let reply = gm_command(&mut session.game, "sphere list 0")
        .await
        .map_err(|e| format!("{e:#}"))?;
    let mut spheres = Vec::new();
    for text in reply.iter().filter_map(chat_text) {
        if let Some(pos) = parse_sphere_line(&text) {
            spheres.push(pos);
        }
    }
    let me = session.game.state.self_pos;
    let Some(pos) = spheres
        .into_iter()
        .min_by(|a, b| dist2(*a, me).total_cmp(&dist2(*b, me)))
    else {
        return Err(format!(
            "no active sphere trigger (sphere {sphere_id}); the quest is not at its sphere step"
        ));
    };
    teleport(session, [pos.0, pos.1, pos.2])
        .await
        .map_err(|e| format!("teleport: {e}"))?;
    let _ = session.sleep_pump(2000).await;
    Ok(())
}

fn parse_sphere_line(line: &str) -> Option<(f32, f32, f32)> {
    let start = line.find("xyz:<")? + 5;
    let end = line[start..].find('>')? + start;
    let parts: Vec<&str> = line[start..end].split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].trim().parse().ok()?,
        parts[1].trim().parse().ok()?,
        parts[2].trim().parse().ok()?,
    ))
}

fn find_npc(session: &Session, templates: &[u32]) -> Option<UnitInfo> {
    let me = session.game.state.self_pos;
    session
        .game
        .state
        .units
        .values()
        .filter(|u| {
            u.unit_type == 1
                && templates.contains(&u.id)
                && !session.game.state.dead_units.contains(&u.obj_id)
        })
        .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
        .cloned()
}

fn find_doodad(session: &Session, template: u32) -> Option<DoodadInfo> {
    let me = session.game.state.self_pos;
    session
        .game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == template)
        .min_by(|a, b| dist2(a.pos, me).total_cmp(&dist2(b.pos, me)))
        .cloned()
}

fn best_member(
    session: &Session,
    catalog: &Catalog,
    members: &[u32],
    anchor: (f32, f32, f32),
) -> Option<u32> {
    members
        .iter()
        .copied()
        .filter(|m| {
            catalog
                .nearest_npc(*m, session.game.state.self_pos)
                .is_some()
                || catalog.nearest_npc(*m, anchor).is_some()
        })
        .min_by(|a, b| {
            let da = catalog
                .nearest_npc(*a, session.game.state.self_pos)
                .map_or(f32::MAX, |p| {
                    dist2((p[0], p[1], p[2]), session.game.state.self_pos)
                });
            let db = catalog
                .nearest_npc(*b, session.game.state.self_pos)
                .map_or(f32::MAX, |p| {
                    dist2((p[0], p[1], p[2]), session.game.state.self_pos)
                });
            da.total_cmp(&db).then(a.cmp(b))
        })
}

fn is_attemptable(
    cfg: &CrawlConfig,
    catalog: &Catalog,
    quest: &Quest,
    completed: &HashSet<u32>,
) -> bool {
    quest.unsupported.is_none()
        && !quest.instance
        && anchor_of(catalog, quest).is_some()
        && matches!(
            catalog.gate_verdict(
                quest,
                cfg.race,
                quest.level.max(cfg.min_level).max(2),
                completed
            ),
            GateVerdict::Play { .. }
        )
}

fn anchor_of(catalog: &Catalog, quest: &Quest) -> Option<(f32, f32, f32)> {
    let me = (0.0, 0.0, 0.0);
    let starter = match quest.starter.kind.as_str() {
        "npc" => catalog.nearest_npc(quest.starter.npc, me).map(to_pos),
        "doodad" => catalog.nearest_doodad(quest.starter.doodad, me).map(to_pos),
        _ => None,
    };
    starter
        .or_else(|| match quest.turnin.kind.as_str() {
            "npc" => catalog.nearest_npc(quest.turnin.npc, me).map(to_pos),
            "doodad" => catalog.nearest_doodad(quest.turnin.doodad, me).map(to_pos),
            _ => None,
        })
        .or_else(|| {
            quest.objectives.iter().find_map(|o| {
                if o.npc != 0 {
                    catalog.nearest_npc(o.npc, me).map(to_pos)
                } else if o.doodad != 0 {
                    catalog.nearest_doodad(o.doodad, me).map(to_pos)
                } else if !o.npcs.is_empty() {
                    o.npcs
                        .iter()
                        .find_map(|n| catalog.nearest_npc(*n, me))
                        .map(to_pos)
                } else {
                    None
                }
            })
        })
}

fn to_pos(p: [f32; 3]) -> (f32, f32, f32) {
    (p[0], p[1], p[2])
}

fn target_hp(session: &Session, obj_id: u32) -> Option<i32> {
    session.game.state.unit_points.get(&obj_id).map(|p| p.0)
}

fn dist2(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    dx * dx + dy * dy + dz * dz
}

async fn send_start_quest(
    game: &mut GameClient,
    quest: u32,
    npc: u32,
    doodad: u32,
    sphere: u32,
) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    s.write_u32(quest)
        .write_bc(npc)
        .write_bc(doodad)
        .write_u32(sphere);
    game.send_cs(cs::CSStartQuestContextPacket, s.as_bytes())
        .await
}

async fn send_complete_quest(
    game: &mut GameClient,
    quest: u32,
    npc: u32,
    doodad: u32,
    selected: i32,
) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    s.write_u32(quest)
        .write_bc(npc)
        .write_bc(doodad)
        .write_i32(selected);
    game.send_cs(cs::CSCompleteQuestContextPacket, s.as_bytes())
        .await
}

async fn send_talk_made(
    game: &mut GameClient,
    npc: u32,
    quest: u32,
    component: u32,
    act: u32,
) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    s.write_bc(npc)
        .write_u32(quest)
        .write_u32(component)
        .write_u32(act);
    game.send_cs(cs::CSQuestTalkMadePacket, s.as_bytes()).await
}

async fn drop_quest(game: &mut GameClient, quest: u32) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    s.write_u32(quest);
    game.send_cs(cs::CSDropQuestContextPacket, s.as_bytes())
        .await
}

/// Sends a GM chat command and returns the packets that answer it.
pub async fn gm_command(game: &mut GameClient, text: &str) -> anyhow::Result<Vec<Packet>> {
    game.chat(0, &format!("/{text}")).await?;
    game.drain(Duration::from_millis(800), Duration::from_secs(4))
        .await
}

pub fn chat_text(p: &Packet) -> Option<String> {
    if !p.is_sc(sc::SCChatMessagePacket) {
        return None;
    }
    let mut s = p.stream();
    s.read_i16().ok()?;
    s.read_i16().ok()?;
    s.read_u32().ok()?;
    s.read_bc().ok()?;
    s.read_u32().ok()?;
    s.read_u8().ok()?;
    s.read_u8().ok()?;
    s.read_u32().ok()?;
    s.read_string().ok()?;
    s.read_string().ok()
}

/// Waits for the answer to one of our skill casts.
async fn wait_skill(session: &mut Session, skill_id: u32, wait: Duration) -> bool {
    let caster = session.game.state.self_obj;
    let saved = session.game.timeout;
    session.game.timeout = wait;
    let answered = session
        .game
        .wait_for("skill response", |p| {
            own_skill_response(p, skill_id, caster) || p.is_sc(sc::SCErrorMsgPacket)
        })
        .await;
    session.game.timeout = saved;
    match answered {
        Ok(p) => {
            session.tracker.observe(std::slice::from_ref(&p));
            if p.is_sc(sc::SCErrorMsgPacket) {
                false
            } else {
                skill_response_result(&p).is_ok_and(|r| r == 0)
            }
        }
        Err(_) => false,
    }
}

async fn wait_skill_fired(session: &mut Session, skill_id: u32, wait: Duration) -> bool {
    let caster = session.game.state.self_obj;
    let saved = session.game.timeout;
    session.game.timeout = wait;
    let fired = session
        .game
        .wait_for("skill fired", |p| {
            (p.is_sc(sc::SCSkillFiredPacket) && own_skill_response(p, skill_id, caster))
                || p.is_sc(sc::SCErrorMsgPacket)
        })
        .await
        .is_ok_and(|p| !p.is_sc(sc::SCErrorMsgPacket));
    session.game.timeout = saved;
    fired
}

fn skill_response_result(packet: &Packet) -> anyhow::Result<u8> {
    if packet.is_sc(sc::SCSkillFiredPacket) {
        return Ok(0);
    }
    ensure!(
        packet.is_sc(sc::SCSkillStartedPacket),
        "server reported an error instead of a skill response"
    );
    let mut stream = packet.stream();
    stream.read_u32()?;
    stream.read_u16()?;
    let caster_kind = stream.read_u8()?;
    SkillCaster::read(caster_kind, &mut stream)?;
    let target_kind = stream.read_u8()?;
    SkillCastTarget::read(target_kind, &mut stream)?;
    let object_kind = stream.read_u8()?;
    SkillObject::read(object_kind, &mut stream)?;
    stream.read_u16()?;
    stream.read_u16()?;
    stream.read_u8()?;
    let flags = stream.read_u8()?;
    Ok(if flags & 1 != 0 { stream.read_u8()? } else { 0 })
}

fn own_skill_response(packet: &Packet, skill_id: u32, caster: u32) -> bool {
    if !packet.is_sc(sc::SCSkillStartedPacket) && !packet.is_sc(sc::SCSkillFiredPacket) {
        return false;
    }
    let mut stream = packet.stream();
    stream.read_u32().ok() == Some(skill_id)
        && stream.read_u16().is_ok()
        && stream.read_u8().is_ok()
        && stream.read_bc().ok() == Some(caster)
}

fn last_events(game: &GameClient) -> String {
    let mut out: Vec<String> = game
        .state
        .events
        .iter()
        .rev()
        .take(4)
        .map(|e| match e {
            crate::game::GameEvent::ErrorMsg(code) => format!("SCErrorMsg {code}"),
            crate::game::GameEvent::ItemTask(t) => {
                format!("item task {}", t.action)
            }
        })
        .collect();
    out.reverse();
    out.join(", ")
}

/// Size of the item stack a GM `/item add` reply created or grew.
fn stack_id(packets: &[Packet], template_id: u32) -> Option<u64> {
    let mut found = None;
    for p in packets
        .iter()
        .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
    {
        let mut s = p.stream();
        let parsed = (|| -> anyhow::Result<()> {
            s.read_u8()?;
            let count = s.read_u8()?;
            for _ in 0..count {
                match s.read_u8()? {
                    5 | 6 | 15 => {
                        s.read_u8()?;
                        s.read_u8()?;
                        let template = s.read_u32()?;
                        let id = s.read_u64()?;
                        s.read_u8()?;
                        s.read_u8()?;
                        s.read_i32()?;
                        let details = s.read_i16()?;
                        if details > 0 {
                            s.read_bytes(details as usize)?;
                        }
                        s.read_i64()?;
                        s.read_i32()?;
                        s.read_u32()?;
                        s.read_u8()?;
                        s.read_i64()?;
                        s.read_i64()?;
                        if template == template_id {
                            found = Some(id);
                        }
                    }
                    4 => {
                        s.read_u8()?;
                        s.read_u8()?;
                        let id = s.read_u64()?;
                        s.read_i32()?;
                        if s.read_u32()? == template_id {
                            found = Some(id);
                        }
                    }
                    other => bail!("item task action {other}"),
                }
            }
            Ok(())
        })();
        if parsed.is_err() {
            break;
        }
    }
    found
}

// ----- evidence windows ------------------------------------------------------

struct LogWindow {
    path: Option<PathBuf>,
    offset: u64,
}

impl LogWindow {
    fn new(path: Option<&Path>) -> Self {
        let offset = path
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        Self {
            path: path.map(Path::to_path_buf),
            offset,
        }
    }

    fn warnings(&self) -> Vec<String> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        use std::io::{Read, Seek, SeekFrom};
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            return Vec::new();
        }
        let mut out: Vec<String> = text
            .lines()
            .filter(|l| l.contains(" WARN ") || l.contains(" ERROR "))
            .filter(|l| !noise_line(l))
            .map(|l| l.chars().take(300).collect())
            .take(10)
            .collect();
        if text.lines().filter(|l| l.contains(" WARN ")).count() > out.len() {
            out.push("...".into());
        }
        out
    }
}

struct ReportWindow {
    dir: Option<PathBuf>,
    before: HashSet<PathBuf>,
}

impl ReportWindow {
    fn new(dir: Option<&Path>) -> Self {
        let dir = dir.map(Path::to_path_buf);
        let before = dir.as_deref().map(list_reports).unwrap_or_default();
        Self { dir, before }
    }

    fn new_reports(&self) -> Vec<String> {
        let Some(dir) = &self.dir else {
            return Vec::new();
        };
        list_reports(dir)
            .into_iter()
            .filter(|p| !self.before.contains(p))
            .filter(|p| p.file_name().is_some_and(|n| n != "summary.json"))
            .filter(|p| !report_is_noise(p))
            .map(|p| {
                p.strip_prefix(dir)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned()
            })
            .take(10)
            .collect()
    }
}

fn noise_line(line: &str) -> bool {
    line.contains("TeleportEnded,") || line.contains("entered closed zone")
}

/// The crawler teleports constantly; those WARNs are not quest evidence.
fn report_is_noise(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let message = v
        .get("trigger")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    message.starts_with("TeleportEnded,") || message.contains("entered closed zone")
}

fn list_reports(dir: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                out.insert(path);
            }
        }
    }
    out
}

// ----- GM access -------------------------------------------------------------

/// The game server config as the server itself loads it: `Config.json`, then
/// `Configurations/**/*.json` (sorted), then `Config.Local.json` in `path`'s directory, with an
/// explicitly named file merged last; `Section__Key` environment variables win over all files.
fn load_game_config(
    path: &Path,
    env: impl IntoIterator<Item = (String, String)>,
) -> anyhow::Result<serde_json::Value> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut files = aaemu_game::config::config_files(dir);
    if !files.iter().any(|file| file == path) {
        files.push(path.to_path_buf());
    }
    let mut merged = serde_json::Value::Object(Default::default());
    for file in files {
        if !file.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let text = aaemu_game::config::relax_json(text.trim_start_matches('\u{feff}'));
        aaemu_game::config::merge(&mut merged, serde_json::from_str(&text)?);
    }
    aaemu_game::config::apply_env(&mut merged, env);
    Ok(merged)
}

/// `(host, port, user, password, database)` of `Connections.MySQLProvider`. The port never
/// falls back to 3306: a missing port would point the crawler at the developer's own MySQL.
fn mysql_provider(
    value: &serde_json::Value,
    port_override: Option<u16>,
) -> anyhow::Result<(String, u16, String, String, String)> {
    let mysql = get_ci(value, "Connections")
        .and_then(|c| get_ci(c, "MySQLProvider"))
        .context("Connections.MySQLProvider missing")?;
    let host = get_ci(mysql, "Host")
        .and_then(|v| v.as_str())
        .unwrap_or("127.0.0.1")
        .to_string();
    let port: u16 = match port_override {
        Some(port) => port,
        None => {
            let text = get_ci(mysql, "Port")
                .map(scalar_string)
                .filter(|p| !p.is_empty())
                .context(
                    "MySQLProvider.Port missing; refusing to default to 3306 (pass --mysql-port)",
                )?;
            text.parse()
                .with_context(|| format!("invalid MySQLProvider.Port {text:?}"))?
        }
    };
    anyhow::ensure!(
        port != 3306,
        "refusing MySQL port 3306 (the developer's own server); the crawler may only use the \
         lane's private MySQL (reached through the game config or --mysql-port)"
    );
    let user = get_ci(mysql, "User")
        .and_then(|v| v.as_str())
        .context("MySQLProvider.User missing")?
        .to_string();
    let password = get_ci(mysql, "Password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let database = get_ci(mysql, "Database")
        .and_then(|v| v.as_str())
        .context("MySQLProvider.Database missing")?
        .to_string();
    Ok((host, port, user, password, database))
}

/// Grants the crawler account access level 100 in the lane's game database, using the
/// `Connections.MySQLProvider` of the game server config. Setup only; never touches MySQL 3306.
fn grant_gm(cfg: &CrawlConfig, account_id: u32) -> anyhow::Result<()> {
    let config_path = cfg
        .game_config
        .as_ref()
        .context("no --game-config (game server Config.Local.json)")?;
    let value = load_game_config(config_path, std::env::vars())?;
    let (host, port, user, password, database) = mysql_provider(&value, cfg.mysql_port)?;
    let mysql_bin = mysql_binary(cfg)?;
    let sql = format!(
        "INSERT INTO accounts (account_id, access_level) VALUES ({account_id}, 100) \
         ON DUPLICATE KEY UPDATE access_level = 100; \
         UPDATE characters SET access_level = 100 WHERE account_id = {account_id};"
    );
    let output = std::process::Command::new(&mysql_bin)
        .arg("--protocol=TCP")
        .arg(format!("-h{host}"))
        .arg(format!("-P{port}"))
        .arg(format!("-u{user}"))
        .arg(&database)
        .arg("-e")
        .arg(&sql)
        .env("MYSQL_PWD", password)
        .output()
        .with_context(|| format!("running {}", mysql_bin.display()))?;
    if !output.status.success() {
        bail!(
            "mysql failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn mysql_binary(cfg: &CrawlConfig) -> anyhow::Result<PathBuf> {
    let candidate = cfg
        .mysql_bin
        .clone()
        .or_else(|| std::env::var_os("AAEMU_MYSQL_BIN").map(PathBuf::from));
    if let Some(p) = candidate {
        if p.is_dir() {
            let exe = p.join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
            return Ok(exe);
        }
        return Ok(p);
    }
    Ok(PathBuf::from("mysql"))
}

fn get_ci<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let obj = value.as_object()?;
    obj.get(key).or_else(|| {
        obj.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

fn scalar_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "aaemu-testclient-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn stack(template: u32, slot: u8, count: i32) -> BagStack {
        BagStack {
            template,
            slot_type: 2,
            slot,
            count,
        }
    }

    fn added(owed: i32, whole: bool) -> AddedStack {
        AddedStack { owed, whole }
    }

    #[test]
    fn two_quests_needing_the_same_item_each_get_a_fresh_stack() {
        let mut bag = HashMap::new();
        let mut ledger = HashMap::new();
        bag.insert(1, stack(500, 3, 2));
        ledger.insert(1, added(2, true));
        let doomed = items_to_destroy(&ledger, &bag);
        assert_eq!(doomed, [(1, 2, 3, 2)]);
        // Applying the cleanup empties the bag again, so the next quest's gain is a new stack.
        for (id, _, _, _) in &doomed {
            bag.remove(id);
            ledger.remove(id);
        }
        assert!(bag.is_empty());
        bag.insert(2, stack(500, 4, 2));
        ledger.insert(2, added(2, true));
        assert_eq!(items_to_destroy(&ledger, &bag), [(2, 2, 4, 2)]);
    }

    #[test]
    fn cleanup_removes_only_the_units_the_crawler_added() {
        let mut bag = HashMap::new();
        let mut ledger = HashMap::new();
        // A pre-existing stack of 3 grew by 2 (GM add): only the 2 are the crawler's.
        bag.insert(9, stack(417, 3, 5));
        ledger.insert(9, added(2, false));
        // A quest reward stack is fully the crawler's.
        bag.insert(10, stack(500, 4, 1));
        ledger.insert(10, added(1, true));
        // A zero responsibility (a consumed unit) is never destroyed.
        bag.insert(11, stack(600, 5, 1));
        ledger.insert(11, added(0, true));
        // An equipment slot is never destroyed.
        bag.insert(
            12,
            BagStack {
                slot_type: 1,
                ..stack(700, 0, 1)
            },
        );
        ledger.insert(12, added(1, true));
        assert_eq!(
            items_to_destroy(&ledger, &bag),
            [(9, 2, 3, 2), (10, 2, 4, 1)]
        );
    }

    #[test]
    fn cleanup_never_destroys_an_untracked_stack() {
        let bag = HashMap::from([(1, stack(500, 2, 1))]);
        assert!(items_to_destroy(&HashMap::new(), &bag).is_empty());
    }

    #[test]
    fn reconnect_forgets_merged_stacks_but_keeps_whole_ones() {
        let mut ledger = HashMap::from([
            (1, added(2, true)),
            (2, added(3, false)),
            (3, added(0, false)),
        ]);
        assert_eq!(forget_merged_after_reconnect(&mut ledger), [(2, 3)]);
        assert_eq!(ledger.get(&1), Some(&added(2, true)));
        assert!(!ledger.contains_key(&2));
        assert!(!ledger.contains_key(&3));
    }

    fn objective(class: &str, index: u8, required: i32) -> Objective {
        Objective {
            class: class.into(),
            component: 0,
            act_id: 0,
            objective_index: index,
            required,
            npc: 0,
            npcs: Vec::new(),
            doodad: 0,
            highlight_doodad: 0,
            item: 0,
            items: Vec::new(),
            sphere: 0,
            level: 0,
        }
    }

    fn quest(objectives: Vec<Objective>) -> Quest {
        use crate::crawler::catalog::{Rewards, Starter, Turnin};
        Quest {
            id: 1,
            name: "quest".into(),
            level: 1,
            zone_id: 0,
            zone: String::new(),
            category_id: 0,
            detail_id: 0,
            repeatable: false,
            let_it_done: false,
            score: 0,
            start_component: 0,
            starter: Starter {
                kind: "auto".into(),
                npc: 0,
                doodad: 0,
                item: 0,
                count: 0,
                sphere: 0,
                act: String::new(),
            },
            objectives,
            turnin: Turnin {
                kind: "auto".into(),
                npc: 0,
                doodad: 0,
                act: String::new(),
            },
            rewards: Rewards::default(),
            gates: Vec::new(),
            gate_items: Vec::new(),
            unsupported: None,
            instance: false,
        }
    }

    #[test]
    fn setup_satisfied_objectives_are_gm_assisted_at_accept() {
        let mut level = objective("QuestActObjLevel", 0, 1);
        level.level = 10;
        let mut gather = objective("QuestActObjItemGather", 1, 1);
        gather.item = 33381;
        let mut group = objective("QuestActObjItemGroupGather", 2, 1);
        group.items = vec![7, 8];
        let q = quest(vec![
            level,
            gather,
            group,
            objective("QuestActObjTalk", 3, 1),
        ]);
        // Complete at accept: the setup level and gate item made them true; talk did not.
        let state = QuestState {
            status: 1,
            objectives: [1, 1, 1, 0, 0],
        };
        assert_eq!(
            setup_assisted_steps(Some(state), &q, 10, &[33381, 8]),
            [
                "objective 1 QuestActObjLevel",
                "objective 2 QuestActObjItemGather",
                "objective 3 QuestActObjItemGroupGather"
            ]
        );
        // Not complete at accept: no flag even though setup could satisfy them.
        let state = QuestState {
            status: 1,
            objectives: [0, 0, 0, 0, 0],
        };
        assert!(setup_assisted_steps(Some(state), &q, 10, &[33381, 8]).is_empty());
        // Complete, but the setup neither raised the level nor put the items in the bag.
        let state = QuestState {
            status: 1,
            objectives: [1, 1, 1, 0, 0],
        };
        assert!(setup_assisted_steps(Some(state), &q, 2, &[]).is_empty());
    }

    fn outcome(failure: Option<QuestFailure>) -> QuestOutcome {
        QuestOutcome {
            outcome: failure.map_or(Ok(()), Err),
            warnings: Vec::new(),
            reports: Vec::new(),
            objectives: Vec::new(),
            gm_assisted_steps: Vec::new(),
            duration_ms: 0,
        }
    }

    #[test]
    fn daily_limit_is_recorded_as_retryable() {
        let cfg = CrawlConfig::defaults();
        let q = quest(Vec::new());
        let r = finish_result(
            &cfg,
            &q,
            outcome(Some(QuestFailure::new("accept", "daily limit (error 621)"))),
            1,
        );
        assert_eq!(r.status, "daily-limit");
    }

    #[test]
    fn setup_flag_reaches_the_finished_result() {
        let cfg = CrawlConfig::defaults();
        let q = quest(Vec::new());
        let mut out = outcome(None);
        out.gm_assisted_steps = vec!["objective 1 QuestActObjItemGather".into()];
        let r = finish_result(&cfg, &q, out, 1);
        assert_eq!(r.status, "passed");
        assert!(r.gm_assisted);
        assert_eq!(r.gm_assisted_steps.len(), 1);
    }

    fn completed_packet(quest: u32) -> Packet {
        let mut body = PacketStream::new();
        body.write_u32(quest).write_raw(&[0; 8]).write_u32(0);
        Packet {
            level: crate::game::LEVEL_GAME,
            opcode: sc::SCQuestContextCompletedPacket,
            body: body.into_bytes(),
        }
    }

    #[test]
    fn final_turn_in_check_accepts_a_completed_status() {
        // The wait loop may not run once the deadline has passed: the final check still sees a
        // completed tracker from the sent packet.
        assert_eq!(turn_in_state(7, 5, &[]), TurnInState::Completed);
        assert_eq!(turn_in_state(7, 0, &[]), TurnInState::Pending);
        assert_eq!(turn_in_state(7, 2, &[]), TurnInState::Failed);
        assert_eq!(
            turn_in_state(7, 0, &[completed_packet(7)]),
            TurnInState::Completed
        );
        // Another quest's completion packet does not count.
        assert_eq!(
            turn_in_state(7, 0, &[completed_packet(8)]),
            TurnInState::Pending
        );
    }

    #[test]
    fn completed_packet_is_matched_by_quest_id() {
        let packet = completed_packet(4242);
        assert_eq!(completed_quest_id(&packet), Some(4242));
        assert_eq!(
            completed_quest_id(&Packet {
                body: Vec::new(),
                ..packet
            }),
            None
        );
    }

    #[test]
    fn game_config_merges_local_over_base_and_env() {
        let dir = temp_dir("game-config");
        // Comments and a trailing comma are valid for the game's relaxed reader.
        std::fs::write(
            dir.join("Config.json"),
            r#"{
                // game-style comment
                "Connections": {"MySQLProvider": {
                    "Host": "10.0.0.2", "Port": "%db_port%", "User": "u",
                    "Password": "p", "Database": "base",
                }},
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("Configurations")).unwrap();
        std::fs::write(
            dir.join("Configurations").join("extra.json"),
            r#"{"Connections":{"MySQLProvider":{"Port":"1111"}}}"#,
        )
        .unwrap();
        let local = dir.join("Config.Local.json");
        std::fs::write(
            &local,
            r#"{"Connections":{"MySQLProvider":{"Port":"3317","Database":"lane"}}}"#,
        )
        .unwrap();

        let value = load_game_config(&local, Vec::new()).unwrap();
        // Local overrides the Configurations file, which overrides Config.json.
        assert_eq!(
            mysql_provider(&value, None).unwrap(),
            (
                "10.0.0.2".into(),
                3317,
                "u".into(),
                "p".into(),
                "lane".into()
            )
        );
        // The environment overlays the files, as it does for the game server.
        let value = load_game_config(
            &local,
            vec![("Connections__MySQLProvider__Port".into(), "4406".into())],
        )
        .unwrap();
        assert_eq!(mysql_provider(&value, None).unwrap().1, 4406);
        // The command line override wins.
        assert_eq!(mysql_provider(&value, Some(5506)).unwrap().1, 5506);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_mysql_port_is_refused_instead_of_3306() {
        let value = serde_json::json!({
            "Connections": {"MySQLProvider": {"User": "u", "Database": "d"}}
        });
        let err = mysql_provider(&value, None).unwrap_err().to_string();
        assert!(err.contains("refusing to default to 3306"), "{err}");
        assert_eq!(mysql_provider(&value, Some(3317)).unwrap().1, 3317);
    }

    #[test]
    fn an_explicit_3306_is_refused() {
        let value = serde_json::json!({
            "Connections": {"MySQLProvider": {"Port": "3306", "User": "u", "Database": "d"}}
        });
        let err = mysql_provider(&value, None).unwrap_err().to_string();
        assert!(err.contains("refusing MySQL port 3306"), "{err}");
        let err = mysql_provider(&value, Some(3306)).unwrap_err().to_string();
        assert!(err.contains("refusing MySQL port 3306"), "{err}");
    }
}

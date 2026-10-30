//! `questchain`: accept quest 251 "Angry Boars", kill three Solzreed boars with the player's
//! melee skill, loot their meat, and turn the quest in with the real client packets (GM account
//! needed for the setup commands).
//!
//! Quest 251 (`QuestActConAcceptNpc` 3512, `QuestActObjItemGather` 3 x item 4058 "Solzreed Boar
//! Meat", `QuestActConReportNpc` 3512, reward item 18791). The bot spawns the giver 3512 and the
//! boars 3475 in front of itself, accepts with `CSStartQuestContext`, weakens each boar with the
//! GM `/damage 99%` setup command, finishes it with `CSStartSkill` 2, loots the corpse with
//! `CSLootOpenBag(lootAll)`, then reports with `CSCompleteQuestContext` and checks the reward.
//!
//! GM-spawned NPCs carry no owner, so bots running in parallel must not spawn near each other:
//! every bot owns one slot of a 4 x 4 grid (`--slot N`, 25 m apart, anchored at the origin where
//! its run began), teleports back to its slot before each `/spawn npc` (a boar chase drifts the
//! bot away) and only accepts an NPC announced within 1 m of the point its own command asked for.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use aaemu_game::systems::skills::types::skill_result;
use anyhow::{bail, ensure, Context};

use super::coreloop::{
    bot_slot, destroy_item, gm_access, item_stacks, item_stacks_any, slot_offset, spawn_point,
    teleport_to, ItemStack, SPAWN_TOLERANCE,
};
use super::questitem::{chat_text, gm_command, last_quest_state};
use super::{
    own_skill_response, packet_obj_id, quest_template_id, skill_response_result, BotConfig, Runner,
};
use crate::game::{GameClient, Packet, UnitInfo};

/// Quest 251 "Angry Boars".
const QUEST: u32 = 251;
/// Its giver and report NPC (chief Goth).
const GIVER: u32 = 3512;
/// Solzreed boar and the meat its loot pack always contains.
const BOAR: u32 = 3475;
const MEAT: u32 = 4058;
/// Objective count and the quest reward item ("heart beating").
const MEATS: i32 = 3;
const REWARD: u32 = 18791;
/// Melee attack skill of the created fighter.
const ATTACK: u32 = 2;

/// How often the player's hit was refused with a result that means the boar moved (out of range,
/// not in front, something between): the bot then walks up to it and aims again. The counts are
/// published as run variables (`retry_too_far`, `retry_out_of_angle`, `retry_obstacle`), which
/// the soak dashboard sums, so an intermittent server facing / line-of-sight bug shows up as a
/// number even when the retry hides it from the pass rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Retries {
    too_far: u32,
    out_of_angle: u32,
    obstacle: u32,
}

impl Retries {
    /// Counts a refused hit with `result`; false when `result` is not one the bot retries.
    fn note(&mut self, result: i32) -> bool {
        let counter = match u8::try_from(result) {
            Ok(skill_result::TOO_FAR_RANGE) => &mut self.too_far,
            Ok(skill_result::OUTOF_ANGLE) => &mut self.out_of_angle,
            Ok(skill_result::OBSTACLE) => &mut self.obstacle,
            _ => return false,
        };
        *counter += 1;
        true
    }

    fn publish(&self, r: &mut Runner<'_>) {
        r.var("retry_too_far", self.too_far);
        r.var("retry_out_of_angle", self.out_of_angle);
        r.var("retry_obstacle", self.obstacle);
    }
}

#[derive(Default)]
struct Cleanup {
    /// The scenario origin, where the bot stood before it moved to its own spot; it goes back
    /// there at the end.
    home: Option<(f32, f32, f32)>,
    giver: Option<u32>,
    boars: Vec<u32>,
    /// Loot that is still in the bag (what the quest turn-in consumed is taken out).
    loot: Vec<ItemStack>,
    reward: Option<ItemStack>,
}

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    gm_access(game, r).await?;
    let mut cleanup = Cleanup::default();
    let result = run_inner(game, bot_slot(cfg), r, &mut cleanup).await;
    let cleanup_result = cleanup_quest(game, r, &mut cleanup).await;
    let leave = super::coreloop::leave_world(game).await;
    result?;
    cleanup_result?;
    leave?;
    Ok(())
}

async fn run_inner(
    game: &mut GameClient,
    slot: u32,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    // A previous run (or another scenario) may have completed the quest: reset it first.
    let _ = gm_command(game, &format!("quest uncomplete {QUEST}")).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;

    // ---- the bot's own spot: parallel bots each own one slot of a grid 25 m apart, anchored at
    // the origin where this run began (GM-spawned NPCs carry no owner, so the bot takes the NPC
    // announced at the point its own command requested) ----
    let s = r.begin("game.questchain.position");
    let origin = game.state.self_pos;
    cleanup.home = Some(origin);
    let (dx, dy) = slot_offset(slot);
    let spot = (origin.0 + dx, origin.1 + dy, origin.2);
    r.var("slot", slot);
    let mut retries = Retries::default();
    retries.publish(r);
    let spawn_at = go_to_spot(game, spot).await?;
    r.end(
        s,
        format!(
            "slot {slot}: at {:?} (origin {origin:?} + {dx:.1}, {dy:.1}), NPCs spawn at {spawn_at:?}",
            game.state.self_pos
        ),
    );

    // ---- the giver in front of the bot ----
    let s = r.begin("game.questchain.giver");
    let giver = spawn_npc(game, GIVER, spawn_at).await?;
    let giver_obj = giver.obj_id;
    cleanup.giver = Some(giver_obj);
    move_near(game, giver.pos).await?;
    r.var("giver_obj", giver_obj);
    r.end(s, format!("NPC {GIVER} obj {giver_obj} at {:?}", giver.pos));

    // ---- accept ----
    let s = r.begin("game.questchain.accept");
    game.change_target(giver_obj).await?;
    game.start_quest(QUEST, giver_obj).await?;
    let answers = game
        .drain(Duration::from_millis(800), Duration::from_secs(6))
        .await?;
    let error = answers
        .iter()
        .find(|p| p.is_sc(sc::SCErrorMsgPacket))
        .and_then(|p| p.stream().read_i16().ok());
    ensure!(
        error.is_none(),
        "quest {QUEST} accept rejected with SCErrorMsg {error:?}"
    );
    let started = answers.iter().any(|p| {
        (p.is_sc(sc::SCQuestContextStartedPacket) || p.is_sc(sc::SCQuestContextUpdatedPacket))
            && quest_template_id(p).ok() == Some(QUEST)
    });
    ensure!(
        started,
        "no SCQuestContextStarted/Updated carried quest {QUEST}; answers: {}",
        packet_summary(&answers)
    );
    let mut quest = last_quest_state(&answers, QUEST);
    ensure!(
        quest.is_some(),
        "no quest state packet carried quest {QUEST}: {}",
        packet_summary(&answers)
    );
    ensure!(
        quest.is_some_and(|q| q.objectives[0] == 0),
        "quest {QUEST} starts with objectives {:?} (want 0); a previous run left progress: {}",
        quest.map(|q| q.objectives),
        packet_summary(&answers)
    );
    r.end(
        s,
        match quest {
            Some(q) => format!(
                "quest {QUEST} accepted, status {} objectives {:?}",
                q.status, q.objectives
            ),
            None => format!("quest {QUEST} accepted"),
        },
    );

    // ---- kill and loot boars until the objective is met ----
    let mut boar_no = 0;
    while quest.is_none_or(|q| q.objectives[0] < MEATS) {
        boar_no += 1;
        ensure!(
            boar_no <= MEATS + 2,
            "too many boars without finishing the objective"
        );
        let s = r.begin(&format!("game.questchain.boar{boar_no}"));
        // Back to the own spot first: the previous boar's chase (or the giver walk) carried the
        // bot away, and a spawn from there would land on another bot's spot.
        let spawn_at = go_to_spot(game, spot).await?;
        let boar = spawn_npc(game, BOAR, spawn_at).await?;
        let boar_obj = boar.obj_id;
        cleanup.boars.push(boar_obj);
        move_near(game, boar.pos).await?;
        game.change_target(boar_obj).await?;
        // Setup: leave the boar at 1 % so the player's next hit is the killing blow. Send it
        // without the long GM reply drain: a damaged boar keeps moving while the reply is read.
        game.chat(0, "/damage 99%").await?;
        game.drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        let mut result = -1;
        for attempt in 0..3 {
            // The boar may have wandered: walk to its current position and aim again.
            if let Some(at) = game.state.units.get(&boar_obj).map(|u| u.pos) {
                move_near(game, at).await?;
            }
            game.change_target(boar_obj).await?;
            game.start_skill(ATTACK, boar_obj).await?;
            let caster = game.state.self_obj;
            let answer = game
                .wait_for("melee skill response", |p| {
                    own_skill_response(p, ATTACK, caster) || p.is_sc(sc::SCErrorMsgPacket)
                })
                .await
                .with_context(|| format!("attack on boar {boar_obj} got no answer"))?;
            if answer.is_sc(sc::SCErrorMsgPacket) {
                bail!(
                    "attack on boar {boar_obj} rejected with SCErrorMsg {:?}",
                    answer.stream().read_i16().ok()
                );
            }
            result = i32::from(skill_response_result(&answer)?);
            if result == 0 {
                break;
            }
            ensure!(
                retries.note(result),
                "attack on boar {boar_obj} (attempt {}) answered with SkillResult {result}",
                attempt + 1
            );
            retries.publish(r);
        }
        ensure!(
            result == 0,
            "attack on boar {boar_obj} kept failing with SkillResult {result}"
        );
        game.wait_for("SCUnitDeathPacket for the boar", |p| {
            p.is_sc(sc::SCUnitDeathPacket) && packet_obj_id(p).ok() == Some(boar_obj)
        })
        .await
        .with_context(|| format!("the player attack did not kill boar {boar_obj}"))?;
        game.drain(Duration::from_millis(500), Duration::from_secs(3))
            .await?;

        // ---- loot the corpse (all items) ----
        let mut body = aaemu_commons::PacketStream::new();
        body.write_bc(boar_obj).write_bc(boar_obj).write_bool(true);
        game.send_cs(cs::CSLootOpenBagPacket, body.as_bytes())
            .await?;
        let looted = game
            .drain(Duration::from_millis(1000), Duration::from_secs(6))
            .await?;
        let meat = meat_received(&looted);
        cleanup.loot.extend(
            item_stacks_any(&looted)
                .into_iter()
                .filter(|stack| stack.count > 0),
        );
        let q = last_quest_state(&looted, QUEST);
        if let Some(q) = q {
            quest = Some(q);
        }
        let note = format!(
            "boar {boar_obj}: SkillResult {result}, killed; loot packets {}; meat {}; quest \
             objectives {:?}",
            packet_summary(&looted),
            meat,
            quest.map(|q| q.objectives)
        );
        ensure!(
            meat,
            "no boar meat ({MEAT}) was looted from {boar_obj}: {note}"
        );
        r.end(s, note);
    }

    let q = quest.context("no quest state carried the objective progress")?;
    ensure!(
        q.objectives[0] >= MEATS,
        "quest {QUEST} objective is {:?}, want {MEATS} (status {})",
        q.objectives,
        q.status
    );

    // ---- turn in ----
    let s = r.begin("game.questchain.report");
    let giver_at = game
        .state
        .units
        .get(&giver_obj)
        .map_or(giver.pos, |u| u.pos);
    move_near(game, giver_at).await?;
    game.change_target(giver_obj).await?;
    let mut body = aaemu_commons::PacketStream::new();
    body.write_u32(QUEST)
        .write_bc(giver_obj)
        .write_bc(0)
        .write_i32(0);
    game.send_cs(cs::CSCompleteQuestContextPacket, body.as_bytes())
        .await?;
    let answers = game
        .drain(Duration::from_millis(1200), Duration::from_secs(8))
        .await?;
    let completed = answers
        .iter()
        .find(|p| p.is_sc(sc::SCQuestContextCompletedPacket))
        .and_then(|p| p.stream().read_u32().ok());
    cleanup.reward = item_stacks(&answers, REWARD).into_iter().next();
    let reward = cleanup.reward.is_some();
    let tasks: Vec<String> = answers
        .iter()
        .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
        .map(|p| {
            let t = crate::game::parse_item_task(&p.body);
            format!(
                "{}:{:?}",
                t.action,
                t.items
                    .iter()
                    .map(|i| (i.task, i.template, i.id))
                    .collect::<Vec<_>>()
            )
        })
        .collect();
    let note = format!(
        "SCQuestContextCompleted {:?}; reward item {REWARD} received {reward}; item tasks \
         {tasks:?}; answers {}",
        completed,
        packet_summary(&answers)
    );
    ensure!(
        completed == Some(QUEST),
        "quest {QUEST} was not completed: {note}"
    );
    ensure!(
        reward,
        "quest {QUEST} did not grant reward item {REWARD}: {note}"
    );
    // The turn-in took the objective's meat out of the bag: only extra loot is left to destroy.
    consume_loot(&mut cleanup.loot, MEAT, MEATS);
    r.end(s, note);

    Ok(())
}

/// Takes `count` units of `template` out of the tracked `loot` (the quest turn-in consumed
/// them), so the cleanup only destroys what is still in the bag.
fn consume_loot(loot: &mut Vec<ItemStack>, template: u32, mut count: i32) {
    for item in loot.iter_mut().filter(|item| item.template == template) {
        let used = item.count.min(count);
        item.count -= used;
        count -= used;
    }
    loot.retain(|item| item.count > 0);
}

/// Drops/resets the quest, removes the spawned NPCs and destroys the reward, also after a
/// failed run.
async fn cleanup_quest(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    let s = r.begin("game.questchain.cleanup");
    // The loot first: dropping the quest takes its items out of the bag by itself.
    for item in cleanup.loot.drain(..) {
        destroy_item(game, &item).await?;
    }
    let mut body = aaemu_commons::PacketStream::new();
    body.write_u32(QUEST);
    game.send_cs(cs::CSDropQuestContextPacket, body.as_bytes())
        .await?;
    let _ = gm_command(game, &format!("quest uncomplete {QUEST}")).await?;
    for boar in cleanup.boars.drain(..) {
        let _ = gm_command(game, &format!("despawn npc {boar}")).await?;
    }
    if let Some(giver) = cleanup.giver.take() {
        let _ = gm_command(game, &format!("despawn npc {giver}")).await?;
    }
    if let Some(reward) = cleanup.reward.take() {
        destroy_item(game, &reward).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    // Leave the spot where the run began, so repeated runs do not walk the bot away.
    if let Some(home) = cleanup.home.take() {
        teleport_to(game, home).await?;
    }
    r.end(
        s,
        "quest dropped/reset, NPCs removed, reward and loot destroyed, bot back home",
    );
    Ok(())
}

/// Teleports the bot to its spot (which also turns it to yaw 0, see [`spawn_point`]) and returns
/// where a `/spawn npc` from there puts an NPC.
async fn go_to_spot(game: &mut GameClient, spot: (f32, f32, f32)) -> anyhow::Result<(f32, f32)> {
    teleport_to(game, spot).await?;
    Ok(spawn_point(game.state.self_pos))
}

/// Spawns NPC `template` where the bot's own `/spawn npc` puts it (`spawn_at`, right after
/// [`go_to_spot`]) and returns the new unit once it is announced.
async fn spawn_npc(
    game: &mut GameClient,
    template: u32,
    spawn_at: (f32, f32),
) -> anyhow::Result<UnitInfo> {
    let seq = game.state.observation_seq;
    let reply = gm_command(game, &format!("spawn npc {template}")).await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(unit) = own_spawn(&game.state.units, template, spawn_at, seq) {
            return Ok(unit);
        }
        if Instant::now() >= deadline {
            bail!(
                "NPC {template} was not announced within {SPAWN_TOLERANCE} m of {spawn_at:?} after \
                 /spawn npc: {texts:?}; announced since then: {}",
                announced_since(&game.state.units, template, spawn_at, seq)
            );
        }
        game.drain(Duration::from_millis(400), Duration::from_secs(1))
            .await?;
    }
}

/// The NPC the bot's own `/spawn npc` just announced: a unit of `template` announced after
/// observation `seq` within [`SPAWN_TOLERANCE`] of `spawn_at`, the point this bot's command
/// requested (GM-spawned NPCs carry no owner). It is judged by where the announcement placed it,
/// not by where it is now: hostile boars run off at once. Another bot's spawn is a whole slot
/// spacing away from `spawn_at`; of several units at the spot (an earlier run's respawned
/// leftovers) the earliest announcement is the command's own, which is created synchronously.
fn own_spawn(
    units: &HashMap<u32, UnitInfo>,
    template: u32,
    spawn_at: (f32, f32),
    seq: u64,
) -> Option<UnitInfo> {
    let off = |u: &UnitInfo| horizontal(spawn_at, u.announced_pos);
    units
        .values()
        .filter(|u| {
            u.unit_type == 1
                && u.id == template
                && u.announced_seq > seq
                && off(u) <= SPAWN_TOLERANCE
        })
        .min_by(|a, b| {
            a.announced_seq
                .cmp(&b.announced_seq)
                .then(off(a).total_cmp(&off(b)))
                .then(a.obj_id.cmp(&b.obj_id))
        })
        .cloned()
}

/// The units of `template` announced after `seq` with their distance to `spawn_at`: what the
/// error of a missed spawn shows (a facing that differs from yaw 0 puts the NPC elsewhere).
fn announced_since(
    units: &HashMap<u32, UnitInfo>,
    template: u32,
    spawn_at: (f32, f32),
    seq: u64,
) -> String {
    let mut seen: Vec<String> = units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == template && u.announced_seq > seq)
        .map(|u| {
            format!(
                "obj {} at ({:.1}, {:.1}), {:.1} m away",
                u.obj_id,
                u.announced_pos.0,
                u.announced_pos.1,
                horizontal(spawn_at, u.announced_pos)
            )
        })
        .collect();
    seen.sort();
    if seen.is_empty() {
        "nothing".into()
    } else {
        seen.join("; ")
    }
}

/// Walk right next to `pos`.
async fn move_near(game: &mut GameClient, pos: (f32, f32, f32)) -> anyhow::Result<()> {
    game.move_to_facing((pos.0 + 1.2, pos.1, pos.2), pos, 6000)
        .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    Ok(())
}

/// Whether the loot reply put the quest meat into the bag: a positive `SCItemTaskSuccess`
/// entry for it (a mere bag listing or took notification does not prove it).
fn meat_received(packets: &[Packet]) -> bool {
    item_stacks(packets, MEAT)
        .iter()
        .any(|stack| stack.count > 0)
}

/// Distance on the ground (the terrain snaps spawned NPCs to a different height).
fn horizontal(a: (f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn packet_summary(packets: &[Packet]) -> String {
    if packets.is_empty() {
        return "nothing".into();
    }
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for p in packets {
        *counts.entry(p.name()).or_default() += 1;
    }
    counts
        .iter()
        .map(|(n, c)| format!("{n} x{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::coreloop::ItemStack;
    use super::*;

    fn stack(template: u32, item_id: u64, count: i32) -> ItemStack {
        ItemStack {
            template,
            item_id,
            slot_type: 2,
            slot: 4,
            count,
        }
    }

    fn unit(obj_id: u32, template: u32, announced: (f32, f32, f32), seq: u64) -> UnitInfo {
        UnitInfo {
            obj_id,
            name: String::new(),
            unit_type: 1,
            id: template,
            // Hostile boars run off right after the announcement.
            pos: (announced.0 + 40.0, announced.1, announced.2),
            tl_id: 0,
            announced_pos: announced,
            announced_seq: seq,
        }
    }

    #[test]
    fn the_own_spawn_is_the_unit_announced_after_the_command_at_the_requested_point() {
        // The bot stands at (100, 200) facing north: its command asks for (100, 203).
        let spawn_at = (100.0, 203.0);
        let units: HashMap<u32, UnitInfo> = [
            // already known before the command (older announcement), also at the spawn point
            unit(1, BOAR, (100.0, 203.0, 51.0), 5),
            // announced after the command, but another bot's spawn 25 m away
            unit(2, BOAR, (125.0, 203.0, 50.0), 11),
            // 3 m from the bot on its other side: not where this command put its NPC
            unit(3, BOAR, (100.0, 197.0, 50.0), 12),
            // another template at the spawn point
            unit(4, GIVER, (100.0, 203.0, 50.0), 13),
            // the own one: 0.5 m off the point, 4 m higher (terrain snap), and it has since
            // run 40 m off
            unit(5, BOAR, (100.4, 203.3, 54.0), 14),
        ]
        .into_iter()
        .map(|u| (u.obj_id, u))
        .collect();
        let own = own_spawn(&units, BOAR, spawn_at, 10).expect("an own spawn");
        assert_eq!(own.obj_id, 5);
        // Nothing qualifies before the announcement or for another template.
        assert!(own_spawn(&units, BOAR, spawn_at, 14).is_none());
        assert!(own_spawn(&units, 99, spawn_at, 10).is_none());
    }

    #[test]
    fn a_unit_just_outside_the_tolerance_is_not_the_own_spawn() {
        let spawn_at = (0.0, 3.0);
        let at = |off: f32| {
            [unit(1, BOAR, (off, 3.0, 0.0), 20)]
                .into_iter()
                .map(|u| (u.obj_id, u))
                .collect::<HashMap<_, _>>()
        };
        assert!(own_spawn(&at(SPAWN_TOLERANCE - 0.1), BOAR, spawn_at, 10).is_some());
        assert!(own_spawn(&at(SPAWN_TOLERANCE + 0.1), BOAR, spawn_at, 10).is_none());
        // The old ring test accepted anything 3 m from the bot, whichever side: not any more.
        assert!(own_spawn(&at(-3.0), BOAR, spawn_at, 10).is_none());
    }

    #[test]
    fn of_several_units_at_the_spot_the_earliest_announcement_wins() {
        // A respawned leftover of an earlier run stands exactly on the point, the command's own
        // NPC 0.6 m beside it: the command created its NPC first, so it was announced first.
        let units: HashMap<u32, UnitInfo> = [
            unit(8, BOAR, (0.0, 3.0, 0.0), 21),
            unit(7, BOAR, (0.6, 3.0, 0.0), 20),
        ]
        .into_iter()
        .map(|u| (u.obj_id, u))
        .collect();
        assert_eq!(
            own_spawn(&units, BOAR, (0.0, 3.0), 10).map(|u| u.obj_id),
            Some(7)
        );
        // At the same announcement the nearer one wins, then the lower object id.
        let units: HashMap<u32, UnitInfo> = [
            unit(3, BOAR, (0.5, 3.0, 0.0), 20),
            unit(2, BOAR, (0.0, 3.0, 0.0), 20),
            unit(1, BOAR, (0.0, 3.0, 0.0), 20),
        ]
        .into_iter()
        .map(|u| (u.obj_id, u))
        .collect();
        assert_eq!(
            own_spawn(&units, BOAR, (0.0, 3.0), 10).map(|u| u.obj_id),
            Some(1)
        );
    }

    #[test]
    fn eight_parallel_bots_each_find_only_their_own_npc() {
        // Every bot began at the same origin (the Nuian start) and owns one slot; each bot's
        // command announces a boar at its own spawn point to all of them.
        let origin = (13000.0, 14000.0, 100.0);
        let spawn_ats: Vec<(f32, f32)> = (0..8)
            .map(|slot| {
                let (dx, dy) = slot_offset(slot);
                spawn_point((origin.0 + dx, origin.1 + dy, origin.2))
            })
            .collect();
        let units: HashMap<u32, UnitInfo> = spawn_ats
            .iter()
            .enumerate()
            .map(|(i, at)| unit(100 + i as u32, BOAR, (at.0, at.1, 100.0), 50 + i as u64))
            .map(|u| (u.obj_id, u))
            .collect();
        for (i, at) in spawn_ats.iter().enumerate() {
            assert_eq!(
                own_spawn(&units, BOAR, *at, 10).map(|u| u.obj_id),
                Some(100 + i as u32),
                "bot {i} must bind its own boar"
            );
        }
    }

    #[test]
    fn a_missed_spawn_names_what_was_announced_instead() {
        let units: HashMap<u32, UnitInfo> = [
            unit(6, BOAR, (110.0, 203.0, 0.0), 12),
            unit(5, BOAR, (100.0, 197.0, 0.0), 11),
            unit(4, BOAR, (100.0, 203.0, 0.0), 3),
            unit(7, GIVER, (100.0, 203.0, 0.0), 13),
        ]
        .into_iter()
        .map(|u| (u.obj_id, u))
        .collect();
        assert_eq!(
            announced_since(&units, BOAR, (100.0, 203.0), 10),
            "obj 5 at (100.0, 197.0), 6.0 m away; obj 6 at (110.0, 203.0), 10.0 m away"
        );
        assert_eq!(announced_since(&units, BOAR, (100.0, 203.0), 20), "nothing");
    }

    #[test]
    fn retried_hits_are_counted_by_their_skill_result() {
        let mut retries = Retries::default();
        assert!(retries.note(i32::from(skill_result::OUTOF_ANGLE)));
        assert!(retries.note(i32::from(skill_result::OUTOF_ANGLE)));
        assert!(retries.note(i32::from(skill_result::OBSTACLE)));
        assert!(retries.note(i32::from(skill_result::TOO_FAR_RANGE)));
        assert_eq!(
            retries,
            Retries {
                too_far: 1,
                out_of_angle: 2,
                obstacle: 1
            }
        );
        // A hit that landed and any other refusal are not retries.
        assert!(!retries.note(0));
        assert!(!retries.note(-1));
        assert!(!retries.note(i32::from(skill_result::OBSTACLE) + 1000));
        assert_eq!(retries.too_far + retries.out_of_angle + retries.obstacle, 4);
    }

    #[test]
    fn the_retry_counts_become_run_variables_of_the_outcome() {
        let rec = crate::record::Recorder::new(false);
        let mut runner = Runner {
            rec: &rec,
            out: Default::default(),
        };
        let mut retries = Retries::default();
        retries.publish(&mut runner);
        // Zero is published too, so a dashboard can tell "no retry" from "not measured".
        assert_eq!(runner.out.vars.get("retry_out_of_angle"), Some(&0));
        retries.note(i32::from(skill_result::OUTOF_ANGLE));
        retries.note(i32::from(skill_result::OBSTACLE));
        retries.publish(&mut runner);
        assert_eq!(runner.out.vars.get("retry_out_of_angle"), Some(&1));
        assert_eq!(runner.out.vars.get("retry_obstacle"), Some(&1));
        assert_eq!(runner.out.vars.get("retry_too_far"), Some(&0));
    }

    #[test]
    fn the_turn_in_consumes_the_objective_meat_and_only_the_rest_is_left_to_destroy() {
        // Three boars, one meat each: the turn-in took all of it.
        let mut loot = vec![stack(MEAT, 7, 1), stack(MEAT, 7, 1), stack(MEAT, 7, 1)];
        consume_loot(&mut loot, MEAT, MEATS);
        assert!(loot.is_empty());

        // A boar that dropped two meat leaves one over, and other loot is never touched.
        let mut loot = vec![stack(MEAT, 7, 1), stack(MEAT, 7, 2), stack(MEAT, 7, 2)];
        loot.push(stack(8022, 8, 1));
        consume_loot(&mut loot, MEAT, MEATS);
        assert_eq!(loot, [stack(MEAT, 7, 2), stack(8022, 8, 1)]);
    }
}

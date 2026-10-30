//! `questchain`: accept quest 251 "Angry Boars", kill three Solzreed boars with the player's
//! melee skill, loot their meat, and turn the quest in with the real client packets (GM account
//! needed for the setup commands).
//!
//! Quest 251 (`QuestActConAcceptNpc` 3512, `QuestActObjItemGather` 3 x item 4058 "Solzreed Boar
//! Meat", `QuestActConReportNpc` 3512, reward item 18791). The bot spawns the giver 3512 and the
//! boars 3475 in front of itself, accepts with `CSStartQuestContext`, weakens each boar with the
//! GM `/damage 99%` setup command, finishes it with `CSStartSkill` 2, loots the corpse with
//! `CSLootOpenBag(lootAll)`, then reports with `CSCompleteQuestContext` and checks the reward.

use std::time::{Duration, Instant};

use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, ensure, Context};

use super::coreloop::{
    destroy_item, gm_access, item_stacks, loot_bag_templates, loot_took_template, ItemStack,
};
use super::questitem::{chat_text, gm_command, last_quest_state};
use super::{own_skill_response, packet_obj_id, quest_template_id, skill_response_result, Runner};
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

#[derive(Default)]
struct Cleanup {
    giver: Option<u32>,
    boars: Vec<u32>,
    reward: Option<ItemStack>,
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;
    let mut cleanup = Cleanup::default();
    let result = run_inner(game, r, &mut cleanup).await;
    let cleanup_result = cleanup_quest(game, r, &mut cleanup).await;
    let leave = super::coreloop::leave_world(game).await;
    result?;
    cleanup_result?;
    leave?;
    Ok(())
}

async fn run_inner(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    // A previous run (or another scenario) may have completed the quest: reset it first.
    let _ = gm_command(game, &format!("quest uncomplete {QUEST}")).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;

    // ---- the giver in front of the bot ----
    let s = r.begin("game.questchain.giver");
    let giver = spawn_npc(game, GIVER).await?;
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
        let boar = spawn_npc(game, BOAR).await?;
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
                result
                    == i32::from(aaemu_game::systems::skills::types::skill_result::TOO_FAR_RANGE),
                "attack on boar {boar_obj} (attempt {}) answered with SkillResult {result}",
                attempt + 1
            );
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
        let _ = gm_command(game, &format!("despawn npc {boar_obj}")).await?;
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
    move_near(game, giver.pos).await?;
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
    r.end(s, note);

    Ok(())
}

/// Drops/resets the quest, removes the spawned NPCs and destroys the reward, also after a
/// failed run.
async fn cleanup_quest(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    let s = r.begin("game.questchain.cleanup");
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
    r.end(s, "quest dropped/reset, NPCs removed, reward destroyed");
    Ok(())
}
/// Spawns NPC `template` in front of the bot and returns the new unit once it is announced.
async fn spawn_npc(game: &mut GameClient, template: u32) -> anyhow::Result<UnitInfo> {
    let before: Vec<u32> = game.state.units.keys().copied().collect();
    let reply = gm_command(game, &format!("spawn npc {template}")).await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        game.drain(Duration::from_millis(400), Duration::from_secs(1))
            .await?;
        let found = game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == template && !before.contains(&u.obj_id))
            .min_by_key(|u| u.obj_id)
            .cloned();
        if let Some(unit) = found {
            return Ok(unit);
        }
        if Instant::now() >= deadline {
            bail!("NPC {template} was not announced after /spawn npc: {texts:?}");
        }
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

/// Whether a loot reply gave the quest meat: an `ItemTask` create or an `SCLootItemTook`.
fn meat_received(packets: &[Packet]) -> bool {
    packets.iter().any(|p| {
        if p.is_sc(sc::SCLootItemTookPacket) {
            return loot_took_template(p).ok() == Some(MEAT);
        }
        if p.is_sc(sc::SCLootBagDataPacket) {
            return loot_bag_templates(&p.body).is_ok_and(|t| t.contains(&MEAT));
        }
        added_template(p, MEAT)
    })
}

/// Whether an `SCItemTaskSuccessPacket` created template `template`.
fn added_template(p: &Packet, template: u32) -> bool {
    if !p.is_sc(sc::SCItemTaskSuccessPacket) {
        return false;
    }
    let task = crate::game::parse_item_task(&p.body);
    task.items
        .iter()
        .any(|i| i.task == 5 && i.template == template)
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

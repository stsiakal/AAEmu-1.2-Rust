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

use super::coreloop::{gm_access, loot_bag_templates, loot_took_template};
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

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;

    // ---- the giver in front of the bot ----
    let s = r.begin("game.questchain.giver");
    let giver = spawn_npc(game, GIVER).await?;
    let giver_obj = giver.obj_id;
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
        move_near(game, boar.pos).await?;
        game.change_target(boar_obj).await?;
        // Setup: leave the boar at 1 % so the player's next hit is the killing blow.
        let reply = gm_command(game, "damage 99%").await?;
        let damage_text: Vec<String> = reply.iter().filter_map(chat_text).collect();
        ensure!(
            !damage_text
                .iter()
                .any(|t| t.contains("Cannot") || t.contains("no target")),
            "/damage did not hit boar {boar_obj}: {damage_text:?}"
        );
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
        let result = skill_response_result(&answer)?;
        ensure!(
            result == 0,
            "attack on boar {boar_obj} answered with SkillResult {result}"
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
    let reward = answers
        .iter()
        .any(|p| p.is_sc(sc::SCItemTaskSuccessPacket) && added_template(p, REWARD));
    let note = format!(
        "SCQuestContextCompleted {:?}; reward item {REWARD} received {reward}; answers {}",
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

    // ---- tidy up: the GM-spawned NPC ----
    let s = r.begin("game.questchain.cleanup");
    let _ = gm_command(game, &format!("despawn npc {giver_obj}")).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    r.end(s, "spawned NPC removed");

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
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

//! `gather`: mine an iron vein with the real gathering interaction and check the labor cost, the
//! phase change and the loot (GM account needed for the setup commands).
//!
//! Setup: `/labor` tops the bot up, `/spawn doodad 1671` places an iron vein 3 m in front of it.
//! The GM spawn makes the spawner the doodad's owner, so the bot only accepts the new vein whose
//! owner is its own object: bots mining side by side each find and mine their own vein.
//! The action is the client's gathering skill 13986 (the vein's `DoodadFuncUse` skill, 10 labor,
//! range 4, 4.5 s cast) cast with a doodad target. The server advances the vein phase and grants
//! loot pack 6359 (iron ore 8022, rare 8081 or stone 8008) directly to the bag. A genuine empty
//! roll is retried on a fresh vein; a labor mismatch or an error fails at once. The ore and the
//! GM-placed vein are removed afterwards.

use std::time::{Duration, Instant};

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::coreloop::{destroy_item, gm_access, item_stacks, labor_change, ItemStack};
use super::questitem::gm_command;
use super::{own_skill_response, skill_response_result, Runner};
use crate::game::{DoodadInfo, GameClient};

/// Doodad 1671 "Iron Vein" and its start func's gathering skill (10 labor, 4.5 s cast, range 4).
const VEIN: u32 = 1671;
const MINING_SKILL: u32 = 13986;
const CAST_TIME: Duration = Duration::from_millis(4500);
/// Items loot pack 6359 can grant: iron ore, high-purity iron ore and stone.
const LOOT_ITEMS: [u32; 3] = [8022, 8081, 8008];
/// `SkillTemplate.ConsumeLaborPower` of 13986.
const LABOR_COST: i32 = 10;

#[derive(Default)]
struct Cleanup {
    vein: Option<u32>,
    /// Doodads that existed before a spawn whose vein was never announced to this bot.
    unbound: Option<Vec<u32>>,
    loot: Vec<ItemStack>,
}

/// The iron vein this bot spawned since `before` (the doodad ids it knew then): the GM spawn
/// makes the spawner the owner, so another bot's vein never matches.
fn own_new_vein(game: &GameClient, before: &[u32]) -> Option<DoodadInfo> {
    let owner = game.state.self_obj;
    game.state
        .doodads
        .values()
        .filter(|d| d.template_id == VEIN && d.owner_obj_id == owner && !before.contains(&d.obj_id))
        .min_by_key(|d| d.obj_id)
        .cloned()
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;
    let mut cleanup = Cleanup::default();
    let result = run_inner(game, r, &mut cleanup).await;
    let cleanup_result = cleanup_gather(game, r, &mut cleanup).await;
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
    let s = r.begin("game.gather.labor");
    gm_command(game, "labor 100").await?;
    r.end(s, "labor +100");

    // The loot pack rolls one of several groups and may be empty on an unlucky roll: mine
    // another GM-placed vein until one grants loot (three tries).
    let mut looted = false;
    let mut last = String::new();
    for attempt in 1..=3 {
        let s = r.begin(&format!("game.gather.mine{attempt}"));
        let before: Vec<u32> = game.state.doodads.keys().copied().collect();
        gm_command(game, &format!("spawn doodad {VEIN}")).await?;
        let vein = match wait_new_vein(game, &before).await {
            Some(vein) => vein,
            None => {
                r.fail(s, "the spawned iron vein was not announced");
                cleanup.unbound = Some(before);
                continue;
            }
        };
        cleanup.unbound = None;
        cleanup.vein = Some(vein.obj_id);
        r.var("vein_obj", vein.obj_id);
        r.var("vein_owner_obj", vein.owner_obj_id);
        let phase_before = vein.func_group_id;
        let pos = vein.pos;
        game.move_to_facing((pos.0 + 1.2, pos.1, pos.2), pos, 4000)
            .await?;
        game.drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        game.change_target(vein.obj_id).await?;
        game.start_doodad_skill(MINING_SKILL, vein.obj_id).await?;
        let caster = game.state.self_obj;
        let answer = game
            .wait_for("gathering skill response", |p| {
                own_skill_response(p, MINING_SKILL, caster) || p.is_sc(sc::SCErrorMsgPacket)
            })
            .await
            .context("the gathering cast got no answer")?;
        if answer.is_sc(sc::SCErrorMsgPacket) {
            bail!(
                "gathering cast rejected with SCErrorMsg {:?}",
                answer.stream().read_i16().ok()
            );
        }
        let result = skill_response_result(&answer)?;
        ensure!(
            result == 0,
            "gathering cast answered with SkillResult {result}"
        );

        // Bounded wait for the cast's effects: the labor charge, the phase change and the loot
        // packet (an empty roll legitimately has no loot packet).
        let deadline = Instant::now() + CAST_TIME + Duration::from_secs(6);
        let mut labor: Option<i32> = None;
        let mut phase_changed = false;
        let mut error = None;
        let mut packets = Vec::new();
        let mut loot = Vec::new();
        while Instant::now() < deadline {
            let Some(p) = game.recv_within(Duration::from_millis(250)).await? else {
                continue;
            };
            if p.is_sc(sc::SCErrorMsgPacket) {
                error = p.stream().read_i16().ok();
                break;
            }
            // Labor regenerates in positive steps; only the skill's negative cost matters.
            if let Some(change) = labor_change(&p).filter(|change| *change < 0) {
                labor = Some(change);
            }
            if p.is_sc(sc::SCDoodadPhaseChangedPacket) {
                let mut st = p.stream();
                let obj = st.read_bc()?;
                let group = st.read_u32()?;
                if obj == vein.obj_id && group != phase_before {
                    phase_changed = true;
                }
            }
            for item in LOOT_ITEMS {
                loot.extend(item_stacks(std::slice::from_ref(&p), item));
            }
            packets.push(p);
            if labor.is_some() && phase_changed && !loot.is_empty() {
                break;
            }
        }
        ensure!(
            error.is_none(),
            "gathering returned SCErrorMsg {error:?}: packets {}",
            packet_summary(&packets)
        );
        // A phase that did not change is a server failure, not an empty loot roll.
        ensure!(
            phase_changed,
            "the vein {vein:?} stayed in phase {phase_before}: SkillResult {result}; packets {}",
            packet_summary(&packets)
        );
        ensure!(
            labor == Some(-LABOR_COST),
            "gathering spent {labor:?} labor instead of -{LABOR_COST}: packets {}",
            packet_summary(&packets)
        );
        last = format!(
            "try {attempt}: vein {} (owner {}); SkillResult {result}; labor {labor:?}; phase \
             {phase_before} -> {}; loot {loot:?}",
            vein.obj_id,
            vein.owner_obj_id,
            game.state
                .doodads
                .get(&vein.obj_id)
                .map_or(0, |d| d.func_group_id)
        );
        cleanup.loot.extend(loot);
        if !cleanup.loot.is_empty() {
            looted = true;
            r.end(s, last.clone());
            break;
        }
        r.end(s, format!("{last}; genuine empty loot roll, retrying"));
        // Remove the empty vein before the next try.
        let _ = gm_command(game, &format!("spawn remove {}", vein.obj_id)).await?;
        cleanup.vein = None;
    }
    ensure!(looted, "no gathering attempt granted loot: {last}");
    Ok(())
}

/// Waits until the vein this bot spawned is announced to it.
async fn wait_new_vein(game: &mut GameClient, before: &[u32]) -> Option<DoodadInfo> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let found = own_new_vein(game, before);
        if found.is_some() {
            return found;
        }
        if Instant::now() >= deadline {
            return None;
        }
        let _ = game
            .drain(Duration::from_millis(400), Duration::from_secs(1))
            .await;
    }
}

/// Removes the GM-placed vein and the ore this run put into the bag, also after a failed run.
async fn cleanup_gather(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    let s = r.begin("game.gather.cleanup");
    if let Some(vein) = cleanup.vein.take() {
        if game.state.doodads.contains_key(&vein) {
            let _ = gm_command(game, &format!("spawn remove {vein}")).await?;
        }
    }
    if let Some(before) = cleanup.unbound.take() {
        // A vein announced late is still this bot's (owner match): remove it.
        game.drain(Duration::from_millis(500), Duration::from_secs(2))
            .await?;
        if let Some(vein) = own_new_vein(game, &before) {
            let _ = gm_command(game, &format!("spawn remove {}", vein.obj_id)).await?;
        }
    }
    for item in cleanup.loot.drain(..) {
        destroy_item(game, &item).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    r.end(s, "vein removed, ore destroyed");
    Ok(())
}

fn packet_summary(packets: &[crate::game::Packet]) -> String {
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

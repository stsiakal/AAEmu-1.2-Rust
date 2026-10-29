//! `gather`: mine an iron vein with the real gathering interaction and check the labor cost and
//! the loot (GM account needed for the setup commands).
//!
//! Setup: `/labor` tops the bot up, `/spawn doodad 1671` places an iron vein 3 m in front of it.
//! The action is the client's gathering skill 13986 (the vein's `DoodadFuncUse` skill, 10 labor,
//! range 4) cast with a doodad target. The server advances the vein to its broken phase and
//! grants loot pack 6359 (3-4 iron ore, item 8022) directly to the bag. A `/spawn remove` deletes
//! the GM-placed vein afterwards.

use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::coreloop::{gm_access, labor_change};
use super::questitem::gm_command;
use super::{own_skill_response, skill_response_result, Runner, QUIET};
use crate::game::{GameClient, GameEvent};

/// Doodad 1671 "Iron Vein" and its start func's gathering skill (10 labor, range 4).
const VEIN: u32 = 1671;
const MINING_SKILL: u32 = 13986;
/// Iron Ore, the guaranteed drop of loot pack 6359.
const IRON_ORE: u32 = 8022;
/// `SkillTemplate.ConsumeLaborPower` of 13986.
const LABOR_COST: i32 = 10;

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;

    // ---- an iron vein in front of the bot ----
    let s = r.begin("game.gather.setup");
    gm_command(game, "labor 100").await?;
    let before: Vec<u32> = game.state.doodads.keys().copied().collect();
    let reply = gm_command(game, &format!("spawn doodad {VEIN}")).await?;
    println!(
        "[gather] /spawn doodad {VEIN}: {:?}",
        reply
            .iter()
            .filter_map(super::questitem::chat_text)
            .collect::<Vec<_>>()
    );
    game.drain(QUIET, Duration::from_secs(4)).await?;
    let me = game.state.self_pos;
    let vein = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == VEIN && !before.contains(&d.obj_id))
        .min_by(|a, b| dist(me, a.pos).total_cmp(&dist(me, b.pos)))
        .cloned()
        .context("the spawned iron vein was not announced")?;
    r.var("vein_obj", vein.obj_id);
    r.end(
        s,
        format!(
            "vein obj {} at {:?} ({:.1} m), labor +100",
            vein.obj_id,
            vein.pos,
            dist(me, vein.pos)
        ),
    );

    // ---- the gathering cast ----
    let s = r.begin("game.gather.mine");
    let pos = vein.pos;
    game.move_to_facing((pos.0 + 1.2, pos.1, pos.2), pos, 4000)
        .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(1))
        .await?;
    let events_before = game.state.events.len();
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
    let seen = game.drain(QUIET, Duration::from_secs(6)).await?;

    let labor_spent: i32 = seen.iter().filter_map(labor_change).sum();
    let items: Vec<(u8, u32)> = game.state.events[events_before..]
        .iter()
        .filter_map(|e| match e {
            GameEvent::ItemTask(t) => Some(t),
            GameEvent::ErrorMsg(_) => None,
        })
        .flat_map(|t| t.items.iter())
        .filter(|i| i.template != 0)
        .map(|i| (i.task, i.template))
        .collect();
    let ore = items
        .iter()
        .any(|(task, tpl)| *task == 5 && *tpl == IRON_ORE);
    let error = game.state.events[events_before..]
        .iter()
        .find_map(|e| match e {
            GameEvent::ErrorMsg(code) => Some(*code),
            GameEvent::ItemTask(_) => None,
        });
    let phase = game
        .state
        .doodads
        .get(&vein.obj_id)
        .map(|d| d.func_group_id);
    let note = format!(
        "SkillResult {result}; labor change {labor_spent:?} (want -{LABOR_COST}); iron ore \
         received {ore}; item tasks {items:?}; phase {phase:?}; errors {error:?}"
    );
    ensure!(
        labor_spent == -LABOR_COST,
        "gathering spent {labor_spent} labor instead of -{LABOR_COST}: {note}"
    );
    ensure!(
        ore,
        "the gathering cast granted no iron ore ({IRON_ORE}): {note}"
    );
    ensure!(error.is_none(), "gathering returned an error: {note}");
    r.end(s, note);

    // ---- tidy up: the GM-placed vein ----
    let s = r.begin("game.gather.cleanup");
    if game.state.doodads.contains_key(&vein.obj_id) {
        let _ = gm_command(game, &format!("spawn remove {}", vein.obj_id)).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    r.end(s, "vein removed");

    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

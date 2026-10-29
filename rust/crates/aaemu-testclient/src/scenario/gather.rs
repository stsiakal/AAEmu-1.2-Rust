//! `gather`: mine an iron vein with the real gathering interaction and check the labor cost and
//! the loot (GM account needed for the setup commands).
//!
//! Setup: `/labor` tops the bot up, `/spawn doodad 1671` places an iron vein 3 m in front of it.
//! The action is the client's gathering skill 13986 (the vein's `DoodadFuncUse` skill, 10 labor,
//! range 4) cast with a doodad target. The server advances the vein to its broken phase and
//! grants loot pack 6359 (iron ore 8022, rare 8081 or stone 8008) directly to the bag. A `/spawn remove` deletes
//! the GM-placed vein afterwards.

use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::coreloop::{gm_access, labor_change};
use super::questitem::gm_command;
use super::{own_skill_response, skill_response_result, Runner, QUIET};
use crate::game::GameClient;

/// Doodad 1671 "Iron Vein" and its start func's gathering skill (10 labor, range 4).
const VEIN: u32 = 1671;
const MINING_SKILL: u32 = 13986;
/// Items loot pack 6359 can grant: iron ore, high-purity iron ore and stone.
const LOOT_ITEMS: [u32; 3] = [8022, 8081, 8008];
/// `SkillTemplate.ConsumeLaborPower` of 13986.
const LABOR_COST: i32 = 10;

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;

    let s = r.begin("game.gather.labor");
    gm_command(game, "labor 100").await?;
    r.end(s, "labor +100");

    // The loot pack rolls one of several groups (iron ore, rare ore or stone) and may be empty
    // on an unlucky roll: mine another GM-placed vein until one grants loot (three tries).
    let mut looted = false;
    let mut last = String::new();
    for attempt in 1..=3 {
        let s = r.begin(&format!("game.gather.mine{attempt}"));
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
        let Some(vein) = game
            .state
            .doodads
            .values()
            .filter(|d| d.template_id == VEIN && !before.contains(&d.obj_id))
            .min_by(|a, b| dist(me, a.pos).total_cmp(&dist(me, b.pos)))
            .cloned()
        else {
            r.fail(s, "the spawned iron vein was not announced");
            continue;
        };
        r.var("vein_obj", vein.obj_id);
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
        let seen = game.drain(QUIET, Duration::from_secs(6)).await?;

        let labor_spent: i32 = seen.iter().filter_map(labor_change).sum();
        let loot: Vec<u32> = LOOT_ITEMS
            .iter()
            .copied()
            .filter(|&tpl| super::questitem::stack_location(&seen, tpl).is_some())
            .collect();
        let tasks: Vec<String> = seen
            .iter()
            .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
            .map(|p| {
                let t = crate::game::parse_item_task(&p.body);
                format!(
                    "{}:{:?}",
                    t.action,
                    t.items.iter().map(|i| i.template).collect::<Vec<_>>()
                )
            })
            .collect();
        let error = seen.iter().find_map(|p| {
            if p.is_sc(sc::SCErrorMsgPacket) {
                p.stream().read_i16().ok()
            } else {
                None
            }
        });
        let phase = game
            .state
            .doodads
            .get(&vein.obj_id)
            .map(|d| d.func_group_id);
        last = format!(
            "try {attempt}: SkillResult {result}; labor change {labor_spent:?} (want -{LABOR_COST}); \
             loot {loot:?}; item tasks {tasks:?}; phase {phase:?}; errors {error:?}"
        );
        let ok = labor_spent == -LABOR_COST && !loot.is_empty() && error.is_none();
        if game.state.doodads.contains_key(&vein.obj_id) {
            let _ = gm_command(game, &format!("spawn remove {}", vein.obj_id)).await?;
        }
        game.drain(Duration::from_millis(300), Duration::from_secs(2))
            .await?;
        if ok {
            looted = true;
            r.end(s, last.clone());
            break;
        }
        r.fail(s, last.clone());
    }
    ensure!(looted, "no gathering attempt granted loot: {last}");

    let s = r.begin("game.leave");
    super::coreloop::leave_world(game).await?;
    r.end(s, "left");
    Ok(())
}
fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

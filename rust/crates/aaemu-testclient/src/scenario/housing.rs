//! `housing`: build a small Nuian house on a real housing plot with the client packets: tax
//! quote, `CSCreateHouse`, the three build steps (skills 14575 / 14575 / 14574 with their
//! materials), then the tax info. The house is demolished again with the GM `/house forcedemo`
//! command so the plot stays free for the next run (GM account needed for setup / cleanup).
//!
//! Plot: the original client's Shape126 in zone 142 (Mahadevi) at 13944.868, 14216.393 resolves
//! to housing area 52 (group 1 "general residential", category 1 = small house), which is exactly
//! what design 172 ("cozy Nuian house") requires.

use std::time::{Duration, Instant};

use aaemu_commons::helpers::convert_coord_to_long;
use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, ensure, Context};

use super::coreloop::{
    added_stack, await_leave, destroy_item, floor_height, gm_access, house_view, item_stacks,
    teleport_to, HouseView, ItemStack,
};
use super::questitem::gm_command;
use super::{own_skill_response, skill_response_result, Runner};
use crate::game::{GameClient, GameState, Packet};

/// Design 172 "cozy Nuian house" and its design item.
const DESIGN: u32 = 172;
const DESIGN_ITEM: u32 = 21166;
/// Placement tax is paid with bound tax certificates (Feature `taxItem` is on by default).
const TAX_CERTIFICATE: u32 = 31892;
/// `COPPER_PER_TAX_CERTIFICATE` of the server's housing tax.
const COPPER_PER_CERTIFICATE: i32 = 10_000;
/// `housing_build_steps` of design 172: (skill, material item, actions).
const BUILD_STEPS: [(u32, u32, i32); 3] = [(14575, 17684, 1), (14575, 17684, 1), (14574, 17683, 1)];
/// Original client housing polygon in zone 142 (see `npcs::subzone` tests).
const PLOT: (f32, f32) = (13944.868, 14216.393);
/// Offsets tried when an earlier run's house still occupies the plot.
const OFFSETS: [(f32, f32); 16] = [
    (0.0, 0.0),
    (30.0, 0.0),
    (-30.0, 0.0),
    (0.0, 30.0),
    (0.0, -30.0),
    (45.0, 45.0),
    (-45.0, -45.0),
    (60.0, -60.0),
    (-60.0, 60.0),
    (60.0, 0.0),
    (-60.0, 0.0),
    (0.0, 60.0),
    (0.0, -60.0),
    (90.0, 0.0),
    (-90.0, 0.0),
    (0.0, 90.0),
];
/// `ErrorMessageType` of the placement rejections worth retrying on another spot.
const INVALID_AREA: i16 = 112;
const OVERLAP_HOUSE: i16 = 113;
const OVERLAP_UNIT: i16 = 114;

/// Visible part of `SCConstructHouseTaxPacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TaxQuote {
    design: u32,
    total: i32,
}

fn tax_quote(body: &[u8]) -> anyhow::Result<TaxQuote> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let design = s.read_u32()?;
    s.read_i32()?; // heavy tax house count
    s.read_i32()?; // normal tax house count
    s.read_bool()?; // is heavy tax house
    s.read_i32()?; // base tax
    s.read_i32()?; // deposit
    let total = s.read_i32()?;
    Ok(TaxQuote { design, total })
}

/// `SCHouseBuildProgressPacket`: `(tl, model, all steps, current step)`.
fn build_progress(body: &[u8]) -> anyhow::Result<(u16, u32, i32, i32)> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    Ok((s.read_u16()?, s.read_u32()?, s.read_i32()?, s.read_i32()?))
}

fn error_code(p: &Packet) -> Option<i16> {
    if p.is_sc(sc::SCErrorMsgPacket) {
        p.stream().read_i16().ok()
    } else {
        None
    }
}

/// `CSConstructHouseTaxPacket` / `CSCreateHousePacket` position encoding.
fn coord_pos(s: &mut PacketStream, x: f32, y: f32, z: f32) {
    s.write_i64(convert_coord_to_long(x))
        .write_i64(convert_coord_to_long(y))
        .write_f32(z);
}

/// How far the re-select of the finished house got. The cleanup needs the character completely
/// in the world (spawned and announced as in game) to run the GM demolition, so whatever step is
/// still missing when the run fails is finished by [`reenter_world`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reselect {
    /// `CSLeaveWorld(reselect)` was sent: the server removes the character when its logout timer
    /// ends and tells the client with `SCLeaveWorldGranted`.
    Leaving(u32),
    /// The character left the world; the client is in the lobby.
    Lobby(u32),
    /// `CSSelectCharacter` was sent: the server has the character, but the client has not spawned
    /// it yet.
    Selected,
    /// `CSSpawnCharacter` was sent: only the in-game notices are still to send.
    Spawned,
}

#[derive(Default)]
struct Cleanup {
    /// Every house of the bot's character the run has to demolish, `(object id, tl)`: the one it
    /// created and any other the server announced (a late `SCMyHouse`, a leftover of an earlier
    /// run).
    houses: Vec<(u32, u16)>,
    items: Vec<ItemStack>,
    reselect: Option<Reselect>,
}

impl Cleanup {
    fn add_house(&mut self, obj_id: u32, tl: u16) {
        if !self.houses.iter().any(|(obj, _)| *obj == obj_id) {
            self.houses.push((obj_id, tl));
        }
    }

    /// Registers every house of the selected character the server announced so far
    /// (`GameState::my_houses`, filled by whichever wait read the packet), so a house that shows
    /// up late, after the create step stopped listening, is demolished too.
    fn register_houses(&mut self, state: &GameState) {
        for house in state
            .my_houses
            .iter()
            .filter(|h| h.owner_id == state.selected_char)
        {
            self.add_house(house.obj_id, house.tl);
        }
    }
}

/// The certificates the server charges for `total` copper (`construction_certificates`).
fn construction_certificates(total: i32) -> i32 {
    ((i64::from(total.max(0)) + i64::from(COPPER_PER_CERTIFICATE) - 1)
        / i64::from(COPPER_PER_CERTIFICATE)) as i32
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;
    let mut cleanup = Cleanup::default();
    let result = run_inner(game, r, &mut cleanup).await;
    let cleanup_result = cleanup_house(game, r, &mut cleanup).await;
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
    // ---- design and build materials ----
    let s = r.begin("game.housing.items");
    let reply = gm_command(game, &format!("item add self {DESIGN_ITEM} 1")).await?;
    let design = added_stack(&reply, DESIGN_ITEM).with_context(|| {
        format!(
            "no design item {DESIGN_ITEM} was added (GM replies {:?})",
            reply
                .iter()
                .filter_map(super::questitem::chat_text)
                .collect::<Vec<_>>()
        )
    })?;
    cleanup.items.push(design);
    let design_item = design.item_id;
    // Three build actions cost 25 labor each. The build materials are tradepacks that occupy the
    // single backpack slot, so each one is added right before the step that consumes it.
    gm_command(game, "labor 200").await?;
    r.var("design_item_id", design_item);
    r.end(s, format!("design item {design_item} ({DESIGN_ITEM})"));

    // ---- a free spot on the housing plot ----
    let s = r.begin("game.housing.teleport");
    let start = (PLOT.0, PLOT.1, 120.0);
    teleport_to(game, start).await?;
    let floor = floor_height(game)
        .await
        .context("the housing plot floor height could not be read")?;
    let plot = (PLOT.0, PLOT.1, floor + 0.1);
    teleport_to(game, plot).await?;
    r.var("plot_x", PLOT.0 as u64);
    r.var("plot_y", PLOT.1 as u64);
    r.end(
        s,
        format!("at {plot:?} (floor {floor:.2}, housing area 52)"),
    );

    // ---- tax quote ----
    let s = r.begin("game.housing.taxquote");
    let mut body = PacketStream::new();
    body.write_u32(DESIGN);
    coord_pos(&mut body, plot.0, plot.1, plot.2);
    game.send_cs(cs::CSConstructHouseTaxPacket, body.as_bytes())
        .await?;
    let answer = game
        .wait_for("SCConstructHouseTaxPacket", |p| {
            p.is_sc(sc::SCConstructHouseTaxPacket) || p.is_sc(sc::SCErrorMsgPacket)
        })
        .await
        .context("CSConstructHouseTax got no answer")?;
    if let Some(code) = error_code(&answer) {
        bail!("CSConstructHouseTax rejected with SCErrorMsg {code}");
    }
    let quote = tax_quote(&answer.body)?;
    ensure!(
        quote.design == DESIGN && quote.total > 0,
        "tax quote {quote:?} (want design {DESIGN} with a positive total)"
    );
    let certificates = construction_certificates(quote.total);
    let reply = gm_command(
        game,
        &format!("item add self {TAX_CERTIFICATE} {certificates}"),
    )
    .await?;
    let certificate = added_stack(&reply, TAX_CERTIFICATE).with_context(|| {
        format!(
            "no {certificates} tax certificates ({TAX_CERTIFICATE}) were added (GM replies {:?})",
            reply
                .iter()
                .filter_map(super::questitem::chat_text)
                .collect::<Vec<_>>()
        )
    })?;
    cleanup.items.push(certificate);
    r.var("tax_total", quote.total as u64);
    r.end(
        s,
        format!(
            "design {DESIGN}, total tax {}, {certificates} certificates",
            quote.total
        ),
    );

    // ---- create at the first spot the server accepts ----
    let s = r.begin("game.housing.create");
    let mut created: Option<(HouseView, Vec<Packet>, Option<u32>)> = None;
    let mut rejected = Vec::new();
    for (dx, dy) in OFFSETS {
        let pos = (plot.0 + dx, plot.1 + dy, plot.2);
        let mut body = PacketStream::new();
        body.write_u32(DESIGN);
        coord_pos(&mut body, pos.0, pos.1, pos.2);
        body.write_f32(0.0) // z rotation
            .write_u64(design_item)
            .write_i32(0) // money amount (tax certificates are used)
            .write_i32(0) // heavy tax
            .write_bool(false); // auto use AA point
        game.send_cs(cs::CSCreateHousePacket, body.as_bytes())
            .await?;
        let answers = create_answers(game, CREATE_ANSWER_WAIT).await?;
        // Whatever the answer says, any house of the bot's the server announced is demolished
        // at the end (also one that is only announced after this step gave up listening).
        cleanup.register_houses(&game.state);
        let code = answers.iter().find_map(error_code);
        let owner = game.state.selected_char;
        let house = answers
            .iter()
            .find(|p| p.is_sc(sc::SCMyHousePacket))
            .and_then(|p| own_house(p, owner));
        let state = answers
            .iter()
            .find(|p| p.is_sc(sc::SCHouseStatePacket))
            .and_then(|p| own_house(p, owner));
        let progress = answers
            .iter()
            .find(|p| p.is_sc(sc::SCHouseBuildProgressPacket))
            .and_then(|p| build_progress(&p.body).ok());
        if let Some(house) = house.or(state) {
            cleanup.add_house(house.obj_id, house.tl);
            created = Some((house, answers, progress.map(|p| u32::from(p.0))));
            break;
        }
        rejected.push(format!("({dx},{dy}): {code:?}"));
        if !matches!(code, Some(INVALID_AREA | OVERLAP_HOUSE | OVERLAP_UNIT)) {
            break;
        }
    }
    let (house, answers, progress_tl) = created.with_context(|| {
        format!(
            "CSCreateHouse was rejected at every spot (last packet: {}; tries {})",
            answers_summary(&rejected),
            rejected.len()
        )
    })?;
    ensure!(
        house.template_id == DESIGN && house.obj_id != 0,
        "created house is {house:?}, want design {DESIGN}"
    );
    ensure!(
        house.owner_id == game.state.selected_char,
        "house owner {} is not the bot character {}",
        house.owner_id,
        game.state.selected_char
    );
    let house_obj = house.obj_id;
    let house_tl = house.tl;
    ensure!(
        answers.iter().any(|p| {
            p.is_sc(sc::SCHouseStatePacket)
                && house_view(&p.body).is_ok_and(|h| h.obj_id == house_obj)
        }),
        "no SCHouseStatePacket for the new house {house_obj}; answers {}",
        packet_summary(&answers)
    );
    ensure!(
        removed_item_id(&answers, design_item),
        "the design item {DESIGN_ITEM} was not consumed: {}",
        packet_summary(&answers)
    );
    ensure!(
        taken_template(&answers, TAX_CERTIFICATE, certificate.item_id),
        "the tax certificates ({TAX_CERTIFICATE}) were not consumed: {}",
        packet_summary(&answers)
    );
    // Both were consumed by the creation: nothing of them is left to destroy.
    cleanup
        .items
        .retain(|item| item.template != DESIGN_ITEM && item.template != TAX_CERTIFICATE);
    r.var("house_obj", house_obj);
    r.var("house_tl", u32::from(house_tl));
    r.end(
        s,
        format!(
            "house obj {house_obj} tl {house_tl} template {} owner {}, SCMyHouse + SCHouseState{}",
            house.template_id,
            house.owner_id,
            progress_tl.map_or(String::new(), |tl| format!(
                " + SCHouseBuildProgress(tl {tl})"
            ))
        ),
    );

    // ---- build steps with the building skills and their materials ----
    let mut all_steps = 0;
    let mut last_step = 0;
    let mut progress_seen = 0;
    for (step, (skill, material, actions)) in BUILD_STEPS.into_iter().enumerate() {
        let s = r.begin(&format!("game.housing.build{}", step + 1));
        move_near(game, house.pos).await?;
        game.change_target(house_obj).await?;
        let mut sent = 0;
        for _ in 0..actions {
            let reply = gm_command(game, &format!("item add self {material} 1")).await?;
            let material_stack = added_stack(&reply, material).with_context(|| {
                format!(
                    "build material {material} could not be added: {:?}",
                    reply
                        .iter()
                        .filter_map(super::questitem::chat_text)
                        .collect::<Vec<_>>()
                )
            })?;
            cleanup.items.push(material_stack);
            game.start_skill(skill, house_obj).await?;
            let caster = game.state.self_obj;
            // The building skills have a 30 s cast time; the skill response, the house progress
            // and the material consumption can arrive in any order.
            let deadline = Instant::now() + Duration::from_secs(70);
            let mut packets = Vec::new();
            let mut answered = false;
            let mut progress = None;
            while Instant::now() < deadline {
                let Some(p) = game.recv_within(Duration::from_millis(250)).await? else {
                    continue;
                };
                if let Some(code) = error_code(&p) {
                    bail!("building skill {skill} rejected with SCErrorMsg {code}");
                }
                if own_skill_response(&p, skill, caster) {
                    let result = skill_response_result(&p)?;
                    ensure!(
                        result == 0,
                        "building skill {skill} answered with SkillResult {result}"
                    );
                    answered = true;
                }
                if p.is_sc(sc::SCHouseBuildProgressPacket) {
                    if let Ok(parsed) = build_progress(&p.body) {
                        if parsed.0 == house_tl {
                            progress = Some(parsed);
                        }
                    }
                }
                packets.push(p);
                if answered && progress.is_some() {
                    break;
                }
            }
            ensure!(
                answered,
                "building skill {skill} got no own SCSkillStarted/Fired"
            );
            // The progress commit charges the material right after the progress packet.
            packets.extend(
                game.drain(Duration::from_millis(300), Duration::from_secs(2))
                    .await?,
            );
            let (tl, _model, all, cur) =
                progress.context(format!("building skill {skill} made no house progress"))?;
            ensure!(
                tl == house_tl,
                "progress for house tl {tl}, want {house_tl}"
            );
            ensure!(
                taken_template(&packets, material, material_stack.item_id),
                "build material {material} was not consumed by skill {skill}: {}",
                packet_summary(&packets)
            );
            // Consumed: only a material of a failed step stays tracked for the cleanup.
            cleanup
                .items
                .retain(|item| item.item_id != material_stack.item_id);
            all_steps = all;
            last_step = cur;
            progress_seen += 1;
            sent += 1;
        }
        r.end(
            s,
            format!(
                "skill {skill} x{sent} (material {material}), progress {last_step}/{all_steps}"
            ),
        );
    }
    ensure!(
        progress_seen >= 1 && last_step == all_steps,
        "house is not finished: progress {last_step}/{all_steps} after {progress_seen} actions"
    );

    // A finished house writes `all_action = current_action = 0`; the re-select gives a fresh
    // owned-house snapshot to assert that state.
    let s = r.begin("game.housing.finished");
    let view = reselect(game, cleanup, house_tl).await?;
    r.end(
        s,
        format!(
            "finished house state tl {house_tl}: actions {}/{}, progress {last_step}/{all_steps}",
            view.current_action, view.all_action
        ),
    );
    // ---- tax info (the 1.2 client cannot prepay tax: `CSChangeHousePayPacket` has no opcode) --
    let s = r.begin("game.housing.taxinfo");
    let mut body = PacketStream::new();
    body.write_u16(house_tl);
    game.send_cs(cs::CSRequestHouseTaxPacket, body.as_bytes())
        .await?;
    let info = game
        .wait_for("SCHouseTaxInfoPacket", |p| {
            p.is_sc(sc::SCHouseTaxInfoPacket) || p.is_sc(sc::SCErrorMsgPacket)
        })
        .await
        .context("CSRequestHouseTax got no answer")?;
    if let Some(code) = error_code(&info) {
        bail!("CSRequestHouseTax rejected with SCErrorMsg {code}");
    }
    let mut st = info.stream();
    let tl = st.read_u16()?;
    ensure!(tl == house_tl, "tax info for tl {tl}, want {house_tl}");
    r.end(s, format!("SCHouseTaxInfo for tl {house_tl}"));

    Ok(())
}

/// The re-select of the finished house: leave to the lobby, select the character again, read the
/// owned-house snapshot the select sends and enter the world. `cleanup.reselect` follows every
/// step, so a failure anywhere (a missing snapshot, a house still reporting build actions) leaves
/// the cleanup exactly the steps still missing and it runs in the world.
async fn reselect(
    game: &mut GameClient,
    cleanup: &mut Cleanup,
    house_tl: u16,
) -> anyhow::Result<HouseView> {
    let char_id = game.state.selected_char;
    game.leave_world(1).await?;
    cleanup.reselect = Some(Reselect::Leaving(char_id));
    // The prepare and the grant are read in whatever order they come.
    await_leave(game, true).await?;
    cleanup.reselect = Some(Reselect::Lobby(char_id));
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    game.list_characters().await?;
    game.wait_for("SCCharacterListPacket", |p| {
        p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
    })
    .await?;
    game.select_character(char_id).await?;
    cleanup.reselect = Some(Reselect::Selected);
    let packet = game
        .wait_for("SCLoginCharInfoHousePacket", |p| {
            p.is_sc(sc::SCLoginCharInfoHousePacket)
                && p.body.len() > 4
                && house_view(&p.body[4..]).is_ok_and(|h| h.tl == house_tl)
        })
        .await
        .context("no SCLoginCharInfoHousePacket for the built house")?;
    let view = house_view(&packet.body[4..])?;
    ensure!(
        view.all_action == 0 && view.current_action == 0,
        "house {house_tl} still reports build actions {}/{} after the last step",
        view.current_action,
        view.all_action
    );
    finish_entry(game, cleanup).await?;
    Ok(view)
}

/// The last steps into the world, from [`Reselect::Selected`] or [`Reselect::Spawned`]: spawn the
/// character, wait for a unit state, send the in-game notices. The state advances after each step
/// and is `None` once the character is in the world.
async fn finish_entry(game: &mut GameClient, cleanup: &mut Cleanup) -> anyhow::Result<()> {
    if cleanup.reselect == Some(Reselect::Selected) {
        game.spawn_character().await?;
        cleanup.reselect = Some(Reselect::Spawned);
        game.wait_for("own SCUnitStatePacket after re-select", |p| {
            p.is_sc(sc::SCUnitStatePacket)
        })
        .await?;
    }
    if cleanup.reselect == Some(Reselect::Spawned) {
        game.notify_in_game().await?;
        game.notify_in_game_completed().await?;
        game.drain(Duration::from_millis(500), Duration::from_secs(4))
            .await?;
        cleanup.reselect = None;
    }
    Ok(())
}

/// How [`create_answers`] listens: the quiet time that ends a burst, the longest single burst and
/// the total time to keep listening when nothing decisive has come.
#[derive(Clone, Copy)]
struct AnswerWait {
    quiet: Duration,
    burst: Duration,
    total: Duration,
}

const CREATE_ANSWER_WAIT: AnswerWait = AnswerWait {
    quiet: Duration::from_millis(1000),
    burst: Duration::from_secs(8),
    total: Duration::from_secs(30),
};

/// The server's answer to a `CSCreateHouse`: the burst that follows it and, when that holds
/// neither a rejection nor a house of the bot's (a loaded server answers late), more bursts until
/// one does or `wait.total` is over.
async fn create_answers(game: &mut GameClient, wait: AnswerWait) -> anyhow::Result<Vec<Packet>> {
    let owner = game.state.selected_char;
    let deadline = Instant::now() + wait.total;
    let mut answers = Vec::new();
    loop {
        answers.extend(game.drain(wait.quiet, wait.burst).await?);
        let decided = answers
            .iter()
            .any(|p| error_code(p).is_some() || own_house(p, owner).is_some());
        if decided || Instant::now() >= deadline {
            return Ok(answers);
        }
    }
}

/// The house in an `SCMyHouse` / `SCHouseState` packet when `owner` owns it.
fn own_house(p: &Packet, owner: u32) -> Option<HouseView> {
    if !(p.is_sc(sc::SCMyHousePacket) || p.is_sc(sc::SCHouseStatePacket)) {
        return None;
    }
    house_view(&p.body).ok().filter(|h| h.owner_id == owner)
}

/// Removes every house of the bot's (finished or not) so the next run can build on the plot
/// again.
async fn cleanup_house(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    let s = r.begin("game.housing.cleanup");
    let reentered = if cleanup.reselect.is_some() {
        reenter_world(game, cleanup).await
    } else {
        Ok(())
    };
    // A house the server announced late is the bot's too: listen once more, then register
    // everything the state knows.
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    cleanup.register_houses(&game.state);
    let mut demolished = Vec::new();
    let mut failed = Vec::new();
    for (house_obj, house_tl) in std::mem::take(&mut cleanup.houses) {
        let (done, texts) = demolish(game, house_obj, house_tl).await?;
        if done {
            demolished.push(format!("house {house_obj} ({texts:?})"));
        } else {
            failed.push(format!("tl {house_tl}: {texts:?}"));
        }
    }
    for item in cleanup.items.drain(..) {
        destroy_item(game, &item).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    let mut house_result: anyhow::Result<()> = Ok(());
    if failed.is_empty() {
        r.end(
            s,
            if demolished.is_empty() {
                "no house to demolish, leftovers destroyed".to_string()
            } else {
                format!("demolished {}, leftovers destroyed", demolished.join(", "))
            },
        );
    } else {
        r.fail(
            s,
            format!(
                "no SCHouseDemolished after /house forcedemo: {}",
                failed.join("; ")
            ),
        );
        house_result = Err(anyhow::anyhow!(
            "house cleanup failed for {}",
            failed.join("; ")
        ));
    }
    reentered?;
    house_result
}

/// How long the cleanup waits for the demolition to be announced after `/house forcedemo`.
const DEMOLISH_PATIENCE: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(10)
};

/// Demolishes one house with the GM command; `true` once its demolition was announced (by the
/// packet, or by the state forgetting a house it knew), with the command's reply lines.
async fn demolish(
    game: &mut GameClient,
    house_obj: u32,
    house_tl: u16,
) -> anyhow::Result<(bool, Vec<String>)> {
    let known = |state: &GameState| state.my_houses.iter().any(|h| h.tl == house_tl);
    let was_known = known(&game.state);
    game.change_target(house_obj).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(1))
        .await?;
    let reply = gm_command(game, "house forcedemo").await?;
    let texts: Vec<String> = reply
        .iter()
        .filter_map(super::questitem::chat_text)
        .collect();
    let is_demolished = |p: &Packet| {
        (p.is_sc(sc::SCHouseDemolishedPacket) || p.is_sc(sc::SCMyHouseRemovedPacket))
            && p.stream().read_u16().ok() == Some(house_tl)
    };
    let deadline = Instant::now() + DEMOLISH_PATIENCE;
    // Whichever wait read the packet, the state forgot the house when it was announced.
    let gone = |game: &GameClient| was_known && !known(&game.state);
    let mut demolished = reply.iter().any(is_demolished) || gone(game);
    while !demolished && Instant::now() < deadline {
        let batch = game
            .drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        demolished = batch.iter().any(is_demolished) || gone(game);
    }
    Ok((demolished, texts))
}

/// Brings the character back into the world after a failure during the re-select, so the cleanup
/// can run the GM demolition: it waits for a requested leave to complete, repeats the lobby steps
/// and finishes the entry from wherever it stopped (`cleanup.reselect`).
async fn reenter_world(game: &mut GameClient, cleanup: &mut Cleanup) -> anyhow::Result<()> {
    let saved = game.timeout;
    let result = reenter_world_steps(game, cleanup).await;
    game.timeout = saved;
    result
}

async fn reenter_world_steps(game: &mut GameClient, cleanup: &mut Cleanup) -> anyhow::Result<()> {
    if let Some(Reselect::Leaving(char_id)) = cleanup.reselect {
        // The server removes the character when the logout timer ends.
        game.timeout = Duration::from_secs(20);
        game.wait_for("SCLeaveWorldGrantedPacket before cleanup", |p| {
            p.is_sc(sc::SCLeaveWorldGrantedPacket)
        })
        .await?;
        cleanup.reselect = Some(Reselect::Lobby(char_id));
    }
    game.timeout = Duration::from_secs(5);
    if let Some(Reselect::Lobby(char_id)) = cleanup.reselect {
        game.drain(Duration::from_millis(300), Duration::from_secs(2))
            .await?;
        game.list_characters().await?;
        game.wait_for("SCCharacterListPacket before cleanup", |p| {
            p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
        })
        .await?;
        game.select_character(char_id).await?;
        cleanup.reselect = Some(Reselect::Selected);
        game.wait_for("SCCharacterStatePacket before cleanup", |p| {
            p.is_sc(sc::SCCharacterStatePacket)
        })
        .await?;
    }
    finish_entry(game, cleanup).await
}

/// Whether an item task consumed `template`: a negative count update or the removal of the
/// exact stack `item_id` this run added (a fully consumed stack is removed by id only).
fn taken_template(packets: &[Packet], template: u32, item_id: u64) -> bool {
    item_stacks(packets, template)
        .iter()
        .any(|stack| stack.count < 0)
        || removed_item_id(packets, item_id)
}

/// Whether an item task removed the exact item id: a `Remove` / `Seize` (design items are
/// removed with `Seize`, which carries only the item id) or a count update that took units away.
/// Entries that add or move the item (`Create`, `Take`, `ChangeOwner`) do not count.
fn removed_item_id(packets: &[Packet], item_id: u64) -> bool {
    packets
        .iter()
        .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
        .any(|p| {
            let mut s = p.stream();
            let parsed = (|| -> anyhow::Result<bool> {
                s.read_u8()?; // task type
                let count = s.read_u8()?;
                for _ in 0..count {
                    match s.read_u8()? {
                        7 => {
                            s.read_u8()?; // slot type
                            s.read_u8()?; // slot
                            let id = s.read_u64()?;
                            s.read_u32()?; // template
                            if id == item_id {
                                return Ok(true);
                            }
                        }
                        13 => {
                            s.read_u8()?; // slot type
                            s.read_u8()?; // slot
                            let id = s.read_u64()?;
                            if id == item_id {
                                return Ok(true);
                            }
                        }
                        4 => {
                            s.read_u8()?;
                            s.read_u8()?;
                            let id = s.read_u64()?;
                            let count = s.read_i32()?;
                            s.read_u32()?;
                            if id == item_id && count < 0 {
                                return Ok(true);
                            }
                        }
                        5 | 6 | 15 => {
                            s.read_u8()?;
                            s.read_u8()?;
                            s.read_u32()?;
                            s.read_u64()?;
                            s.read_u8()?;
                            s.read_u8()?;
                            s.read_i32()?;
                            let len = s.read_i16()?;
                            if len < 0 {
                                anyhow::bail!("negative details");
                            }
                            s.read_bytes(len as usize)?;
                            s.read_i64()?;
                            s.read_i32()?;
                            s.read_u32()?;
                            s.read_u8()?;
                            s.read_i64()?;
                            s.read_i64()?;
                        }
                        _ => anyhow::bail!("unsupported item action"),
                    }
                }
                Ok(false)
            })();
            parsed.unwrap_or(false)
        })
}
/// Walk right next to `pos`.
async fn move_near(game: &mut GameClient, pos: (f32, f32, f32)) -> anyhow::Result<()> {
    game.move_to_facing((pos.0 + 1.2, pos.1, pos.2), pos, 6000)
        .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    Ok(())
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

fn answers_summary(rejected: &[String]) -> String {
    if rejected.is_empty() {
        return "no SCErrorMsg".into();
    }
    rejected.join(", ")
}

#[cfg(test)]
mod tests {
    use aaemu_game::models::item_task::ItemTask;

    use super::super::coreloop::tests::{item, scripted_game, task_packet, Peer};
    use super::*;
    use crate::game::test_packets::{house_body, npc_state_body, packet};
    use crate::record::Recorder;

    const ID: u64 = 9;

    fn changes(task: ItemTask) -> Vec<Packet> {
        vec![task_packet(vec![task])]
    }

    #[test]
    fn an_item_counts_as_removed_by_remove_seize_or_a_negative_count_update() {
        let stack = item(TAX_CERTIFICATE, ID, 30);
        assert!(removed_item_id(&changes(ItemTask::remove(&stack)), ID));
        assert!(removed_item_id(&changes(ItemTask::remove_slot(&stack)), ID));
        assert!(removed_item_id(
            &changes(ItemTask::count_update(&stack, -5)),
            ID
        ));
        // Adding, topping up or moving the stack is not a removal, nor is another item's.
        assert!(!removed_item_id(&changes(ItemTask::add(&stack)), ID));
        assert!(!removed_item_id(
            &changes(ItemTask::count_update(&stack, 5)),
            ID
        ));
        assert!(!removed_item_id(&changes(ItemTask::remove(&stack)), ID + 1));
    }

    #[test]
    fn a_template_is_taken_only_by_a_negative_count_or_the_removal_of_the_added_stack() {
        let stack = item(TAX_CERTIFICATE, ID, 30);
        // The certificates being added (create or top-up) were not consumed.
        let added = changes(ItemTask::add(&stack));
        assert!(!taken_template(&added, TAX_CERTIFICATE, ID));
        let topped_up = changes(ItemTask::count_update(&stack, 30));
        assert!(!taken_template(&topped_up, TAX_CERTIFICATE, ID));
        // A partial or complete consumption was.
        let partial = changes(ItemTask::count_update(&stack, -12));
        assert!(taken_template(&partial, TAX_CERTIFICATE, ID));
        let removed = changes(ItemTask::remove_slot(&stack));
        assert!(taken_template(&removed, TAX_CERTIFICATE, ID));
        // The removal of some other stack proves nothing about this one.
        assert!(!taken_template(&removed, TAX_CERTIFICATE, ID + 1));
    }

    #[test]
    fn the_certificates_match_the_servers_rounding() {
        assert_eq!(construction_certificates(0), 0);
        assert_eq!(construction_certificates(300_000), 30);
        assert_eq!(construction_certificates(300_001), 31);
        assert_eq!(
            construction_certificates(300_001),
            aaemu_game::systems::housing::hooks::construction_certificates(300_001)
        );
    }

    // ---- the way back into the world after a failed re-select ----

    const CHAR: u32 = 42;
    const TL: u16 = 7;
    const HOUSE_OBJ: u32 = 900;

    fn prepare_body() -> Vec<u8> {
        let mut s = PacketStream::new();
        s.write_i32(10_000).write_u8(1).write_bool(false);
        s.into_bytes()
    }

    /// `SCLoginCharInfoHousePacket`: the owner id, then the house.
    fn login_house(all_action: i32, current_action: i32) -> Vec<u8> {
        let mut body = CHAR.to_le_bytes().to_vec();
        body.extend(house_body(
            TL,
            HOUSE_OBJ,
            DESIGN,
            CHAR,
            all_action,
            current_action,
        ));
        body
    }

    fn unit_state() -> Vec<u8> {
        npc_state_body(5, 1, (1.0, 2.0, 3.0))
    }

    /// The server's side of `leave_world(1)`, the lobby steps and the select, up to the house
    /// snapshot it answers the select with.
    async fn serve_reselect(peer: &mut Peer, snapshot: Vec<u8>) {
        peer.expect(cs::CSLeaveWorldPacket).await;
        peer.send(sc::SCPrepareLeaveWorldPacket, &prepare_body())
            .await;
        peer.send(sc::SCLeaveWorldGrantedPacket, &[1]).await;
        peer.expect(cs::CSListCharacterPacket).await;
        peer.send(sc::SCCharacterListPacket, &[1, 0]).await;
        peer.expect(cs::CSSelectCharacterPacket).await;
        peer.send(sc::SCLoginCharInfoHousePacket, &snapshot).await;
    }

    const ENTRY: [u16; 3] = [
        cs::CSSpawnCharacterPacket,
        cs::CSNotifyInGamePacket,
        cs::CSNotifyInGameCompletedPacket,
    ];
    const LEAVE_AND_SELECT: [u16; 3] = [
        cs::CSLeaveWorldPacket,
        cs::CSListCharacterPacket,
        cs::CSSelectCharacterPacket,
    ];

    #[tokio::test]
    async fn a_failure_after_the_select_leaves_the_cleanup_the_spawn_and_the_notices() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let server = tokio::spawn(async move {
            // Injected failure: the snapshot still reports build actions.
            serve_reselect(&mut peer, login_house(2, 1)).await;
            // The cleanup must finish the entry (no second select, no early GM command).
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.send(sc::SCUnitStatePacket, &unit_state()).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer
        });
        let mut cleanup = Cleanup::default();
        let error = reselect(&mut game, &mut cleanup, TL).await.unwrap_err();
        assert!(
            error.to_string().contains("still reports build actions"),
            "{error:#}"
        );
        // The old code recorded `Selected` as "in the world" and ran the demolition without
        // spawning; the state must now say what is missing.
        assert_eq!(cleanup.reselect, Some(Reselect::Selected));
        reenter_world(&mut game, &mut cleanup).await.unwrap();
        assert_eq!(cleanup.reselect, None);
        let peer = server.await.unwrap();
        assert_eq!(peer.received, [LEAVE_AND_SELECT, ENTRY].concat());
        // The wait limits are the caller's again.
        assert_eq!(game.timeout, Duration::from_millis(400));
    }

    #[tokio::test]
    async fn a_failure_after_the_spawn_leaves_the_cleanup_only_the_notices() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let server = tokio::spawn(async move {
            serve_reselect(&mut peer, login_house(0, 0)).await;
            // Injected failure: the spawn gets no unit state.
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer
        });
        let mut cleanup = Cleanup::default();
        let error = reselect(&mut game, &mut cleanup, TL).await.unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        assert_eq!(cleanup.reselect, Some(Reselect::Spawned));
        reenter_world(&mut game, &mut cleanup).await.unwrap();
        assert_eq!(cleanup.reselect, None);
        let peer = server.await.unwrap();
        // One spawn only: the cleanup did not spawn the character twice.
        assert_eq!(peer.received, [LEAVE_AND_SELECT, ENTRY].concat());
    }

    #[tokio::test]
    async fn a_successful_reselect_returns_the_snapshot_and_leaves_nothing_to_finish() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let server = tokio::spawn(async move {
            serve_reselect(&mut peer, login_house(0, 0)).await;
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.send(sc::SCUnitStatePacket, &unit_state()).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer
        });
        let mut cleanup = Cleanup::default();
        let view = reselect(&mut game, &mut cleanup, TL).await.unwrap();
        assert_eq!((view.tl, view.all_action, view.current_action), (TL, 0, 0));
        assert_eq!(cleanup.reselect, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_cleanup_from_the_lobby_selects_again_and_enters() {
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSListCharacterPacket).await;
            peer.send(sc::SCCharacterListPacket, &[1, 0]).await;
            peer.expect(cs::CSSelectCharacterPacket).await;
            peer.send(sc::SCCharacterStatePacket, &[]).await;
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.send(sc::SCUnitStatePacket, &unit_state()).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer
        });
        let mut cleanup = Cleanup {
            reselect: Some(Reselect::Lobby(CHAR)),
            ..Cleanup::default()
        };
        reenter_world(&mut game, &mut cleanup).await.unwrap();
        assert_eq!(cleanup.reselect, None);
        assert_eq!(game.state.selected_char, CHAR);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_cleanup_of_a_pending_leave_waits_for_the_grant_first() {
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.send(sc::SCLeaveWorldGrantedPacket, &[1]).await;
            peer.expect(cs::CSListCharacterPacket).await;
            peer.send(sc::SCCharacterListPacket, &[1, 0]).await;
            peer.expect(cs::CSSelectCharacterPacket).await;
            peer.send(sc::SCCharacterStatePacket, &[]).await;
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.send(sc::SCUnitStatePacket, &unit_state()).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer
        });
        let mut cleanup = Cleanup {
            reselect: Some(Reselect::Leaving(CHAR)),
            ..Cleanup::default()
        };
        reenter_world(&mut game, &mut cleanup).await.unwrap();
        assert_eq!(cleanup.reselect, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_cleanup_demolishes_the_house_only_once_the_character_is_in_the_world() {
        // The house was built, the re-select failed after the select: the GM command is only
        // accepted once the entry is complete (the fixture checks the order of the packets).
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let server = tokio::spawn(async move {
            peer.expect(cs::CSSpawnCharacterPacket).await;
            peer.send(sc::SCUnitStatePacket, &unit_state()).await;
            peer.expect(cs::CSNotifyInGamePacket).await;
            peer.expect(cs::CSNotifyInGameCompletedPacket).await;
            peer.expect(cs::CSChangeTargetPacket).await;
            peer.expect(cs::CSSendChatMessagePacket).await;
            peer.send(sc::SCHouseDemolishedPacket, &TL.to_le_bytes())
                .await;
            peer
        });
        let rec = Recorder::new(false);
        let mut runner = Runner {
            rec: &rec,
            out: Default::default(),
        };
        let mut cleanup = Cleanup {
            reselect: Some(Reselect::Selected),
            ..Cleanup::default()
        };
        cleanup.add_house(HOUSE_OBJ, TL);
        cleanup_house(&mut game, &mut runner, &mut cleanup)
            .await
            .unwrap();
        assert!(runner.out.ok(), "{:?}", runner.out.steps);
        assert!(cleanup.houses.is_empty() && cleanup.reselect.is_none());
        let note = &runner.out.steps.last().unwrap().note;
        assert!(note.contains("demolished house 900"), "{note}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_demolition_another_wait_already_read_still_counts() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        // The state knows the house (SCMyHouse), then the server demolishes it while the bot
        // is still waiting after the target change, before the command's own reply is read.
        game.state
            .observe(&packet(
                sc::SCMyHousePacket,
                house_body(TL, HOUSE_OBJ, DESIGN, CHAR, 0, 0),
            ))
            .unwrap();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSChangeTargetPacket).await;
            peer.send(sc::SCHouseDemolishedPacket, &TL.to_le_bytes())
                .await;
            peer.expect(cs::CSSendChatMessagePacket).await;
            peer
        });
        let (demolished, _) = demolish(&mut game, HOUSE_OBJ, TL).await.unwrap();
        assert!(demolished);
        assert!(game.state.my_houses.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_house_nobody_announced_demolished_is_a_failed_cleanup() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let server = tokio::spawn(async move {
            peer.expect(cs::CSChangeTargetPacket).await;
            peer.expect(cs::CSSendChatMessagePacket).await;
            peer
        });
        let rec = Recorder::new(false);
        let mut runner = Runner {
            rec: &rec,
            out: Default::default(),
        };
        let mut cleanup = Cleanup::default();
        cleanup.add_house(HOUSE_OBJ, TL);
        let error = cleanup_house(&mut game, &mut runner, &mut cleanup)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("house cleanup failed"),
            "{error:#}"
        );
        assert!(!runner.out.ok());
        server.await.unwrap();
    }

    // ---- houses the server announces late ----

    #[test]
    fn every_house_of_the_character_the_state_knows_is_registered_once() {
        let mut state = GameState {
            selected_char: CHAR,
            ..GameState::default()
        };
        let mut cleanup = Cleanup::default();
        cleanup.register_houses(&state);
        assert!(cleanup.houses.is_empty());
        for (tl, obj, owner) in [(7, 900, CHAR), (8, 901, 43)] {
            state
                .observe(&packet(
                    sc::SCMyHousePacket,
                    house_body(tl, obj, DESIGN, owner, 0, 0),
                ))
                .unwrap();
        }
        cleanup.register_houses(&state);
        cleanup.register_houses(&state);
        // Only the bot's own house, and not twice.
        assert_eq!(cleanup.houses, [(900, 7)]);
        // A house announced after the first registration (a late SCMyHouse) joins it.
        state
            .observe(&packet(
                sc::SCMyHousePacket,
                house_body(9, 950, DESIGN, CHAR, 0, 0),
            ))
            .unwrap();
        cleanup.register_houses(&state);
        assert_eq!(cleanup.houses, [(900, 7), (950, 9)]);
        cleanup.add_house(950, 9);
        assert_eq!(cleanup.houses.len(), 2);
    }

    #[tokio::test]
    async fn a_house_answered_after_the_first_quiet_period_is_still_found() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let wait = AnswerWait {
            quiet: Duration::from_millis(150),
            burst: Duration::from_millis(600),
            total: Duration::from_secs(5),
        };
        let server = tokio::spawn(async move {
            // The server is slow: the house arrives after the first burst ended quiet.
            tokio::time::sleep(Duration::from_millis(400)).await;
            peer.send(
                sc::SCMyHousePacket,
                &house_body(TL, HOUSE_OBJ, DESIGN, CHAR, 3, 3),
            )
            .await;
            peer
        });
        let answers = create_answers(&mut game, wait).await.unwrap();
        let house = answers
            .iter()
            .find_map(|p| own_house(p, CHAR))
            .expect("the late house is in the answers");
        assert_eq!((house.tl, house.obj_id), (TL, HOUSE_OBJ));
        // ... and the state registered it for the cleanup whoever read the packet.
        let mut cleanup = Cleanup::default();
        cleanup.register_houses(&game.state);
        assert_eq!(cleanup.houses, [(HOUSE_OBJ, TL)]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_rejection_ends_the_wait_for_the_house_at_once() {
        let (mut game, mut peer) = scripted_game();
        game.state.selected_char = CHAR;
        let wait = AnswerWait {
            quiet: Duration::from_millis(100),
            burst: Duration::from_millis(500),
            total: Duration::from_secs(30),
        };
        let server = tokio::spawn(async move {
            peer.send(sc::SCErrorMsgPacket, &OVERLAP_HOUSE.to_le_bytes())
                .await;
            peer
        });
        let started = Instant::now();
        let answers = create_answers(&mut game, wait).await.unwrap();
        assert_eq!(answers.iter().find_map(error_code), Some(OVERLAP_HOUSE));
        assert!(started.elapsed() < Duration::from_secs(5));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_answer_at_all_ends_after_the_total_wait() {
        let (mut game, _peer) = scripted_game();
        let wait = AnswerWait {
            quiet: Duration::from_millis(50),
            burst: Duration::from_millis(100),
            total: Duration::from_millis(300),
        };
        let started = Instant::now();
        let answers = create_answers(&mut game, wait).await.unwrap();
        assert!(answers.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(300));
    }
}

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

use super::coreloop::{floor_height, gm_access, house_view, teleport_to, HouseView};
use super::questitem::{gm_command, stack_id};
use super::{skill_response_result, Runner, QUIET};
use crate::game::{GameClient, Packet};

/// Design 172 "cozy Nuian house" and its design item.
const DESIGN: u32 = 172;
const DESIGN_ITEM: u32 = 21166;
/// Placement tax is paid with bound tax certificates (Feature `taxItem` is on by default).
const TAX_CERTIFICATE: u32 = 31892;
const TAX_CERTIFICATES: u32 = 500;
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

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    gm_access(game, r).await?;

    // ---- design, build materials, tax certificates ----
    let s = r.begin("game.housing.items");
    let reply = gm_command(game, &format!("item add self {DESIGN_ITEM} 1")).await?;
    let design_item = stack_id(&reply, DESIGN_ITEM).with_context(|| {
        format!(
            "no design item {DESIGN_ITEM} was added (GM replies {:?})",
            reply
                .iter()
                .filter_map(super::questitem::chat_text)
                .collect::<Vec<_>>()
        )
    })?;
    gm_command(
        game,
        &format!("item add self {TAX_CERTIFICATE} {TAX_CERTIFICATES}"),
    )
    .await?;
    // Three build actions cost 25 labor each. The build materials are tradepacks that occupy the
    // single backpack slot, so each one is added right before the step that consumes it.
    gm_command(game, "labor 200").await?;
    r.var("design_item_id", design_item);
    r.end(
        s,
        format!("design item {design_item} ({DESIGN_ITEM}), {TAX_CERTIFICATES} tax certificates"),
    );

    // ---- a free spot on the housing plot ----
    let s = r.begin("game.housing.teleport");
    let start = (PLOT.0, PLOT.1, 120.0);
    teleport_to(game, start).await?;
    let floor = floor_height(game).await.unwrap_or(120.0);
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
    r.var("tax_total", quote.total as u64);
    r.end(s, format!("design {DESIGN}, total tax {}", quote.total));

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
        let answers = game
            .drain(Duration::from_millis(1000), Duration::from_secs(8))
            .await?;
        let code = answers.iter().find_map(error_code);
        let house = answers
            .iter()
            .find(|p| p.is_sc(sc::SCMyHousePacket))
            .and_then(|p| house_view(&p.body).ok());
        let state = answers
            .iter()
            .find(|p| p.is_sc(sc::SCHouseStatePacket))
            .and_then(|p| house_view(&p.body).ok());
        let progress = answers
            .iter()
            .find(|p| p.is_sc(sc::SCHouseBuildProgressPacket))
            .and_then(|p| build_progress(&p.body).ok());
        if let Some(house) = house.or(state) {
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
    ensure!(
        answers.iter().any(|p| p.is_sc(sc::SCHouseStatePacket)),
        "no SCHouseStatePacket for the new house; answers {}",
        packet_summary(&answers)
    );
    let house_obj = house.obj_id;
    let house_tl = house.tl;
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
            ensure!(
                stack_id(&reply, material).is_some(),
                "build material {material} could not be added: {:?}",
                reply
                    .iter()
                    .filter_map(super::questitem::chat_text)
                    .collect::<Vec<_>>()
            );
            game.start_skill(skill, house_obj).await?;
            let answer = game
                .wait_for("building skill response", |p| {
                    p.is_sc(sc::SCSkillStartedPacket)
                        || p.is_sc(sc::SCSkillFiredPacket)
                        || p.is_sc(sc::SCErrorMsgPacket)
                })
                .await
                .with_context(|| format!("building skill {skill} got no answer"))?;
            if let Some(code) = error_code(&answer) {
                bail!("building skill {skill} rejected with SCErrorMsg {code}");
            }
            let result = skill_response_result(&answer)?;
            ensure!(
                result == 0,
                "building skill {skill} answered with SkillResult {result}"
            );
            // The building skills have a 30 s cast time.
            let saved = game.timeout;
            game.timeout = Duration::from_secs(50);
            let progress = game
                .wait_for("SCHouseBuildProgressPacket", |p| {
                    p.is_sc(sc::SCHouseBuildProgressPacket)
                })
                .await;
            game.timeout = saved;
            let progress = progress
                .with_context(|| format!("building skill {skill} made no house progress"))?;
            let (tl, _model, all, cur) = build_progress(&progress.body)?;
            ensure!(
                tl == house_tl,
                "progress for house tl {tl}, want {house_tl}"
            );
            all_steps = all;
            last_step = cur;
            progress_seen += 1;
            sent += 1;
            game.drain(Duration::from_millis(300), Duration::from_secs(2))
                .await?;
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

    let s = r.begin("game.housing.finished");
    let extra = game.drain(QUIET, Duration::from_secs(6)).await?;
    let done = answers
        .iter()
        .chain(extra.iter())
        .any(|p| p.is_sc(sc::SCDoodadCreatedPacket) || p.is_sc(sc::SCDoodadsCreatedPacket));
    r.end(
        s,
        format!(
            "build step -1 at {last_step}/{all_steps}; attached house doodads announced: {done}"
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

    // ---- demolition so the plot stays free ----
    let s = r.begin("game.housing.demolish");
    game.change_target(house_obj).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(1))
        .await?;
    let reply = gm_command(game, "house forcedemo").await?;
    let texts: Vec<String> = reply
        .iter()
        .filter_map(super::questitem::chat_text)
        .collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    let is_demolished = |p: &Packet| {
        (p.is_sc(sc::SCHouseDemolishedPacket) || p.is_sc(sc::SCMyHouseRemovedPacket))
            && p.stream().read_u16().ok() == Some(house_tl)
    };
    let mut demolished = reply.iter().any(is_demolished);
    while !demolished && Instant::now() < deadline {
        let batch = game
            .drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        demolished = batch.iter().any(is_demolished);
    }
    ensure!(
        demolished,
        "no SCHouseDemolished(tl {house_tl}) after /house forcedemo: {texts:?}"
    );
    r.end(s, format!("house {house_obj} demolished ({texts:?})"));

    let s = r.begin("game.leave");
    super::coreloop::leave_world(game).await?;
    r.end(s, "left");
    Ok(())
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

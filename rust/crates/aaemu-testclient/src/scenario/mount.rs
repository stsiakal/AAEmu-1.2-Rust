//! `mount`: summon a mount, ride it, move while mounted, dismount and dismiss it, asserting the
//! mate packets and the mounted movement (GM account needed for `/item add`).
//!
//! Setup adds the brown Lilliot horse whistle (item 8159) and summons it with the item's use skill
//! 10602 (`CSStartSkill` with an item caster, the client's summon). `CSMountMate` takes the driver
//! seat (attach point 1), `CSMoveUnit` with the mate's object id moves the mount (a faster step
//! than the walking baseline sent just before), `CSUnMountMate` leaves the seat and
//! `CSRemoveMate` dismisses it.

use std::time::Duration;

use aaemu_game::opcodes::{cs, sc};
use anyhow::{ensure, Context};

use super::coreloop::{attached, detached, gm_access};
use super::questitem::gm_command;
use super::{summon_mate, BotConfig, Runner};
use crate::game::GameClient;

/// `AttachPointKind.Driver`.
const DRIVER: u8 = 1;
/// The reasons the real client uses for mount / dismount.
const MOUNT_REASON: u8 = 1;
const DISMOUNT_REASON: u8 = 4;

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    gm_access(game, r).await?;

    // ---- the summon item ----
    let s = r.begin("game.mount.item");
    let reply = gm_command(game, &format!("item add self {} 1", cfg.mate_item_template)).await?;
    let stack = super::questitem::stack_id(&reply, cfg.mate_item_template).with_context(|| {
        format!(
            "no item {} was added: the account needs GM access (replies {:?})",
            cfg.mate_item_template,
            reply
                .iter()
                .filter_map(super::questitem::chat_text)
                .collect::<Vec<_>>()
        )
    })?;
    r.var("mount_item_id", stack);
    r.end(
        s,
        format!("item {stack} (template {})", cfg.mate_item_template),
    );

    // ---- walking baseline ----
    let s = r.begin("game.mount.walk");
    let start = game.state.self_pos;
    let walk_to = (start.0 + 10.0, start.1, start.2);
    game.move_to(walk_to, 2500).await?;
    let self_obj = game.state.self_obj;
    game.wait_for("own walking movement echo", |p| {
        p.is_sc(sc::SCOneUnitMovementPacket) && super::packet_obj_id(p).ok() == Some(self_obj)
    })
    .await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    let walked = game
        .state
        .observed_self_pos
        .context("no walking position echo")?;
    let walk_rate = dist(start, walked) / 2.5;
    r.end(
        s,
        format!(
            "walked {:.1} m at {walk_rate:.1} m/s to {walked:?}",
            dist(start, walked)
        ),
    );

    // ---- summon ----
    let s = r.begin("game.mount.summon");
    let mate = summon_mate(game, cfg, stack, None).await?;
    ensure!(!mate.dead && mate.hp > 0, "summoned mate is dead: {mate:?}");
    let mate_obj = mate.obj_id;
    let tl = mate.tl_id;
    r.var("mate_obj", mate_obj);
    r.var("mate_tl", u32::from(tl));
    r.end(
        s,
        format!(
            "mate obj {mate_obj} tl {tl} hp {} (SCMateSpawned + SCUnitState + SCUnitPoints)",
            mate.hp
        ),
    );

    // ---- mount ----
    let s = r.begin("game.mount.ride");
    let mut body = aaemu_commons::PacketStream::new();
    body.write_u16(tl).write_u8(DRIVER).write_u8(MOUNT_REASON);
    game.send_cs(cs::CSMountMatePacket, body.as_bytes()).await?;
    let self_obj = game.state.self_obj;
    let packet = game
        .wait_for("SCUnitAttachedPacket", |p| {
            p.is_sc(sc::SCUnitAttachedPacket)
        })
        .await
        .context("CSMountMate got no SCUnitAttachedPacket")?;
    let (child, point, parent, reason) = attached(&packet)?;
    ensure!(
        child == self_obj && point == DRIVER && parent == mate_obj,
        "SCUnitAttached has child {child}, point {point}, parent {parent}, reason {reason}; \
         want child {self_obj}, point {DRIVER}, parent {mate_obj}"
    );
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    r.end(
        s,
        format!("attached child {child} at point {point} to mate {parent} (reason {reason})"),
    );

    // ---- move while mounted: a mounted step the walking baseline cannot reach ----
    let s = r.begin("game.mount.move");
    let from = game.state.units.get(&mate_obj).map_or(start, |u| u.pos);
    let mounted_to = (from.0 + 20.0, from.1, from.2);
    game.move_obj_to(mate_obj, mounted_to, 1000).await?;
    game.wait_for("mounted movement echo", |p| {
        p.is_sc(sc::SCOneUnitMovementPacket) && super::packet_obj_id(p).ok() == Some(mate_obj)
    })
    .await?;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    let reached = game
        .state
        .units
        .get(&mate_obj)
        .map(|u| u.pos)
        .context("the mount vanished while moving")?;
    let moved = dist(from, reached);
    let ride_rate = moved / 1.0;
    ensure!(
        (reached.0 - mounted_to.0).abs() <= 1.0 && (reached.1 - mounted_to.1).abs() <= 1.0,
        "server did not apply the mounted move: mate at {reached:?}, asked {mounted_to:?}"
    );
    ensure!(
        ride_rate > walk_rate * 2.0,
        "mounted step only reached {ride_rate:.1} m/s, walking was {walk_rate:.1} m/s"
    );
    r.end(
        s,
        format!(
            "mate moved {moved:.1} m at {ride_rate:.1} m/s (walking {walk_rate:.1} m/s), \
             rider followed at {reached:?}"
        ),
    );

    // ---- dismount ----
    let s = r.begin("game.mount.dismount");
    let mut body = aaemu_commons::PacketStream::new();
    body.write_u16(tl)
        .write_u8(DRIVER)
        .write_u8(DISMOUNT_REASON);
    game.send_cs(cs::CSUnMountMatePacket, body.as_bytes())
        .await?;
    let packet = game
        .wait_for("SCUnitDetachedPacket", |p| {
            p.is_sc(sc::SCUnitDetachedPacket)
        })
        .await
        .context("CSUnMountMate got no SCUnitDetachedPacket")?;
    let (child, reason) = detached(&packet)?;
    ensure!(
        child == self_obj && reason == DISMOUNT_REASON,
        "SCUnitDetached has child {child}, reason {reason}; want child {self_obj}, reason \
         {DISMOUNT_REASON}"
    );
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    r.end(s, format!("detached child {child} (reason {reason})"));

    // ---- dismiss ----
    let s = r.begin("game.mount.dismiss");
    game.remove_mate(tl).await?;
    game.drain(Duration::from_millis(800), Duration::from_secs(3))
        .await?;
    r.end(s, format!("CSRemoveMate({tl}) sent"));

    let s = r.begin("game.leave");
    super::coreloop::leave_world(game).await?;
    r.end(s, "left");
    Ok(())
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

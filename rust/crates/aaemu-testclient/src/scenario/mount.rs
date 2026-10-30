//! `mount`: summon a mount, ride it, move while mounted, dismount and dismiss it, asserting the
//! mate packets and that the rider follows the mount (GM account needed for `/item add`).
//!
//! Setup adds the brown Lilliot horse whistle (item 8159) and summons it with the item's use skill
//! 10602 (`CSStartSkill` with an item caster, the client's summon). `CSMountMate` takes the driver
//! seat (attach point 1), `CSMoveUnit` with the mate's object id moves the mount, the server's
//! `/position` read must place the rider at the mount, `CSUnMountMate` leaves the seat and
//! `CSRemoveMate` dismisses it (`SCUnitsRemovedPacket`).

use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::{ensure, Context};

use super::coreloop::{
    attached, detached, gm_access, item_stacks, removed_ids, self_position, teleport_to, ItemStack,
};
use super::questitem::gm_command;
use super::{summon_mate, BotConfig, Runner};
use crate::game::GameClient;

/// `AttachPointKind.Driver`.
const DRIVER: u8 = 1;
/// The reasons the real client uses for mount / dismount.
const MOUNT_REASON: u8 = 1;
const DISMOUNT_REASON: u8 = 4;

#[derive(Default)]
struct Cleanup {
    /// Where the bot stood before the mount carried it 20 m away; it goes back there at the end.
    home: Option<(f32, f32, f32)>,
    mate: Option<(u32, u16)>,
    mounted: bool,
    item: Option<ItemStack>,
}

pub(super) async fn run(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    gm_access(game, r).await?;
    let mut cleanup = Cleanup::default();
    let result = run_inner(game, cfg, r, &mut cleanup).await;
    let cleanup_result = cleanup_mount(game, r, &mut cleanup).await;
    let leave = super::coreloop::leave_world(game).await;
    result?;
    cleanup_result?;
    leave?;
    Ok(())
}

async fn run_inner(
    game: &mut GameClient,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    cleanup.home = Some(game.state.self_pos);

    // ---- the summon item ----
    let s = r.begin("game.mount.item");
    let reply = gm_command(game, &format!("item add self {} 1", cfg.mate_item_template)).await?;
    let item = item_stacks(&reply, cfg.mate_item_template)
        .into_iter()
        .next()
        .or_else(|| {
            super::questitem::stack_location(&reply, cfg.mate_item_template).map(
                |(id, slot_type, slot)| ItemStack {
                    template: cfg.mate_item_template,
                    item_id: id,
                    slot_type,
                    slot,
                    count: 1,
                },
            )
        })
        .with_context(|| {
            format!(
                "no item {} was added: the account needs GM access (replies {:?})",
                cfg.mate_item_template,
                reply
                    .iter()
                    .filter_map(super::questitem::chat_text)
                    .collect::<Vec<_>>()
            )
        })?;
    cleanup.item = Some(item);
    let stack = item.item_id;
    r.var("mount_item_id", stack);
    r.end(
        s,
        format!("item {stack} (template {})", cfg.mate_item_template),
    );

    // ---- summon ----
    let s = r.begin("game.mount.summon");
    let mate = summon_mate(game, cfg, stack, None).await?;
    ensure!(!mate.dead && mate.hp > 0, "summoned mate is dead: {mate:?}");
    let mate_obj = mate.obj_id;
    let tl = mate.tl_id;
    cleanup.mate = Some((mate_obj, tl));
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
    let mut body = PacketStream::new();
    body.write_u16(tl).write_u8(DRIVER).write_u8(MOUNT_REASON);
    game.send_cs(cs::CSMountMatePacket, body.as_bytes()).await?;
    let self_obj = game.state.self_obj;
    let packet = game
        .wait_for("SCUnitAttachedPacket", |p| {
            p.is_sc(sc::SCUnitAttachedPacket)
                && attached(p)
                    .is_ok_and(|(child, _, parent, _)| child == self_obj && parent == mate_obj)
        })
        .await
        .context("CSMountMate got no own SCUnitAttachedPacket")?;
    let (child, point, parent, reason) = attached(&packet)?;
    ensure!(
        child == self_obj && point == DRIVER && parent == mate_obj,
        "SCUnitAttached has child {child}, point {point}, parent {parent}, reason {reason}; \
         want child {self_obj}, point {DRIVER}, parent {mate_obj}"
    );
    cleanup.mounted = true;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    r.end(
        s,
        format!("attached child {child} at point {point} to mate {parent} (reason {reason})"),
    );

    // ---- move while mounted; the rider is a child of the mount and must follow it ----
    let s = r.begin("game.mount.move");
    let from = game
        .state
        .units
        .get(&mate_obj)
        .map_or(game.state.self_pos, |u| u.pos);
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
    ensure!(
        (reached.0 - mounted_to.0).abs() <= 1.0 && (reached.1 - mounted_to.1).abs() <= 1.0,
        "server did not apply the mounted move: mate at {reached:?}, asked {mounted_to:?}"
    );
    let rider = self_position(game).await?;
    ensure!(
        dist(rider, reached) <= 1.0,
        "the rider did not follow the mount: rider at {rider:?}, mate at {reached:?}"
    );
    r.end(
        s,
        format!(
            "mate moved {:.1} m to {reached:?}; rider at {rider:?}",
            dist(from, reached)
        ),
    );

    // ---- dismount ----
    let s = r.begin("game.mount.dismount");
    let mut body = PacketStream::new();
    body.write_u16(tl)
        .write_u8(DRIVER)
        .write_u8(DISMOUNT_REASON);
    game.send_cs(cs::CSUnMountMatePacket, body.as_bytes())
        .await?;
    let packet = game
        .wait_for("SCUnitDetachedPacket", |p| {
            p.is_sc(sc::SCUnitDetachedPacket)
                && detached(p).is_ok_and(|(child, _)| child == self_obj)
        })
        .await
        .context("CSUnMountMate got no own SCUnitDetachedPacket")?;
    let (child, reason) = detached(&packet)?;
    ensure!(
        child == self_obj && reason == DISMOUNT_REASON,
        "SCUnitDetached has child {child}, reason {reason}; want child {self_obj}, reason\
         {DISMOUNT_REASON}"
    );
    cleanup.mounted = false;
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    let rider = self_position(game).await?;
    ensure!(
        dist(rider, reached) <= 1.0,
        "the rider did not stand at the mount after dismount: rider {rider:?}, mate {reached:?}"
    );
    r.end(
        s,
        format!("detached child {child} (reason {reason}), rider at {rider:?}"),
    );

    // ---- dismiss ----
    let s = r.begin("game.mount.dismiss");
    game.remove_mate(tl).await?;
    let removed = wait_mate_removed(game, mate_obj).await?;
    ensure!(
        !game.state.units.contains_key(&mate_obj),
        "mate {mate_obj} is still in the unit map after SCUnitsRemoved"
    );
    cleanup.mate = None;
    r.end(
        s,
        format!("SCUnitsRemoved for mate {mate_obj}: {removed:?}"),
    );

    Ok(())
}

/// Waits for the removal packet that names `mate_obj`.
async fn wait_mate_removed(game: &mut GameClient, mate_obj: u32) -> anyhow::Result<Vec<u32>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Some(p) = game.recv_within(Duration::from_millis(250)).await? {
            if p.is_sc(sc::SCUnitsRemovedPacket) {
                let ids = removed_ids(&p)?;
                seen.extend(ids.iter().copied());
                if ids.contains(&mate_obj) {
                    return Ok(ids);
                }
            }
        }
    }
    anyhow::bail!("no SCUnitsRemovedPacket naming mate {mate_obj} (saw {seen:?})")
}

/// Dismount / dismiss / item cleanup, also after a failed run.
async fn cleanup_mount(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    cleanup: &mut Cleanup,
) -> anyhow::Result<()> {
    let s = r.begin("game.mount.cleanup");
    if let Some((mate_obj, tl)) = cleanup.mate {
        if cleanup.mounted {
            let mut body = PacketStream::new();
            body.write_u16(tl)
                .write_u8(DRIVER)
                .write_u8(DISMOUNT_REASON);
            game.send_cs(cs::CSUnMountMatePacket, body.as_bytes())
                .await?;
            let self_obj = game.state.self_obj;
            let _ = game
                .wait_for("cleanup SCUnitDetachedPacket", |p| {
                    p.is_sc(sc::SCUnitDetachedPacket)
                        && detached(p).is_ok_and(|(child, _)| child == self_obj)
                })
                .await;
        }
        game.remove_mate(tl).await?;
        let _ = wait_mate_removed(game, mate_obj).await;
    }
    if let Some(item) = cleanup.item.take() {
        super::coreloop::destroy_item(game, &item).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    // Leave the spot where the run began, so repeated runs do not carry the bot away.
    if let Some(home) = cleanup.home.take() {
        teleport_to(game, home).await?;
    }
    r.end(s, "mount dismissed, summon item destroyed, bot back home");
    Ok(())
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
}

//! Shared helpers of the core-loop scenarios (`housing`, `mount`, `questchain`, `gather`).

use std::time::Duration;

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::sc;
use anyhow::{ensure, Context};

use super::questitem::{chat_text, gm_command};
use super::{Runner, QUIET};
use crate::game::{GameClient, Packet};

/// Assert that the bot's account may run GM setup commands.
pub(super) async fn gm_access(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.gm");
    let reply = gm_command(game, "quest list").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    ensure!(!texts.is_empty(), "no reply to the GM command /quest list");
    gm_command(game, "revive").await?;
    r.end(s, "GM commands accepted");
    Ok(())
}

/// Teleport with the GM `/move` command and complete the client handshake (the server only
/// adopts the position from the client's `CSTeleportEnded` + movement packets).
pub(super) async fn teleport_to(game: &mut GameClient, pos: (f32, f32, f32)) -> anyhow::Result<()> {
    game.chat(0, &format!("/move {} {} {}", pos.0, pos.1, pos.2))
        .await?;
    game.wait_for("SCTeleportUnitPacket", |p| {
        p.is_sc(sc::SCTeleportUnitPacket)
    })
    .await?;
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    game.move_to(at, 5000).await?;
    game.drain(QUIET, Duration::from_secs(8)).await?;
    Ok(())
}

/// The heightmap floor below the bot (`/height`), parsed from its system chat line.
pub(super) async fn floor_height(game: &mut GameClient) -> anyhow::Result<f32> {
    let reply = gm_command(game, "height").await?;
    let line = reply
        .iter()
        .filter_map(chat_text)
        .find(|t| t.contains("Floor:"))
        .context("no /height reply with a floor height")?;
    let after = line
        .split("Floor:")
        .nth(1)
        .context("malformed /height line")?;
    let value = after
        .trim_start()
        .split([',', ' '])
        .next()
        .context("missing floor value")?;
    value
        .parse()
        .with_context(|| format!("floor height in {line:?} is not a number"))
}

/// `SCCharacterLaborPowerChangedPacket` amount (negative = spent).
pub(super) fn labor_change(p: &Packet) -> Option<i32> {
    if !p.is_sc(sc::SCCharacterLaborPowerChangedPacket) {
        return None;
    }
    p.stream().read_i32().ok()
}

/// The `SCHouseStatePacket` / `SCMyHousePacket` body (`House.Write`), reduced to the fields the
/// scenarios assert.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HouseView {
    pub tl: u16,
    pub obj_id: u32,
    pub template_id: u32,
    pub co_owner_id: u32,
    pub owner_id: u32,
    pub permission: u8,
    pub all_action: i32,
    pub current_action: i32,
    pub pos: (f32, f32, f32),
    pub name: String,
}

pub(super) fn house_view(body: &[u8]) -> anyhow::Result<HouseView> {
    use aaemu_commons::helpers::convert_long_to_coord;
    let mut s = PacketStream::from_bytes(body.to_vec());
    let tl = s.read_u16()?;
    s.read_u32()?; // house database id
    let obj_id = s.read_bc()?;
    let template_id = s.read_u32()?;
    s.read_u32()?; // model id
    let co_owner_id = s.read_u32()?;
    let owner_id = s.read_u32()?;
    s.read_string()?; // owner name
    s.read_u32()?; // account id
    let permission = s.read_u8()?;
    let all_action = s.read_i32()?;
    let current_action = s.read_i32()?;
    s.read_i32()?; // tax
    let x = convert_long_to_coord(s.read_i64()?);
    let y = convert_long_to_coord(s.read_i64()?);
    let z = s.read_f32()?;
    let name = s.read_string()?;
    Ok(HouseView {
        tl,
        obj_id,
        template_id,
        co_owner_id,
        owner_id,
        permission,
        all_action,
        current_action,
        pos: (x, y, z),
        name,
    })
}

/// `SCUnitAttachedPacket`: `(child obj id, attach point, mount/slave obj id, reason)`.
pub(super) fn attached(p: &Packet) -> anyhow::Result<(u32, u8, u32, u8)> {
    let mut s = p.stream();
    Ok((s.read_bc()?, s.read_u8()?, s.read_bc()?, s.read_u8()?))
}

/// `SCUnitDetachedPacket`: `(child obj id, reason)`.
pub(super) fn detached(p: &Packet) -> anyhow::Result<(u32, u8)> {
    let mut s = p.stream();
    Ok((s.read_bc()?, s.read_u8()?))
}

/// `CSLeaveWorldPacket(quit)` and, for the normal logout timer, `SCLeaveWorldGranted`. Waiting
/// keeps the next run of the same character from being denied ("enter world denied").
pub(super) async fn leave_world(game: &mut GameClient) -> anyhow::Result<()> {
    game.leave_world(0).await?;
    let prep = game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    let logout_ms = prep.stream().read_i32().unwrap_or(0);
    if logout_ms <= 10_000 {
        game.wait_sc(sc::SCLeaveWorldGrantedPacket).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    Ok(())
}

/// `SCLootBagDataPacket` item count and the item template ids it lists (`LootItem.Write` starts
/// with slot type, slot, template, fake id, ...).
pub(super) fn loot_bag_templates(body: &[u8]) -> anyhow::Result<Vec<u32>> {
    let mut s = PacketStream::from_bytes(body.to_vec());
    let count = s.read_u8()?;
    let mut templates = Vec::new();
    for _ in 0..count {
        s.read_u8()?; // slot type
        s.read_u8()?; // slot
        templates.push(s.read_u32()?);
    }
    Ok(templates)
}

/// `SCLootItemTookPacket` item template id.
pub(super) fn loot_took_template(p: &Packet) -> anyhow::Result<u32> {
    Ok(p.stream().read_u32()?)
}

/// One item entry of an `SCItemTaskSuccessPacket` (the count is what the task added).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ItemStack {
    pub template: u32,
    pub item_id: u64,
    pub slot_type: u8,
    pub slot: u8,
    pub count: i32,
}

/// The item entries of `template` in an `SCItemTaskSuccessPacket` among `packets`, including
/// `AddStack` entries (which `scenario::parse_item_task` does not read).
pub(super) fn item_stacks(packets: &[Packet], template: u32) -> Vec<ItemStack> {
    let mut out = Vec::new();
    for p in packets
        .iter()
        .filter(|p| p.is_sc(sc::SCItemTaskSuccessPacket))
    {
        let mut s = p.stream();
        let parsed = (|| -> anyhow::Result<()> {
            s.read_u8()?; // task type
            let count = s.read_u8()?;
            for _ in 0..count {
                match s.read_u8()? {
                    4 => {
                        let slot_type = s.read_u8()?;
                        let slot = s.read_u8()?;
                        let item_id = s.read_u64()?;
                        let count = s.read_i32()?;
                        let tpl = s.read_u32()?;
                        if tpl == template {
                            out.push(ItemStack {
                                template: tpl,
                                item_id,
                                slot_type,
                                slot,
                                count,
                            });
                        }
                    }
                    5 | 6 | 15 => {
                        let slot_type = s.read_u8()?;
                        let slot = s.read_u8()?;
                        let tpl = s.read_u32()?;
                        let item_id = s.read_u64()?;
                        s.read_u8()?; // grade
                        s.read_u8()?; // flags
                        let count = s.read_i32()?;
                        let len = s.read_i16()?;
                        if len < 0 {
                            anyhow::bail!("negative item detail length");
                        }
                        s.read_bytes(len as usize)?;
                        s.read_i64()?; // create time
                        s.read_i32()?; // lifespan
                        s.read_u32()?; // made unit
                        s.read_u8()?; // world id
                        s.read_i64()?; // unsecure time
                        s.read_i64()?; // unpack time
                        if tpl == template {
                            out.push(ItemStack {
                                template: tpl,
                                item_id,
                                slot_type,
                                slot,
                                count,
                            });
                        }
                    }
                    _ => anyhow::bail!("unsupported item task action"),
                }
            }
            Ok(())
        })();
        if parsed.is_err() {
            break;
        }
    }
    out
}

/// `CSDestroyItemPacket(itemId, slotType, slot, count)`.
pub(super) async fn destroy_item(game: &mut GameClient, item: &ItemStack) -> anyhow::Result<()> {
    let mut s = PacketStream::new();
    s.write_u64(item.item_id)
        .write_u8(item.slot_type)
        .write_u8(item.slot)
        .write_i32(item.count.max(1));
    game.send_cs(aaemu_game::opcodes::cs::CSDestroyItemPacket, s.as_bytes())
        .await
}

/// `SCUnitsRemovedPacket` object ids.
pub(super) fn removed_ids(p: &Packet) -> anyhow::Result<Vec<u32>> {
    let mut s = p.stream();
    let n = s.read_u16()?;
    let mut ids = Vec::new();
    for _ in 0..n {
        ids.push(s.read_bc()?);
    }
    Ok(ids)
}

/// The bot's own server position (`/position`), for checks the movement echo does not expose
/// (a rider is a child transform while mounted).
pub(super) async fn self_position(game: &mut GameClient) -> anyhow::Result<(f32, f32, f32)> {
    let reply = gm_command(game, "position").await?;
    let line = reply
        .iter()
        .filter_map(chat_text)
        .find(|t| t.contains("X:"))
        .context("no /position reply")?;
    let axis = |name: &str| -> anyhow::Result<f32> {
        let rest = line
            .split(&format!("{name}: "))
            .nth(1)
            .with_context(|| format!("missing {name} in {line:?}"))?;
        let raw = rest
            .split_whitespace()
            .next()
            .context("missing axis value")?
            .trim_start_matches("|cFFFFFFFF")
            .trim_end_matches("|r");
        raw.parse()
            .with_context(|| format!("axis {name} in {line:?} is not a number"))
    };
    Ok((axis("X")?, axis("Y")?, axis("Z")?))
}

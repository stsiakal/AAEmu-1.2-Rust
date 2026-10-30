//! Shared helpers of the core-loop scenarios (`housing`, `mount`, `questchain`, `gather`).

use std::time::Duration;

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::sc;
use anyhow::{ensure, Context};

use super::questitem::{chat_text, gm_command};
use super::{BotConfig, Runner, QUIET};
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

/// The longest logout timer of a normal leave (the server's 10 s); a longer one is the combat
/// timer (5 minutes), which a quitting bot does not wait out.
const NORMAL_LOGOUT_MS: i32 = 10_000;

/// How long the server may take from `SCPrepareLeaveWorld` to `SCLeaveWorldGranted`: the logout
/// timer plus the departure save, which queues behind the other bots' saves.
const GRANT_PATIENCE: Duration = Duration::from_secs(30);

/// What the server answered to a `CSLeaveWorld`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LeaveAnswer {
    /// The logout timer (ms) of `SCPrepareLeaveWorld`; `None` when the grant came first.
    pub logout_ms: Option<i32>,
    /// `SCLeaveWorldGranted` arrived: the character left the world.
    pub granted: bool,
}

/// Waits for the answers to a `CSLeaveWorld`: `SCPrepareLeaveWorld` starts the logout timer and
/// `SCLeaveWorldGranted` ends it. Whichever arrives first is read, none is dropped while waiting
/// for the other. With `must_grant` the call returns once the character left the world;
/// otherwise (a quitting bot) a combat timer of minutes ends the wait at its announcement.
pub(super) async fn await_leave(
    game: &mut GameClient,
    must_grant: bool,
) -> anyhow::Result<LeaveAnswer> {
    let first = game
        .wait_for("SCPrepareLeaveWorld or SCLeaveWorldGranted", |p| {
            p.is_sc(sc::SCPrepareLeaveWorldPacket) || p.is_sc(sc::SCLeaveWorldGrantedPacket)
        })
        .await?;
    let granted = first.is_sc(sc::SCLeaveWorldGrantedPacket);
    let mut answer = LeaveAnswer {
        logout_ms: (!granted).then(|| first.stream().read_i32().unwrap_or(0)),
        granted,
    };
    let timer_ends_soon = answer.logout_ms.is_none_or(|ms| ms <= NORMAL_LOGOUT_MS);
    if !answer.granted && (must_grant || timer_ends_soon) {
        let saved = game.timeout;
        game.timeout = saved.max(GRANT_PATIENCE);
        let granted = game.wait_sc(sc::SCLeaveWorldGrantedPacket).await;
        game.timeout = saved;
        granted?;
        answer.granted = true;
    }
    Ok(answer)
}

/// `CSLeaveWorldPacket(quit)` and, for the normal logout timer, `SCLeaveWorldGranted`. Waiting
/// keeps the next run of the same character from being denied ("enter world denied").
pub(super) async fn leave_world(game: &mut GameClient) -> anyhow::Result<()> {
    game.leave_world(0).await?;
    await_leave(game, false).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    Ok(())
}

/// One item entry of an `SCItemTaskSuccessPacket` (the count is what the task added; a negative
/// count is a `ItemCountUpdate` that consumed that many).
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
    item_stacks_matching(packets, |tpl| tpl == template)
}

/// Every item entry [`item_stacks`] can read, whatever its template.
pub(super) fn item_stacks_any(packets: &[Packet]) -> Vec<ItemStack> {
    item_stacks_matching(packets, |_| true)
}

fn item_stacks_matching(packets: &[Packet], want: impl Fn(u32) -> bool) -> Vec<ItemStack> {
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
                        if want(tpl) {
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
                        if want(tpl) {
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
            continue;
        }
    }
    out
}

/// The stack of `template` that the task packets just added. Only when no entry of the template
/// can be read at all, the slot-only parse of `questitem::stack_location` stands in (its count is
/// assumed to be 1); entries that took units away never count as an addition.
pub(super) fn added_stack(packets: &[Packet], template: u32) -> Option<ItemStack> {
    let stacks = item_stacks(packets, template);
    if let Some(stack) = stacks.iter().find(|s| s.count > 0) {
        return Some(*stack);
    }
    if !stacks.is_empty() {
        return None;
    }
    super::questitem::stack_location(packets, template).map(|(item_id, slot_type, slot)| {
        ItemStack {
            template,
            item_id,
            slot_type,
            slot,
            count: 1,
        }
    })
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

/// The spawn grid of bots that run the same scenario at the same time: [`SLOTS`] spots in a
/// square grid, [`SLOT_SPACING`] metres apart. Each bot owns one slot for the whole run
/// (`--slot N`, see [`bot_slot`]), so the NPCs it spawns never land near another bot's spot.
pub(super) const SLOTS: u32 = 16;
const SLOT_COLUMNS: u32 = 4;
pub(super) const SLOT_SPACING: f32 = 25.0;

/// The bot's slot: `BotConfig::slot` (`--slot`), else the number its account name ends with (the
/// soak runner's accounts are `<prefix>b<index>`), else 0; always below [`SLOTS`]. Only an
/// explicit, unique `--slot` per bot keeps more than [`SLOTS`] bots apart.
pub(super) fn bot_slot(cfg: &BotConfig) -> u32 {
    cfg.slot.unwrap_or_else(|| trailing_number(&cfg.user)) % SLOTS
}

/// The decimal number a name ends with (0 without one), reduced modulo [`SLOTS`] as it is read.
fn trailing_number(name: &str) -> u32 {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    name[stem.len()..]
        .bytes()
        .fold(0, |acc, digit| (acc * 10 + u32::from(digit - b'0')) % SLOTS)
}

/// The offset (east, north), in metres, of `slot` from the scenario origin: a 4 x 4 grid centred
/// on it. The origin is where the bot stood when the scenario began, captured once, never the
/// bot's current position (which drifts while it follows an NPC).
pub(super) fn slot_offset(slot: u32) -> (f32, f32) {
    let slot = slot % SLOTS;
    let centre = (SLOT_COLUMNS - 1) as f32 / 2.0;
    (
        ((slot % SLOT_COLUMNS) as f32 - centre) * SLOT_SPACING,
        ((slot / SLOT_COLUMNS) as f32 - centre) * SLOT_SPACING,
    )
}

/// How far in front of the bot the GM `/spawn` commands place an NPC, and the leeway accepted
/// when matching an announcement to the command (the terrain snap and float rounding).
pub(super) const SPAWN_DISTANCE: f32 = 3.0;
pub(super) const SPAWN_TOLERANCE: f32 = 1.0;

/// Where `/spawn npc` puts an NPC for a bot that just teleported to `at`. The teleport leaves the
/// bot facing yaw 0, and the server places the NPC `SPAWN_DISTANCE` in front
/// (`add_distance_to_front`: `x -= d * sin(yaw)`, `y += d * cos(yaw)`), straight north.
pub(super) fn spawn_point(at: (f32, f32, f32)) -> (f32, f32) {
    (at.0, at.1 + SPAWN_DISTANCE)
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

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashSet;

    use aaemu_commons::net::FrameCodec;
    use aaemu_commons::Marshal;
    use aaemu_game::models::item::Item;
    use aaemu_game::models::item_task::ItemTask;
    use aaemu_game::opcodes::cs;
    use aaemu_game::systems::economy::item_packets::SCItemTaskSuccessPacket;
    use futures::{SinkExt, StreamExt};
    use tokio::io::DuplexStream;
    use tokio_util::codec::Framed;

    use super::*;
    use crate::game::LEVEL_GAME;

    /// A bag item (`slot type 2`, slot 4) as the server's item tasks describe it.
    pub(in crate::scenario) fn item(template_id: u32, id: u64, count: i32) -> Item {
        let mut i = Item::new(1);
        i.template_id = template_id;
        i.id = id;
        i.count = count;
        i.slot_type = 2;
        i.slot = 4;
        i
    }

    /// An `SCItemTaskSuccessPacket` written by the server's own packet type.
    pub(in crate::scenario) fn task_packet(tasks: Vec<ItemTask>) -> Packet {
        let mut body = PacketStream::new();
        SCItemTaskSuccessPacket {
            action: 11,
            tasks,
            force_remove: Vec::new(),
        }
        .write(&mut body);
        Packet {
            level: LEVEL_GAME,
            opcode: sc::SCItemTaskSuccessPacket,
            body: body.into_bytes(),
        }
    }

    #[test]
    fn item_stacks_keeps_reading_after_a_packet_with_an_unparsed_action() {
        let meat = item(4058, 7, 1);
        // `ItemUpdateBits` (action 10) is not an entry `item_stacks` reads.
        let unparsed = task_packet(vec![ItemTask::update_bits(&meat)]);
        let added = task_packet(vec![ItemTask::add(&meat)]);
        let stacks = item_stacks(&[unparsed, added], 4058);
        assert_eq!(
            stacks,
            [ItemStack {
                template: 4058,
                item_id: 7,
                slot_type: 2,
                slot: 4,
                count: 1
            }]
        );
    }

    #[test]
    fn item_stacks_reads_added_and_consumed_counts() {
        let meat = item(4058, 7, 3);
        let packets = [
            task_packet(vec![ItemTask::count_update(&meat, 2)]),
            task_packet(vec![ItemTask::count_update(&meat, -3)]),
        ];
        let counts: Vec<i32> = item_stacks(&packets, 4058)
            .iter()
            .map(|s| s.count)
            .collect();
        assert_eq!(counts, [2, -3]);
        // `added_stack` only reports what a task added.
        assert_eq!(added_stack(&packets[1..], 4058), None);
        assert_eq!(added_stack(&packets, 4058).map(|s| s.count), Some(2));
    }

    fn config(user: &str, slot: Option<u32>) -> BotConfig {
        serde_json::from_value(serde_json::json!({
            "login": "127.0.0.1:1",
            "user": user,
            "password": "x",
            "character": user,
            "slot": slot,
        }))
        .unwrap()
    }

    #[test]
    fn the_sixteen_slots_are_distinct_spots_at_least_25_metres_apart_around_the_origin() {
        let spots: Vec<(f32, f32)> = (0..SLOTS).map(slot_offset).collect();
        let distinct: HashSet<(i32, i32)> = spots
            .iter()
            .map(|s| ((s.0 * 10.0) as i32, (s.1 * 10.0) as i32))
            .collect();
        assert_eq!(distinct.len(), SLOTS as usize);
        let mut closest = f32::MAX;
        for (i, a) in spots.iter().enumerate() {
            for b in &spots[i + 1..] {
                closest = closest.min(((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt());
            }
        }
        assert!(closest >= SLOT_SPACING - 0.001, "spots {closest} m apart");
        // Centred on the origin, so no spot is further than 53 m from where the bots began.
        let (east, north) = spots
            .iter()
            .fold((0.0, 0.0), |sum, s| (sum.0 + s.0, sum.1 + s.1));
        assert!(east.abs() < 0.001 && north.abs() < 0.001);
        let farthest = spots.iter().map(|s| s.0.hypot(s.1)).fold(0.0, f32::max);
        assert!(
            farthest < 53.2,
            "farthest slot {farthest} m from the origin"
        );
        // A slot beyond the grid wraps instead of leaving it.
        assert_eq!(slot_offset(SLOTS), slot_offset(0));
        assert_eq!(slot_offset(SLOTS + 5), slot_offset(5));
    }

    #[test]
    fn eight_bots_of_one_soak_each_get_their_own_spot_whatever_their_character_ids() {
        // The soak runner names its accounts `<prefix>b<index>` and passes `--slot <index>`.
        let prefix = "sk10301234b";
        let by_flag: HashSet<u32> = (0..8)
            .map(|i| bot_slot(&config(&format!("{prefix}{i}"), Some(i))))
            .collect();
        assert_eq!(by_flag.len(), 8);
        // Without the flag the account name's trailing index decides, with the same result.
        let by_name: HashSet<u32> = (0..8)
            .map(|i| bot_slot(&config(&format!("{prefix}{i}"), None)))
            .collect();
        assert_eq!(by_name, by_flag);
        assert_eq!(bot_slot(&config("sk10301234b3", None)), 3);
    }

    #[test]
    fn an_explicit_slot_wins_over_the_account_name_and_stays_inside_the_grid() {
        assert_eq!(bot_slot(&config("bot12", Some(5))), 5);
        assert_eq!(bot_slot(&config("bot12", None)), 12);
        assert_eq!(bot_slot(&config("bot", None)), 0);
        assert_eq!(bot_slot(&config("bot16", None)), 0);
        assert_eq!(bot_slot(&config("bot", Some(SLOTS + 2))), 2);
        // A very long number never overflows.
        assert!(bot_slot(&config("bot99999999999999999999999999", None)) < SLOTS);
    }

    #[test]
    fn the_spawn_point_is_where_the_servers_spawn_command_puts_an_npc_at_yaw_zero() {
        assert_eq!(spawn_point((100.0, 200.0, 50.0)), (100.0, 203.0));
        // `/spawn npc` places the NPC with the server's own `add_distance_to_front`; a bot that
        // just teleported faces yaw 0 (its `move_to` sends direction 0).
        let mut npc = aaemu_game::models::transform::Transform {
            x: 100.0,
            y: 200.0,
            z: 50.0,
            ..Default::default()
        };
        aaemu_game::systems::vehicles::slaves::add_distance_to_front(&mut npc, SPAWN_DISTANCE);
        assert_eq!((npc.x, npc.y), spawn_point((100.0, 200.0, 50.0)));
    }

    /// The server end of an in-memory game connection: what a scenario sent, and what it is
    /// answered. The fixtures answer each client packet in order; nothing arrives unprompted.
    pub(in crate::scenario) struct Peer {
        framed: Framed<DuplexStream, FrameCodec>,
        /// The opcode of every client packet read so far, in order.
        pub received: Vec<u16>,
    }

    impl Peer {
        /// The next client packet's opcode.
        pub(in crate::scenario) async fn next(&mut self) -> u16 {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.framed.next())
                .await
                .expect("the client sent no packet")
                .expect("the client closed the connection")
                .expect("a frame");
            let opcode = u16::from_le_bytes([frame[4], frame[5]]);
            self.received.push(opcode);
            opcode
        }

        /// Reads the next client packet and requires it to be `opcode`.
        pub(in crate::scenario) async fn expect(&mut self, opcode: u16) {
            let got = self.next().await;
            assert_eq!(
                got,
                opcode,
                "expected {} but the client sent {}",
                crate::game::packet_name(crate::record::Dir::C2S, LEVEL_GAME, opcode),
                crate::game::packet_name(crate::record::Dir::C2S, LEVEL_GAME, got)
            );
        }

        /// Sends a level-1 packet to the client.
        pub(in crate::scenario) async fn send(&mut self, opcode: u16, body: &[u8]) {
            let mut frame = vec![0xdd, LEVEL_GAME, 0, 0];
            frame.extend_from_slice(&opcode.to_le_bytes());
            frame.extend_from_slice(body);
            self.framed.send(frame).await.expect("the client is gone");
        }
    }

    /// A game client wired to an in-memory server end; its waits time out after 400 ms.
    pub(in crate::scenario) fn scripted_game() -> (GameClient, Peer) {
        let (io, peer) = tokio::io::duplex(64 * 1024);
        let mut game = GameClient::from_io(io, crate::record::Recorder::new(false));
        game.timeout = Duration::from_millis(400);
        let peer = Peer {
            framed: Framed::new(peer, FrameCodec),
            received: Vec::new(),
        };
        (game, peer)
    }

    /// `SCPrepareLeaveWorldPacket(time, target, idleKick)`.
    fn prepare_body(logout_ms: i32) -> Vec<u8> {
        let mut s = PacketStream::new();
        s.write_i32(logout_ms).write_u8(1).write_bool(false);
        s.into_bytes()
    }

    #[tokio::test]
    async fn a_grant_that_comes_before_the_prepare_is_not_lost() {
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSLeaveWorldPacket).await;
            peer.send(sc::SCLeaveWorldGrantedPacket, &[1]).await;
            peer.send(sc::SCPrepareLeaveWorldPacket, &prepare_body(10_000))
                .await;
            peer
        });
        game.leave_world(1).await.unwrap();
        let answer = await_leave(&mut game, true).await.unwrap();
        assert_eq!(
            answer,
            LeaveAnswer {
                logout_ms: None,
                granted: true
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_prepare_then_the_grant_end_a_normal_leave() {
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSLeaveWorldPacket).await;
            peer.send(sc::SCPrepareLeaveWorldPacket, &prepare_body(10_000))
                .await;
            peer.send(sc::SCLeaveWorldGrantedPacket, &[0]).await;
            peer
        });
        game.leave_world(0).await.unwrap();
        let answer = await_leave(&mut game, false).await.unwrap();
        assert_eq!(
            answer,
            LeaveAnswer {
                logout_ms: Some(10_000),
                granted: true
            }
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_combat_timer_ends_a_quit_but_not_a_leave_that_must_be_granted() {
        // A quitting bot does not wait out the 5 minute combat timer ...
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSLeaveWorldPacket).await;
            peer.send(sc::SCPrepareLeaveWorldPacket, &prepare_body(300_000))
                .await;
            peer
        });
        game.leave_world(0).await.unwrap();
        let answer = await_leave(&mut game, false).await.unwrap();
        assert_eq!(
            answer,
            LeaveAnswer {
                logout_ms: Some(300_000),
                granted: false
            }
        );
        server.await.unwrap();

        // ... a re-select needs the character out of the world and waits for the grant.
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSLeaveWorldPacket).await;
            peer.send(sc::SCPrepareLeaveWorldPacket, &prepare_body(300_000))
                .await;
            peer.send(sc::SCLeaveWorldGrantedPacket, &[1]).await;
            peer
        });
        game.leave_world(1).await.unwrap();
        let answer = await_leave(&mut game, true).await.unwrap();
        assert!(answer.granted);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_leave_nobody_answers_times_out() {
        let (mut game, mut peer) = scripted_game();
        let server = tokio::spawn(async move {
            peer.expect(cs::CSLeaveWorldPacket).await;
            peer
        });
        game.leave_world(0).await.unwrap();
        let error = await_leave(&mut game, true).await.unwrap_err();
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
        server.await.unwrap();
    }

    #[test]
    fn the_house_builder_matches_the_reader() {
        let body = crate::game::test_packets::house_body(7, 900, 172, 42, 2, 1);
        let view = house_view(&body).unwrap();
        assert_eq!(
            (
                view.tl,
                view.obj_id,
                view.template_id,
                view.owner_id,
                view.all_action,
                view.current_action,
                view.name.as_str()
            ),
            (7, 900, 172, 42, 2, 1, "house")
        );
    }
}

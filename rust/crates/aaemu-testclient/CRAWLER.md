# Quest crawler (`aaemu-testclient crawl`)

Plays every 1.2 quest of one race through the real client packets and reports what broke.
GM commands are used only for setup: `/level`, `/move`, `/item add`, `/spawn npc|doodad`,
`/revive`, `/damage` (only to put an item-use target below its required HP threshold) and the
read-only `/sphere list`. Accepting, talking, fighting, gathering/interacting, sphere entry,
item use and turn-in all go through `CS*` packets, and the objective counters are read from
`SCQuestContext*` packets.

The four playable races are `nuian` (race 1), `elf` (race 4), `harani` (race 5) and `firran`
(race 6, the client's "Ferre"). Start one process per race for race- and faction-gated quests;
Nuian/Elf are Nuia (mother faction 148), Harani/Firran are Haranya (149).

## Build

```sh
cd rust
cargo build --profile live --bin aaemu-testclient
```

## Run

```sh
TC=D:/aaemu-target/<lane>/live/aaemu-testclient.exe
$TC crawl --race nuian \
  --login 127.0.0.1:<lane-login-port> \
  --user crawl-nuian --character CrawlerNuian \
  --levels 1-15 --min-level 15 --limit 60 \
  --out D:/aaemu-live-<lane>/crawl \
  --game-config D:/aaemu-live-<lane>/rs-game/Config.Local.json \
  --mysql-bin "C:/Program Files/MySQL/MySQL Server 8.4/bin" \
  --server-log D:/aaemu-live-<lane>/logs/rs-game.log \
  --bugreports D:/aaemu-live-<lane>/rs-game/BugReports \
  --quest-timeout 180
```

- `--game-config` + `--mysql-bin` grant the crawler's account access level 100 before its
  first character is created (`accounts` / `characters` in the lane's game database). The
  file is merged over the `Config.json` next to it and overridden by
  `Connections__MySQLProvider__*` environment variables, as the game server does; a config
  without a port is refused, and an explicit 3306 is refused too (the crawler may only use the
  lane's private MySQL) unless `--mysql-port` names another port. `Configurations/**/*.json` is
  merged in the server's order. Without `--game-config` the GM bootstrap prints the SQL the
  account needs.
- `--gm-closed-zones` re-grants the account and the character access level 100 right before
  character select, so the server does not evict the character from a zone the 1.2 data marks
  `closed` (for example Bronze Rock). The server's closed-zone check reads the **character's**
  access level, and a character created in the same run starts from the server config default
  (0), so the pre-login grant alone does not protect it. Default off: closed-zone quests are
  still attempted, and a failed attempt is recorded as `closed-zone` instead of `failed` (see
  "Output").
- `--min-level` raises the character before each quest (setup); `0` uses the quest level.
  `--limit` counts quests the crawler can actually attempt, not unsupported/gated ones;
  without it the crawler attempts every quest currently available in the level band.
- `--from-quest` starts at the first catalog quest with that id or later; `--levels a-b`
  restricts the level band.
- Results are appended to `<out>/<race>-<character>.jsonl`, one file per character: two
  characters of the same race never share results. Gates and passes are seeded from the
  server's completed-quest list for the character that just entered the world, not from the
  file; the seeded ids are logged at start. Unsupported and skipped quests are re-recorded but do not consume the limit. A quest
  that passes unlocks the quests chained behind it later in the same run; a prerequisite that
  failed keeps its dependants skipped until it passes in a later run. `--skip-failed`
  additionally skips quests that already failed in that character's result file, so a
  full-band pass only spends time on quests that were never attempted.
- Enable `BugReports.Enabled` in the lane's `rs-game/Config.Local.json` (Directory empty =
  `<app>/BugReports`) to attach report paths to failures. The crawler ignores teleport /
  closed-zone report noise.
- `--dashboard` (with `--out`, no race) only rewrites the dashboard from existing JSONL files.

## Output

`catalog.json` is derived from `compact.sqlite3`, the main-world `npc_spawns.json` /
`doodad_spawns.json` and the doodad/skill tables: per quest its starter, objective acts and
targets, turn-in, rewards, race/faction/level requirements and the nearest authored target
positions. It is written on every run and never committed.

Each result records `passed`, `daily-limit`, `failed`, `unsupported`, `skipped` or
`closed-zone`, the failing step (`accept`, `objective N <act>`, `turn-in`, ...), the objective
counters reached, the duration, the server WARN/ERROR lines in the quest window and new
bug-report paths. `closed-zone` means the quest was played and failed while its zone (or one of
its objective zones) has `closed = 't'` in `zones`, or the server evicted the character from a
closed zone during it (`SCErrorMsg 287`). A pass in a closed zone stays a pass. `closed-zone`
is deliberately not a failure and has its own dashboard column in every table, including the
per-zone one. A quest is marked `gm_assisted` (with the objective list) when a GM command or
the pre-accept setup satisfied an objective: an item hand-over for a gather act with no
highlight doodad, a gather or level act the setup `/level`/gate/starter item made true at
accept, or `/level` for a level act. `daily-limit` is error 621 (`QuestDailyLimit`); it is not
a pass and is retried in later runs.

`crawl.json` / `crawl.md` aggregate the latest result per race, character and quest: totals per
race, then one row per character, level band, zone and act kind (with GM-assisted passes in
their own column, outside the plain passed total); failing quests ranked by how many races hit
them; bug-report fingerprints. Old per-race result files without a `character` field (for
example `nuian.jsonl` from before results were scoped per character) are ignored and counted in
the header.
A ledger records every bag unit the crawler adds, and cleanup destroys exactly those units
(plus late quest rewards still in the ledger) after each quest and before the next one, so a
long run does not fill the inventory. The ledger survives a reconnect while the bag is
rebuilt from the new connection's inventory snapshot.

## Current coverage and limits

Supported objectives: `ObjTalk`, `ObjTalkNpcGroup`, `ObjMonsterHunt`, `ObjMonsterGroupHunt`,
`ObjItemGather` / `ObjItemGroupGather` (GM item setup when there is no usable highlight
doodad), `ObjItemUse` / `ObjItemGroupUse` (item cast on self, a required NPC/group, or a
doodad; injured-target requirements use one setup hit), `ObjSphere` (position from
`/sphere list 0`), and `ObjLevel`. Doodad interactions use the skill of the spawned funct
group.

Unsupported and reported as such: cinema, craft, aggro, express/effect fire, zone-wide kill
and other zone acts, mate level, mail, condition, distance, phase checks, item equips,
starter spheres, empty/"reserve" templates, and doodads whose data offers no interaction
skill. Quests in instance worlds and quests whose data has no main-world spawn for a required
NPC/doodad are skipped.

Quests in closed zones are still played. The quest's `quest_contexts.zone_id` or an
objective's zone act points at a `zones` row with `closed = 't'`; a failed attempt there is
recorded as `closed-zone` instead of `failed`, and so is a failure during a quest whose own
zone is open when the server evicts the character (`SCErrorMsg 287`). Passes stay passes, so
the pass count in those zones is unchanged. Without access level 100 the server evicts a
character after about 10 s (`world/zone_access.rs`); `--gm-closed-zones` avoids that. The
2026-10-30 crawl blamed the Bronze Rock failures (quests 2098-2103) on faction-3 quest mobs
taking no damage; that was wrong. Those zones are closed (154/192/193 in the client's zone
keys) and the crawler's fights were interrupted by the eviction teleport. Kill objectives
there are not evidence of a damage bug; the general crawler limit (auto attack only, no gear)
still applies to high-level mobs.

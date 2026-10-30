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
- `--min-level` raises the character before each quest (setup); `0` uses the quest level.
  `--limit` counts quests the crawler can actually attempt, not unsupported/gated ones;
  without it the crawler attempts every quest currently available in the level band.
- `--from-quest` starts at the first catalog quest with that id or later; `--levels a-b`
  restricts the level band.
- Results are appended to `<out>/<race>-<character>.jsonl`, one file per character: two
  characters of the same race never share results. Gates and passes are seeded from the
  server's completed-quest list for the character that just entered the world, not from the
  file. Unsupported and skipped quests are re-recorded but do not consume the limit. A quest
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

Each result records `passed`, `daily-limit`, `failed`, `unsupported` or `skipped`, the failing
step (`accept`, `objective N <act>`, `turn-in`, ...), the objective counters reached, the
duration, the server WARN/ERROR lines in the quest window and new bug-report paths. A quest is
marked `gm_assisted` (with the objective list) when a GM command or the pre-accept setup
satisfied an objective: an item hand-over for a gather act with no highlight doodad, a gather
or level act the setup `/level`/gate/starter item made true at accept, or `/level` for a level
act. `daily-limit` is error 621 (`QuestDailyLimit`); it is not a pass and is retried in later
runs.

`crawl.json` / `crawl.md` aggregate the latest result per race and quest: totals per race,
level band, zone and act kind (with GM-assisted passes in their own column, outside the plain
passed total); failing quests ranked by how many races hit them; bug-report fingerprints.
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

Known live failure worth triage: in Bronze Rock the faction-3 quest mobs (e.g. quest 2101,
2102, 2103 targets) take no damage from the auto attack, even after `CSSetForceAttack`, while
faction-115 monsters die normally.

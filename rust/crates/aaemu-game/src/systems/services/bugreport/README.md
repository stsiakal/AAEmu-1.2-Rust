# Automated bug reports (`aaemu-bugreport/1`)

Fork-only subsystem (`aaemu-game/src/systems/services/bugreport/`) that turns server trouble
into self-contained JSON reports with the context a developer needs. It works for real players
today and gives bots/scenarios an API for their own decision timeline.

## Enable

```json
"BugReports": {
  "Enabled": false,
  "Directory": "",
  "MaxFullReportsPerFingerprint": 3,
  "NearbyRadius": 40,
  "PacketRing": 64,
  "LogRing": 200,
  "RatePerSecond": 5,
  "MaxFingerprints": 2000,
  "MaxFullReports": 2000,
  "MaxTotalBytes": 67108864
}
```

`Config.json` ships it disabled; enable it in `Config.Local.json` or with
`--BugReports:Enabled=true` (tests enable it explicitly). `Directory` empty means
`<app dir>/BugReports`. Relative paths resolve against the process working directory.

## Triggers

| Kind | Source |
| --- | --- |
| `log` | `tracing` WARN / ERROR events (target, message, fields; object ids parsed from fields like `obj_id` and from the message) |
| `panic` | panic hook; the previous hook's output (default hook / `Logs/Crash.log`) is kept |
| `command` | `/bug <text>` (access level 0, `Configurations/AccessLevels.json`) |
| `invariant` | 5 s budgeted checks: HP < 0 or > max, unit far below the terrain floor, parent object missing, slave position jump > 500 m |
| `external` | `bugreport::report(...)` / `Trigger::External` from subsystems, scenarios and bots |

`systems/vehicles/audit.rs` is embedded as `ship_audit` when the actor or target is a slave or
is attached to one.

## Dedup and files

A fingerprint is `FNV-1a/64(trigger kind, subsystem/target/check/location, message with digit
runs replaced by #, opcode)`. Repeated incidents collapse into one fingerprint with counters.

```
<Directory>/<run>/<fingerprint>-<n>.json   first N occurrences (N = MaxFullReportsPerFingerprint)
<Directory>/<run>/summary.json             counters, examples, systems table (every 5 s + shutdown)
```

`<run>` is `<local timestamp>-<pid>`. The first `N` occurrences write a full file unless the
global `RatePerSecond` budget for that second is exhausted; later occurrences only increment
the counters in `summary.json`. `MaxFingerprints`, `MaxFullReports` and `MaxTotalBytes` cap one
run (new fingerprints / files after a cap only bump `capped_reports` in the summary). `/bug`
reports are grouped by character, ignore the text in the fingerprint, are limited to 500
characters and have a 60 s per-character cooldown. INFO lines go to a separate bounded ring,
never the report channel, so an INFO flood cannot drop WARN/ERROR reports. The packet/log rings
and world snapshots are read on the report writer thread, never on the packet or tick hot path.

## Report

```json
{
  "format": "aaemu-bugreport/1",
  "id": "20261029-164500-1234-000001",
  "fingerprint": "3c9ab0c4d1e2f345",
  "fingerprint_number": 1,
  "trigger": {"kind": "log", "level": "WARN", "target": "...", "message": "...", "fields": {}},
  "severity": "warn",
  "timestamp": "2026-10-29T14:45:00.123Z",
  "build": "0.1.0",
  "run": "20261029-164500-1234",
  "actor": null,
  "target": {"obj_id": 1234, "kind": "npc", "name": "...", "level": 10, "hp": 100, "max_hp": 100,
             "position": {"x": 0, "y": 0, "z": 0, "rot_x": 0, "rot_y": 0, "rot_z": 0},
             "zone_id": 100, "instance_id": 0, "parent_obj_id": 0, "sticky_parent_id": 0,
             "buffs": [1, 2], "equipment": [{"slot": 0, "template_id": 12345}], "money": null,
             "labor_power": null},
  "observed_ids": [1234],
  "nearby": {"radius": 40.0, "units": [], "doodads": []},
  "ship_audit": null,
  "packets": {"in": [], "out": []},
  "logs": [{"timestamp": "...", "level": "WARN", "target": "...", "message": "..."}],
  "details": {}
}
```

* Actor / target snapshots: obj id, kind, name/template, level, hp/mp, position/rotation,
  zone/instance, parent + sticky parent, buff ids, equipment item templates, money and labor
  (characters). Player snapshots also carry `character_id` and `account_id`.
* `nearby`: units and doodads within `NearbyRadius` (id, kind, template, position, hp for units).
* `packets`: the actor connection's last `PacketRing` packets in both directions (opcode, name,
  size, timestamp, truncated body).
* `logs`: the last `LogRing` lines that mention the actor/target object or character id.
* Passwords, secrets, tokens, tickets and API keys are redacted from the whole report (details,
  messages, packet bodies and log lines); auth/login/ticket/key packet bodies are replaced
  outright.

## API for subsystems, scenarios and bots

```rust
use aaemu_game::systems::services::bugreport::{self, Outcome, Severity, Trigger};

bugreport::report(
    server,
    Trigger::external("bots.trade", "trade run made no progress", Severity::Warn),
    Some(actor_obj_id),
    Some(target_obj_id),
    serde_json::json!({"plan": "trade_run", "step": 3}),
);
bugreport::outcome("bots.trade", "sell_pack", Outcome::Refused);
```

`report` returns `false` when bug reports are disabled. `outcome` feeds the `systems` table in
`summary.json` (`ok` / `fail` / `refused`, total and success rate) that the bots dashboard reads.
`build` uses `AAEMU_GIT_DESCRIBE` when set at build time, otherwise the crate version.

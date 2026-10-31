# Quest crawler triage notes

Triage of the 2026-10-30 crawl evidence in `D:/aaemu-audits/quest-crawl-2026-10-30/`
(read-only evidence; the files there are not edited). These notes supersede that folder's
Bronze Rock entry.

## Bronze Rock quests 2098-2103: closed zone, not a damage bug

Verdict: the "faction-3 quest mobs take no damage" reading was wrong. Bronze Rock is a closed
zone and the crawler was evicted from it.

Data and server evidence:

- `zones` rows 19 `w_bronze_rock_1`, 138 `w_bronze_rock_2` and 139 `w_bronze_rock_3` have
  `closed = 't'` (client zone keys 154, 192 and 193). Quests 2098-2103 all have
  `quest_contexts.zone_id = 19`.
- `world/zone_access.rs` (`needs_return` / `update`) reads the **character's** access level,
  warns five times 2 s apart (`SCErrorMsg 287`, `closed_zone`) and transports a character
  with `access_level < 100` out. The audit log shows "entered closed zone 154" every ~10 s
  and a `TeleportEnded` loop in each Bronze Rock quest window.
- Faction is not the cause: faction-3 and faction-115 targets fail the same way, no
  invulnerability exists in the damage path, and `can_attack` accepts both. The audit's own
  level table shows kills succeed only against level <= 3 mobs (the crawler fights unarmed
  with auto attack only), which explains the rest.
- Before this change the pre-login GM grant updated the account and
  already-existing characters, but a character created in that run was inserted with the
  server config's character default (0), so the closed-zone check still evicted it.

Crawler handling:

- Quests whose zone (or objective zone act) is closed are recorded as `closed-zone` before
  any packet is sent, not as failures, and have their own dashboard column.
- `--gm-closed-zones` opts in to playing them and re-grants access level 100 to the account
  and the character immediately before character select.

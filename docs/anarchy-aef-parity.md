# Anarchy / AEF safeguard parity and tuning (Horizon)

This note maps high-impact safeguards from `LinearRegionFileFormatTools-linearv2/Config for things i want.txt` (AnarchyExploitFixes-style config) to Horizon's built-in `[anarchy]` implementation and documents Horizon-applicable gaps.

Scope: high-playercount anarchy, playable with low restriction. Tune in `server.toml` under `[anarchy]` (see `crates/horizon-server/src/config.rs` / generated server reference docs for schema).

## Implemented and tuned in Horizon

### Chunk / entity limits
- `falling_blocks_per_chunk` (default `Some(60)`): caps falling block entities per chunk during periodic cleanup.
- `experience_bottles_per_chunk` (default `Some(25)`): caps thrown experience bottle projectiles per chunk during periodic cleanup.
- `dropped_items_per_chunk` (default `300`): caps dropped item entities per chunk during periodic cleanup (0 disables).
- `minecarts_per_chunk` (default `40`) and `vehicles_per_chunk` (default `60`): caps minecarts/vehicles per chunk; occupied vehicles are preserved when possible.
- `villagers_per_chunk` (default `0`, disabled) with `villager_removal_priority`: optional villager cap/removal policy.
- `mob_limits_per_chunk` (default `{}`): per-entity-type mob caps per chunk.
- `block_entities_per_chunk` (default `1000`): excess block entities in a chunk are removed and their blocks cleared during periodic cleanup.

Runtime enforcement lives in `crates/horizon-client/src/game/anarchy.rs` (`anarchy_limit_transient_entities` / block and non-block entity limit passes, ticked from `anarchy_tick`).

### Projectile/entity age cleanup
- `projectile_max_age_ticks` (default `300`): global projectile max age cleanup; runtime currently exempts ender pearls and wither skulls from this global removal path.
- `projectile_age_limits` (default `{}`): per-projectile-type age limits (checked periodically against projectile entity type).

These provide AEF-style projectile lifetime control without aggressively breaking core PvP/travel mechanics; tune per server if needed.

### Interaction / spam throttles
- `lever_uses_per_two_seconds` (default `30`): lever interaction rate limit.
- `inventory_opens_per_second` (default `20`): inventory/container open rate limit.

Enforced via `anarchy_allow_interaction` and call sites in gameplay interaction handling.

### Sign protections
- `prevent_command_sign` (default `true`): blocks command-sign style abuse where supported by Horizon sign handling.
- `sign_line_character_limit` (default `80`) and `sign_total_character_limit` (default `384`): sign text size limits to reduce sign-lag/oversized payload abuse.

Enforced in sign edit handling (`crates/horizon-client/src/game/sign_edit.rs`) and anarchy config.

### Existing anarchy extras (already present)
- Cracked login flow/hold, ignore/whisper/social/skin/worldstats commands, dupe-chain event controls, world border behavior toggle, random spawn/RTP/home/stat helpers, etc. (see `anarchy.rs` and server config docs). These are orthogonal to AEF lag/exploit patches but part of the same `[anarchy]` feature set.

## Horizon-applicable mappings / gaps (high impact)

The following AEF categories are not fully mapped 1:1 in Horizon today. Applicability depends on Horizon's Rust engine/protocol behavior (different from Paper/Folia plugin hooks), so gaps should be addressed with engine-specific fixes/tests.

### Not yet implemented (or only partially covered)
- **Explosion drop/container protection:** AEF `handle-excess-drops`, container material filtering, and `anti-shulker-drops`. Horizon does not currently expose equivalent anarchy toggles for explosion item-drop suppression or shulker-content drop suppression.
- **Flooding / block-physics abuse controls:** `prevent-flooding-machines` (piston pushing waterlogged blocks) and regional physics throttling (`redstone`, `pistons`, `liquid-spread`, `block-physics`, `block-spread`, `sculk-bloom`, `noteblock`, `explosions` pause/limits). No direct Horizon `[anarchy]` equivalents yet.
- **Pathfinding limits:** global/custom mob target-distance/pathfinding caps are not exposed in Horizon anarchy config.
- **Inventory action spam throttling:** Horizon rate-limits inventory opens, but AEF-style per-action click throttles (`COLLECT_TO_CURSOR`, `HOTBAR_SWAP`, `MOVE_TO_OTHER_INVENTORY`, etc.) are not implemented.
- **Entity age limits beyond current projectile handling:** AEF custom entity age limits (e.g. falling blocks/arrows/snowballs/spectral arrows/wither skulls) are only partially covered by projectile age controls; non-projectile/falling-block age cleanup knobs are not exposed.
- **Book/item/inventory payload size protections:** `anti-book-ban` (page/title/author/item/inventory byte-size limits, caching) is not represented in Horizon `[anarchy]` config.
- **Coordinate/teleport exploit hardening:** `pearl-phase`, `prevent-fast-world-teleport-crash`, `prevent-teleport-coordinate-exploit` are not implemented as Horizon anarchy safeguards.
- **Protocol/crash exploit patches specific to Paper/Bukkit/Purpur:** `tab-complete-crash-patch`, `window-click-crash-patch`, `sequence-crash-patch`, `map-cursor-lag-patch`, `beehive-crash-patch`, and `remove-beehive-coordinates` target plugin/server internals or packet handling paths that differ in Horizon. They require Horizon-specific audit/fixes rather than config parity.

### Tuning guidance (low restriction, high playercount)
- Keep chunk/entity caps enabled but generous (current defaults) to stop worst-case lag machines while preserving normal gameplay.
- Prefer raising limits before disabling if players hit legitimate farms/builds; use per-type `mob_limits_per_chunk`/`projectile_age_limits` for targeted hotspots.
- Sign and interaction throttles are low-friction protections; keep enabled unless a specific gameplay mode requires otherwise.
- For heavily damaged worlds, profile with representative `view-distance`/`simulation-distance` and monitor TPS, MSPT percentiles, chunk-load latency, and save/cache behavior before tightening limits.

## Verification evidence (workspace state)
- Linear v2 runtime/storage/prune/restore/save-load-restart smoke and delete-mode prune flows were exercised on disposable copies in this migration effort (see session notes/checkpoint artifacts under `target/` when present).
- Automated checks/tests relevant to touched crates were run in offline mode during development (`horizon-world`, `horizon-server`, selected `horizon-client` checks as environment allowed).

# RSAnarchy Server

A server fork of [Horizon](https://github.com/horizon-rust/horizon), a Rust port of Minecraft Java Edition 26.3, run for .RS Anarchy.


## Building

Needs Rust 1.95 or newer. The client build also needs the generated mod-API inputs at the repo root, which are included: `native-hooks.json`, `tools/native-hook-fragments/`, and `packaging/`. Don't delete them.

On Windows, `_build.and.deploy.ps1` builds both binaries and copies them into the server folder. From the repo:

```powershell
.\_build.and.deploy.ps1           # build, copy, verify
.\_build.and.deploy.ps1 -Start    # also start the server
```

To build the Linux binaries from Windows through WSL, use `scripts\build-wsl.ps1`. It writes them to `dist\linux`. Ubuntu 24.04 is the tested distro. On a Linux machine, `scripts/build-linux.sh` does the same build.

## Anarchy extras

These are optional and configured in the `[anarchy]` section of `server.toml`:

- Cracked login with `/register`, `/login`, and `/changepassword`, with rate limits.
- `/whisper`, `/ignore`, `/ignorelist`, `/discord`, `/addsocial`, `/skin set`, and `/worldstats`.
- Homes with `/sethome`, `/home`, `/delhome`, and `/homes`, up to 5 per player.
- `/rtp` (also `/randomtp`) and `/playerstats [name]`. A random spot is picked ahead of time, so `/rtp` and respawns move the player once, to a spot whose ground is loaded.
- A custom tab list header and footer.
- `world_border = false` turns the world border off. The saved border is ignored and `/worldborder` changes are undone each tick.
- A dupe chain event, a Christmas event that is off unless set in `[anarchy]`.

Run `/help` in game for the full list.

## Chat protection

Three checks run on chat, configured in the `[chat_filter]` section of `server.toml`:

- **Advert and spam screen:** on by default. Blocks web addresses, server addresses, and advertising phrases. Also blocks spam, meaning more than 5 messages in 5 seconds or the same message 3 times in 30 seconds. Three hits within 10 minutes gives a silent 60-minute mute: the player's messages show only to themselves.
- **Bot detection:** kicks a player who chats at a steady clip (8 or more messages in 2 minutes) while staying within 1 block of one spot for 2 minutes. Operators (permission level 2 and up) are exempt. The limits are constants in `crates/horizon-client/src/lan/hub/bot_watch.rs`.

## Storage

The dedicated server uses Linear v2 (`.linear`) for terrain, entities, and POI. It rejects worlds with legacy region files during startup.

Run `horizon-world convert-linear-v2 <world-folder> --dry-run` to verify conversion, or substitute a new staging directory for `--dry-run` to write and verify region files.

## License

MIT. See `LICENSE`.

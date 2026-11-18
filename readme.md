# RSAnarchy Server

A server fork of [Horizon](https://github.com/horizon-rust/horizon), a Rust port of Minecraft Java Edition 26.3, run for .RS Anarchy.

## What's in the repo

The repo still contains the full Horizon workspace, client included. The client isn't removed yet:

- **Server:** `horizon-server` (the supervisor you run), with `horizon-server-files`, `horizon-chat-filter`, `horizon-anticheat`, `horizon-protocol`, and `horizon-world`.
- **Engine:** `horizon-client` is also the engine. The server launches it to load the world and run the game, so it has to be built and deployed next to the server. It also still contains the client's rendering, input, and UI code.

Removing the client code from the engine is planned but not done.

## Building

Needs Rust 1.95 or newer. The client build also needs the generated mod-API inputs at the repo root, which are included: `native-hooks.json`, `tools/native-hook-fragments/`, and `packaging/`. Don't delete them.

On Windows, `_build.and.deploy.ps1` builds both binaries and copies them into the server folder. From the repo:

```powershell
.\_build.and.deploy.ps1           # build, copy, verify
.\_build.and.deploy.ps1 -Start    # also start the server
```

To build the Linux binaries from Windows through WSL, use `scripts\build-wsl.ps1`. It writes them to `dist\linux`. Ubuntu 24.04 is the tested distro. On a Linux machine, `scripts/build-linux.sh` does the same build.

## Deploy layout

The deploy script writes to `C:\Users\liqui\Desktop\RSAnarchy`:

- `rsanarchy.server\`: the binaries, `horizon-server.exe` and `horizon-client.exe` (the engine). The script only writes here, and keeps the previous copies as `.previous`.
- `rsanarchy\`: the server itself, with the world, `server.toml`, `server.properties`, and logs. The script never copies into or deletes from it.

The script refuses to copy while a server is running. Stop the server first with `stop` in its console, or pass `-StopRunning`.

## Running

Run `horizon-server run` from the server folder (`rsanarchy\`). Settings are in `server.toml`.

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

- **Slur filter:** blocks words from its list. By default the second hit in 10 minutes mutes for 10 minutes, and the third bans for 24 hours. Single words and swears are only stopped by this list.
- **Advert and spam screen:** on by default. Blocks web addresses, server addresses, and advertising phrases. Also blocks spam, meaning more than 5 messages in 5 seconds or the same message 3 times in 30 seconds. Three hits within 10 minutes gives a silent 60-minute mute: the player's messages show only to themselves.
- **Bot detection:** kicks a player who chats at a steady clip (8 or more messages in 2 minutes) while staying within 1 block of one spot for 2 minutes. Operators (permission level 2 and up) are exempt. The limits are constants in `crates/horizon-client/src/lan/hub/bot_watch.rs`.

## Storage

The dedicated server uses Linear v2 (`.linear`) for terrain, entities, and POI. It rejects worlds with legacy region files during startup.

Run `horizon-world convert-linear-v2 <world-folder> --dry-run` to verify conversion, or substitute a new staging directory for `--dry-run` to write and verify region files. Staging contains only converted regions. Keep the original world as a backup and preserve the rest of the world, including player data and `level.dat`, when installing the staged regions with the server stopped.

## License

MIT. See `LICENSE`.

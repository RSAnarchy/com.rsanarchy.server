# RSAnarchy Server

A server-only fork of [Horizon](https://github.com/horizon-rust/horizon), a Rust port of Minecraft Java Edition 26.3, for .RS Anarchy.

> **Note:** This is a server-only fork. All client, launcher, shader, and GUI crates have been removed.

## Building

```powershell
cargo build --profile fast -p horizon-server
```

## Running

Run `horizon-server run` from the server folder. Settings are in `server.toml`, and the anarchy extras are configured in its `[anarchy]` section.

## Anarchy extras

Optional server features, off unless enabled in `server.toml`:

- Cracked login: `/register`, `/login`, `/changepassword`, with rate limits.
- `/whisper`, `/ignore`, `/ignorelist`, `/discord`, `/addsocial`, `/skin set`, `/worldstats`.
- Homes: `/sethome`, `/home`, `/delhome`, `/homes` (5 per player).
- `/rtp` (or `/randomtp`) and `/playerstats [name]`.
- A custom tab list header and footer.
- A dupe chain event (off by default, see `[anarchy]` in `server.toml`).

Run `/help` in game for the full list.

## Linear Region Format

Linear region file support is included but WIP
(**[Linear region file format](https://github.com/xymb-endcrystalme/LinearRegionFileFormatTools)**)

## License

MIT

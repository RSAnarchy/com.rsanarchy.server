//! `[anarchy]`, the anarchy-server extras built in (no plugin), off unless
//! server.toml turns them on:
//!
//! - the cracked login (`/register`, `/login`, `/changepassword`): a player
//!   with an account is held, from the moment they are placed, in the login
//!   dimension (the End, at `login_spot`) in spectator mode, frozen (the hub
//!   drops their moves), vanished (nobody below level 4 sees them), outside
//!   chat and every command but the login ones, until they authenticate;
//!   someone without an account registers first. Nobody on the server learns
//!   where they stand: the login dimension shows the game's own End, never
//!   the map. The hold ends at the timeout (`login_timeout_seconds`) with a
//!   kick, like the queue's, and ends the moment they authenticate with a
//!   teleport to the world spawn.
//! - `/ignore` and `/ignorelist`: whose chat, emotes, whispers and character
//!   lines a player does not want. The hub drops them on the way out.
//! - `/whisper <player> <message>`: `/msg`'s alias.
//! - `/discord`, `/addsocial`: the links `[anarchy]` carries.
//! - `/skin set <nickname>`: the named Mojang profile's skin on the player.
//! - `/worldstats`: how many players the server has spawned at least once,
//!   how old the map is and what the world weighs on disk.
//!
//! Passwords are never stored or logged in clear: a salted, stretched SHA-256
//! goes into `passwords.db`, a SQLite database beside server.properties, and every reply says
//! no more than the commands do. All lines are translation keys with the
//! game's English as their fallback (`game/chat_filter.rs`'s `Reply`).
use super::{chat_filter::Reply, dedicated, lan::LanHost, server_travel::LevelTeleport, Dim, Game};
use horizon_client::{
    containers::BlockEntity, lan::data::component_nbt, mod_api::NativeCommand, text::{self, Text},
};
use horizon_protocol::packets::play::{self, Clientbound as C};
use horizon_server_files::user_lists::NameAndId;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The commands `anarchy_command` runs; `/whisper` is rewritten to `/msg`
/// before this is asked.
pub(super) const ANARCHY_COMMANDS: [&str; 17] = [
    "help",
    "sethome",
    "home",
    "delhome",
    "homes",
    "rtp",
    "randomtp",
    "playerstats",
    "ignore",
    "ignorelist",
    "register",
    "login",
    "changepassword",
    "discord",
    "addsocial",
    "skin",
    "worldstats",
];

/// The commands a held player may run.
pub(super) const LOGIN_COMMANDS: [&str; 4] = ["register", "login", "changepassword", "help"];

/// The password's stretch (salted SHA-256 rounds, after the first).
const PASSWORD_ROUNDS: u32 = 2048;
/// A password's shortest and longest length.
const PASSWORD_MIN: usize = 4;
const PASSWORD_MAX: usize = 64;
/// `/skin set`'s lookups a player may start, apart in time.
const SKIN_COOLDOWN: Duration = Duration::from_secs(10);
/// Wrong passwords from one address before that address is blocked from `/login`.
const IP_ATTEMPTS: u32 = 3;
/// How long an address stays blocked from `/login` after `IP_ATTEMPTS` wrong passwords.
const IP_BLOCK: Duration = Duration::from_secs(15 * 60);
/// Addresses that must each run out their `IP_ATTEMPTS` on one account to lock it.
const ACCOUNT_GUESSERS: usize = 2;
/// How long an account stays locked after that, refusing even the right password.
const ACCOUNT_LOCK: Duration = Duration::from_secs(60 * 60);

/// A held player's state before the hold: saved in place of the hold's own state.
#[derive(Clone)]
pub(super) struct HeldSave {
    pub(super) position: [f64; 3],
    pub(super) dimension: String,
    pub(super) mode: i32,
}

/// The `/login` brakes: per address, and per account when several addresses guess at it.
/// In memory, so a restart clears them.
#[derive(Default)]
pub(super) struct LoginGuard {
    addresses: HashMap<IpAddr, AddressRecord>,
    accounts: HashMap<u128, AccountRecord>,
}

#[derive(Default)]
struct AddressRecord {
    failures: u32,
    blocked_until: Option<Instant>,
}

#[derive(Default)]
struct AccountRecord {
    /// Wrong passwords per address, for this account.
    failures: HashMap<IpAddr, u32>,
    locked_until: Option<Instant>,
}

/// The brake that stops a `/login` before, or a wrong password sets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Brake {
    None,
    Address,
    Account,
}

impl LoginGuard {
    /// Whether `ip` may try a password for `uuid` now. Expired blocks are cleared.
    pub(super) fn gate(&mut self, ip: IpAddr, uuid: u128, now: Instant) -> Brake {
        if let Some(account) = self.accounts.get_mut(&uuid) {
            match account.locked_until {
                Some(until) if now < until => return Brake::Account,
                Some(_) => {
                    account.locked_until = None;
                    account.failures.clear();
                }
                None => {}
            }
        }
        if let Some(address) = self.addresses.get_mut(&ip) {
            match address.blocked_until {
                Some(until) if now < until => return Brake::Address,
                Some(_) => *address = AddressRecord::default(),
                None => {}
            }
        }
        Brake::None
    }

    /// A wrong password from `ip` for `uuid`. Returns the brake this failure set.
    pub(super) fn failed(&mut self, ip: IpAddr, uuid: u128, now: Instant) -> Brake {
        let address = self.addresses.entry(ip).or_default();
        address.failures += 1;
        let address_blocked = address.failures >= IP_ATTEMPTS;
        if address_blocked {
            address.blocked_until.get_or_insert(now + IP_BLOCK);
        }
        let account = self.accounts.entry(uuid).or_default();
        *account.failures.entry(ip).or_insert(0) += 1;
        let guessers = account.failures.values().filter(|n| **n >= IP_ATTEMPTS).count();
        if guessers >= ACCOUNT_GUESSERS {
            account.locked_until = Some(now + ACCOUNT_LOCK);
            account.failures.clear();
            return Brake::Account;
        }
        if address_blocked { Brake::Address } else { Brake::None }
    }

    /// A right password from `ip` for `uuid`: its wrong passwords are forgiven.
    pub(super) fn succeeded(&mut self, ip: IpAddr, uuid: u128) {
        self.addresses.remove(&ip);
        if let Some(account) = self.accounts.get_mut(&uuid) {
            account.failures.remove(&ip);
        }
    }
}

/// How often the hold reminds a held player of `/login` (ticks).
const PROMPT_EVERY: u64 = 600;

/// server.toml's `[anarchy]` section.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct AnarchyFile {
    pub anarchy: AnarchyConfig,
}
impl Default for AnarchyFile {
    fn default() -> Self {
        Self {
            anarchy: AnarchyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct AnarchyConfig {
    pub enabled: bool,
    pub discord: String,
    pub social: Vec<String>,
    pub login_timeout_seconds: u64,
    pub login_spot: [f64; 3],
    /// The dupe chain (a christmas event): hoppers within `dupe_radius` of a sign with
    /// the `dupe_sign` text double what they move. Off unless set; `dupe_ends_unix` (0:
    /// no end) turns it off after that time.
    pub dupe_chain: bool,
    pub dupe_sign: String,
    pub dupe_radius: f64,
    pub dupe_ends_unix: u64,
}
impl Default for AnarchyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discord: String::new(),
            social: Vec::new(),
            login_timeout_seconds: 120,
            login_spot: [100.5, 50.0, 0.5],
            dupe_chain: false,
            dupe_sign: "dupe".to_owned(),
            dupe_radius: 10.0,
            dupe_ends_unix: 0,
        }
    }
}

/// The config and the state of `[anarchy]`: the password store, the login
/// attempts and the skin lookups' cooldowns, and the game mode a held player
/// had (back to them on authentication).
pub(super) struct Anarchy {
    pub config: AnarchyConfig,
    pub store: Store,
    login: LoginGuard,
    skin_attempts: HashMap<u128, Instant>,
    /// The game mode of each held player (spectator while held).
    held_modes: HashMap<u128, i32>,
    /// Where each held player stood before the hold (their overworld feet): release puts them back here.
    held_saves: HashMap<u128, HeldSave>,
    /// Players waiting for a random spawn's chunk to load (the spot is searched each tick).
    pub(super) pending_spawn: Vec<u128>,
    /// Ticks of no damage left after a random landing (the reference's nodamagetime).
    pub(super) grace: HashMap<u128, u32>,
    /// Whether a hopper is a dupe hopper, for a few seconds (see `dupe_chain_at`).
    pub(super) dupe_cache: HashMap<[i32; 3], (Instant, bool)>,
    /// Online sessions: when each player's current session began (for playtime).
    pub(super) sessions: HashMap<u128, Instant>,
    /// When each player last used `/rtp` (its cooldown).
    pub(super) rtp_last: HashMap<u128, Instant>,
}

/// The legacy `passwords.json`, read once into the database by `Store::open`.
#[derive(Deserialize)]
struct LegacyStore {
    players: Vec<LegacyRecord>,
}

#[derive(Deserialize)]
struct LegacyRecord {
    id: String,
    name: String,
    salt: String,
    hash: String,
    #[serde(default)]
    ignores: Vec<LegacyIgnored>,
}

#[derive(Deserialize)]
struct LegacyIgnored {
    id: String,
    name: String,
}

/// The schema: accounts and ignores are separate, so an ignore list never
/// needs an account (and never creates one with a password nobody knows).
/// Ids are the 16 big-endian bytes of the UUID, the primary key of each row.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    uuid BLOB PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    salt BLOB NOT NULL,
    hash BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ignores (
    viewer BLOB NOT NULL,
    target BLOB NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (viewer, target)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS homes (
    owner BLOB NOT NULL,
    name TEXT NOT NULL COLLATE NOCASE,
    dim TEXT NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    z REAL NOT NULL,
    yaw REAL NOT NULL,
    pitch REAL NOT NULL,
    PRIMARY KEY (owner, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS stats (
    owner BLOB PRIMARY KEY NOT NULL,
    first_join INTEGER NOT NULL,
    playtime INTEGER NOT NULL
) WITHOUT ROWID;";

/// The salts are 16 random bytes; the hash is the salt stretched over the
/// password (`PASSWORD_ROUNDS` further rounds of SHA-256).
fn hash_password(salt: &[u8], password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    let mut hash: [u8; 32] = hasher.finalize().into();
    for _ in 0..PASSWORD_ROUNDS {
        hash = Sha256::digest(hash).into();
    }
    hash
}

/// Equal bytes, compared without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The database key of a UUID.
fn key(uuid: u128) -> Vec<u8> {
    uuid.to_be_bytes().to_vec()
}

/// A UUID from its 16 stored bytes.
fn from_key(bytes: &[u8]) -> Option<u128> {
    <[u8; 16]>::try_from(bytes).ok().map(u128::from_be_bytes)
}

/// A UUID from the 32 hex digits the legacy JSON used.
fn parse_uuid(text: &str) -> Option<u128> {
    u128::from_str_radix(text, 16).ok()
}

/// The game mode a held player goes back to. Their saved data can say spectator (it was
/// saved while held, in spectator), and a spectator must not come back out of the hold, so
/// that becomes survival.
fn restored_mode(saved: i32) -> i32 {
    if saved == horizon_client::lan::guest::SPECTATOR {
        0
    } else {
        saved
    }
}

/// The random spawn (from random-spawn, by josvth): a new player, or one with
/// no bed, lands at a random point within `RANDOM_SPAWN_RADIUS` of 0,0.
pub(super) const RANDOM_SPAWN_RADIUS: f64 = 1000.0;

/// How far above the found ground a random spawn lands the player (the
/// reference's `location.add(0, 3, 0)`), so they do not land inside a block.
pub(super) const RANDOM_SPAWN_LIFT: f64 = 3.0;

/// No damage after a random landing: 5 seconds (the reference's nodamagetime).
pub(super) const RANDOM_SPAWN_GRACE_TICKS: u32 = 100;

/// The random spawn's attempts before it gives up for this tick.
pub(super) const RANDOM_SPAWN_ATTEMPTS: u32 = 64;

/// What a column of the world holds, as the spawn search reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Column {
    /// The chunk is not loaded yet: request it, and try again later.
    Loading,
    /// The column's top block cannot hold a player (water, lava, leaves,
    /// cactus and the like, the reference's blacklist).
    Unsafe,
    /// The top solid block's Y.
    Ground(i32),
}

/// The outcome of one spawn search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Pick {
    /// Feet to stand at.
    Found([f64; 3]),
    /// The chunk at this column must load before the search can finish.
    Waiting([i32; 2]),
    /// No ground in `RANDOM_SPAWN_ATTEMPTS` tries (the caller falls back).
    Exhausted,
}

/// A point uniform over the disc of `radius` (square root of the radius
/// sample, so the area is even; the reference's plain radius clusters the
/// points at the centre). `unit` yields numbers in [0, 1).
pub(super) fn disc_point(radius: f64, unit: &mut impl FnMut() -> f64) -> [i32; 2] {
    let r = unit().sqrt() * radius;
    let phi = unit() * std::f64::consts::TAU;
    [(r * phi.cos()).floor() as i32, (r * phi.sin()).floor() as i32]
}

/// The reference's chooseSpawn loop: random columns until one has ground
/// that is safe to stand on. `column` reads a column (`x`, `z`).
pub(super) fn pick_random_spawn(
    unit: &mut impl FnMut() -> f64,
    column: impl Fn(i32, i32) -> Column,
) -> Pick {
    for _ in 0..RANDOM_SPAWN_ATTEMPTS {
        let [x, z] = disc_point(RANDOM_SPAWN_RADIUS, unit);
        match column(x, z) {
            Column::Ground(y) => {
                return Pick::Found([
                    f64::from(x) + 0.5,
                    f64::from(y) + 1.0 + RANDOM_SPAWN_LIFT,
                    f64::from(z) + 0.5,
                ]);
            }
            Column::Loading => return Pick::Waiting([x >> 4, z >> 4]),
            Column::Unsafe => {}
        }
    }
    Pick::Exhausted
}

/// Blocks a player cannot stand on at a random spawn (the reference's blacklist:
/// fire, leaves, cactus). Water and lava are caught by their fluid.
pub(super) fn unsafe_ground(name: &str) -> bool {
    matches!(name, "minecraft:fire" | "minecraft:soul_fire" | "minecraft:cactus")
        || name.ends_with("_leaves")
}

/// A number in [0, 1) from the OS random source.
pub(super) fn os_unit() -> f64 {
    let mut bytes = [0u8; 8];
    // The OS random source does not fail on a running machine; if it did, a
    // constant keeps the search finite (it would fall back to the world spawn).
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// The hex of a fixed-length field (the legacy JSON's salts and hashes).
fn decode_hex(text: &str, len: usize) -> Option<Vec<u8>> {
    if text.len() != len * 2 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..len)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect();
    bytes.filter(|b| b.len() == len)
}

impl Game<'_> {
    /// server.toml's `[anarchy]` (game/dedicated.rs reads it at the start):
    /// the config and its state, or `None` while it is off. The password
    /// database opens beside server.properties; if it cannot, the extras stay
    /// off and the console says why, rather than running with no accounts.
    pub(super) fn load_anarchy(&mut self) {
        let file = std::fs::read_to_string("server.toml")
            .ok()
            .and_then(|text| toml::from_str::<AnarchyFile>(&text).ok())
            .filter(|file| file.anarchy.enabled);
        self.anarchy = file.and_then(|file| match Store::open() {
            Ok(store) => Some(Anarchy {
                config: file.anarchy,
                store,
                login: LoginGuard::default(),
                skin_attempts: HashMap::new(),
                held_modes: HashMap::new(),
                held_saves: HashMap::new(),
                pending_spawn: Vec::new(),
                grace: HashMap::new(),
                dupe_cache: HashMap::new(),
                sessions: HashMap::new(),
                rtp_last: HashMap::new(),
            }),
            Err(e) => {
                dedicated::log("ERROR", &format!("Anarchy off: the password database did not open: {e}"));
                None
            }
        });
        if self.anarchy.is_some() {
            dedicated::log("INFO", "Anarchy extras on (server.toml [anarchy])");
        }
    }

    /// A random spawn's feet from the loaded world (random-spawn's chooseSpawn):
    /// safe ground, three blocks up. A column whose chunk is not loaded asks
    /// the streaming for it (`streaming.center`) and gives None this time, so
    /// a later try finds it.
    pub(super) fn anarchy_random_feet(&mut self) -> Option<[f64; 3]> {
        let picked = {
            let world = &self.world;
            let streaming = &self.streaming;
            let facts = self.res.facts();
            let air = world.air;
            pick_random_spawn(&mut os_unit, |x, z| {
                if !streaming.is_lit(x >> 4, z >> 4) {
                    return Column::Loading;
                }
                for y in (world.bottom_y()..world.top_y()).rev() {
                    let state = world.block(x, y, z);
                    if state == air {
                        continue;
                    }
                    // Water or lava, fire, leaves or cactus on top is not ground to stand on.
                    let block = facts.get(state);
                    if !matches!(block.fluid, horizon_client::blocks::Fluid::None)
                        || unsafe_ground(&block.name)
                    {
                        return Column::Unsafe;
                    }
                    return Column::Ground(y);
                }
                Column::Unsafe
            })
        };
        match picked {
            Pick::Found(feet) => Some(feet),
            Pick::Waiting([cx, cz]) => {
                self.streaming.center = (cx, cz);
                None
            }
            Pick::Exhausted => None,
        }
    }

    /// A player's session begins: first join recorded, the clock for playtime starts.
    pub(super) fn anarchy_session_start(&mut self, uuid: u128) {
        if let Some(anarchy) = self.anarchy.as_mut() {
            if let Err(e) = anarchy.store.ensure_stats(uuid, unix_now()) {
                dedicated::log("ERROR", &format!("stats not recorded: {e}"));
            }
            anarchy.sessions.insert(uuid, Instant::now());
        }
    }

    /// A player's session ends (they left): its time is added to their playtime.
    pub(super) fn anarchy_session_end(&mut self, uuid: u128) {
        if let Some(anarchy) = self.anarchy.as_mut()
            && let Some(start) = anarchy.sessions.remove(&uuid)
            && let Err(e) = anarchy.store.add_playtime(uuid, start.elapsed().as_secs() as i64)
        {
            dedicated::log("ERROR", &format!("playtime not saved: {e}"));
        }
    }

    /// `/rtp` and `/randomtp`: a random spot like a respawn, without the respawn. A
    /// 30-second cooldown: each search loads chunks.
    pub(super) fn anarchy_rtp(&mut self, host: &mut LanHost, conn: u32, uuid: u128) -> Vec<Reply> {
        const COOLDOWN: Duration = Duration::from_secs(30);
        if let Some(last) = self.anarchy.as_ref().and_then(|a| a.rtp_last.get(&uuid).copied())
            && last.elapsed() < COOLDOWN
        {
            return vec![Reply::literal("Wait a moment before the next /rtp")];
        }
        match self.anarchy_random_feet() {
            Some(feet) => {
                if let Some(anarchy) = self.anarchy.as_mut() {
                    anarchy.rtp_last.insert(uuid, Instant::now());
                }
                self.anarchy_grace_start(uuid);
                self.level_teleports.push(LevelTeleport {
                    conn,
                    dim: Dim::Overworld,
                    at: feet,
                    rotation: None,
                    vehicle: None,
                    cause: None,
                });
                self.run_level_teleports(host);
                vec![Reply::literal("Teleported to a random spot")]
            }
            None => vec![Reply::literal("Finding a spot: try /rtp again in a moment")],
        }
    }

    /// `/playerstats [name]`: the join date, playtime and homes of the player or of
    /// another who has joined this server.
    pub(super) fn anarchy_playerstats(&self, host: &LanHost, uuid: u128, name: &str, args: &[&str]) -> Vec<Reply> {
        let (who, id) = match args.first() {
            None => (name.to_owned(), uuid),
            Some(target) => match self.resolve_name(target) {
                Some(found) => (found.name, found.id),
                None => return vec![Reply::literal(format!("No player named {target} has joined"))],
            },
        };
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        let stats = match anarchy.store.stats_of(id) {
            Ok(Some(stats)) => stats,
            Ok(None) => return vec![Reply::literal(format!("{who} has no stats yet"))],
            Err(e) => {
                dedicated::log("ERROR", &format!("playerstats failed: {e}"));
                return vec![unknown()];
            }
        };
        let (first_join, mut playtime) = stats;
        // The session in progress counts too.
        if let Some(start) = anarchy.sessions.get(&id) {
            playtime += start.elapsed().as_secs() as i64;
        }
        let homes = anarchy.store.home_names(id).map_or(0, |h| h.len());
        let online = host.hub.guests.iter().any(|g| g.uuid == id);
        let (y, mo, d, h, mi) = utc_parts(first_join);
        vec![
            Reply::literal(format!("--- {who} ---")).with_color(horizon_client::text::GOLD),
            Reply::literal(format!("Joined: {y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC"))
                .with_color(horizon_client::text::GRAY),
            Reply::literal(format!("Playtime: {}", duration_text(playtime)))
                .with_color(horizon_client::text::GRAY),
            Reply::literal(format!("Homes: {homes}/{HOME_LIMIT}")).with_color(horizon_client::text::GRAY),
            Reply::literal(format!("Status: {}", if online { "online" } else { "offline" }))
                .with_color(horizon_client::text::GRAY),
        ]
    }

    /// Whether the hopper at `pos` is a dupe hopper now: the dupe chain is on, its
    /// event has not ended, and a sign with the dupe text is within the radius. Cached
    /// for five seconds per hopper, so a busy chain does not scan every block entity
    /// for each item.
    pub(super) fn dupe_chain_at(&mut self, pos: [i32; 3]) -> bool {
        let Some(config) = self.anarchy.as_ref().map(|a| a.config.clone()) else {
            return false;
        };
        if !config.dupe_chain || config.dupe_sign.trim().is_empty() {
            return false;
        }
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if config.dupe_ends_unix != 0 && now_unix >= config.dupe_ends_unix {
            return false;
        }
        let now = Instant::now();
        if let Some(anarchy) = self.anarchy.as_ref()
            && let Some((at, found)) = anarchy.dupe_cache.get(&pos)
            && now.duration_since(*at) < Duration::from_secs(5)
        {
            return *found;
        }
        let want = config.dupe_sign.trim().to_lowercase();
        let radius_sq = config.dupe_radius * config.dupe_radius;
        let found = self.world.block_entities.iter().any(|(key, entity)| {
            let BlockEntity::Sign(sign) = entity else {
                return false;
            };
            let (x, y, z) = *key;
            let d = [x - pos[0], y - pos[1], z - pos[2]]
                .iter()
                .map(|v| f64::from(*v) * f64::from(*v))
                .sum::<f64>();
            d <= radius_sq
                && [&sign.front, &sign.back].iter().any(|side| {
                    side.lines
                        .iter()
                        .any(|line| line.trim().to_lowercase() == want)
                })
        });
        if let Some(anarchy) = self.anarchy.as_mut() {
            anarchy.dupe_cache.insert(pos, (now, found));
        }
        found
    }

    /// A held player's pre-hold state, if they are held.
    pub(super) fn anarchy_held_save(&self, uuid: u128) -> Option<HeldSave> {
        self.anarchy.as_ref().and_then(|a| a.held_saves.get(&uuid).cloned())
    }

    /// A random landing: no damage for the grace window.
    pub(super) fn anarchy_grace_start(&mut self, uuid: u128) {
        if let Some(anarchy) = self.anarchy.as_mut() {
            anarchy.grace.insert(uuid, RANDOM_SPAWN_GRACE_TICKS);
        }
    }

    /// Whether `uuid` is inside the grace window (takes no damage).
    pub(super) fn anarchy_graced(&self, uuid: u128) -> bool {
        self.anarchy.as_ref().is_some_and(|a| a.grace.contains_key(&uuid))
    }

    /// Queues a player for a random spawn whose chunk is not loaded yet.
    pub(super) fn anarchy_pend(&mut self, uuid: u128) {
        if let Some(anarchy) = self.anarchy.as_mut()
            && !anarchy.pending_spawn.contains(&uuid)
        {
            anarchy.pending_spawn.push(uuid);
        }
    }
    /// Whether the anarchy extras are on (a dedicated server's `[anarchy]`).
    pub(super) fn anarchy_on(&self) -> bool {
        self.options.dedicated.is_some() && self.anarchy.is_some()
    }
}

/// The password store: `passwords.db`, a SQLite database beside
/// server.properties. WAL journaling, so a save never leaves a half-written
/// file and reads do not wait on writes. Every call is one statement or one
/// transaction; an error is returned, never swallowed.
pub(super) struct Store {
    conn: Connection,
}

impl Store {
    /// The database beside server.properties, created with its schema, and
    /// any legacy `passwords.json` moved into it.
    pub(super) fn open() -> Result<Self, String> {
        let conn = Connection::open("passwords.db").map_err(|e| e.to_string())?;
        Self::init(conn)
    }

    #[cfg(test)]
    fn memory() -> Result<Self, String> {
        Self::init(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        let err = |e: rusqlite::Error| e.to_string();
        conn.busy_timeout(Duration::from_secs(5)).map_err(err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(err)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(err)?;
        conn.execute_batch(SCHEMA).map_err(err)?;
        let store = Self { conn };
        store.migrate_home_names()?;
        store.migrate_legacy();
        Ok(store)
    }

    /// Home names match without regard to case: a database made before the column was
    /// `COLLATE NOCASE` is rebuilt with it, once (`user_version` 1). Where two names differ only
    /// by case, the first (in name order) is kept and the other is logged, not dropped silently.
    fn migrate_home_names(&self) -> Result<(), String> {
        let err = |e: rusqlite::Error| e.to_string();
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(err)?;
        if version >= 1 {
            return Ok(());
        }
        let before: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM homes", [], |row| row.get(0))
            .map_err(err)?;
        self.conn
            .execute_batch(
                "BEGIN;
                 CREATE TABLE homes_nocase (
                     owner BLOB NOT NULL,
                     name TEXT NOT NULL COLLATE NOCASE,
                     dim TEXT NOT NULL,
                     x REAL NOT NULL,
                     y REAL NOT NULL,
                     z REAL NOT NULL,
                     yaw REAL NOT NULL,
                     pitch REAL NOT NULL,
                     PRIMARY KEY (owner, name)
                 ) WITHOUT ROWID;
                 INSERT OR IGNORE INTO homes_nocase
                     SELECT owner, name, dim, x, y, z, yaw, pitch FROM homes ORDER BY name;
                 DROP TABLE homes;
                 ALTER TABLE homes_nocase RENAME TO homes;
                 PRAGMA user_version = 1;
                 COMMIT;",
            )
            .map_err(err)?;
        let after: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM homes", [], |row| row.get(0))
            .map_err(err)?;
        if after != before {
            dedicated::log(
                "WARN",
                &format!(
                    "{} home(s) had a name that differed from another only by case and were not kept",
                    before - after
                ),
            );
        }
        Ok(())
    }

    /// Moves a `passwords.json` from before the database into it, once, and
    /// renames it out of the way. The import ignores rows already present.
    fn migrate_legacy(&self) {
        let path = "passwords.json";
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        match self.import_legacy(&text) {
            Ok(moved) => {
                let _ = std::fs::rename(path, "passwords.json.migrated");
                dedicated::log(
                    "INFO",
                    &format!("Moved {moved} account(s) from passwords.json into passwords.db"),
                );
            }
            Err(e) => dedicated::log("ERROR", &format!("passwords.json was not moved: {e}")),
        }
    }

    /// The legacy records' accounts and ignores, in one transaction.
    fn import_legacy(&self, text: &str) -> Result<usize, String> {
        let err = |e: rusqlite::Error| e.to_string();
        let legacy: LegacyStore = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let tx = self.conn.unchecked_transaction().map_err(err)?;
        let mut moved = 0;
        for record in legacy.players {
            let (Some(uuid), Some(salt), Some(hash)) = (
                parse_uuid(&record.id),
                decode_hex(&record.salt, 16),
                decode_hex(&record.hash, 32),
            ) else {
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO accounts (uuid, name, salt, hash) VALUES (?1, ?2, ?3, ?4)",
                params![key(uuid), record.name, salt, hash],
            )
            .map_err(err)?;
            moved += 1;
            for ignored in record.ignores {
                if let Some(target) = parse_uuid(&ignored.id) {
                    tx.execute(
                        "INSERT OR IGNORE INTO ignores (viewer, target, name) VALUES (?1, ?2, ?3)",
                        params![key(uuid), key(target), ignored.name],
                    )
                    .map_err(err)?;
                }
            }
        }
        tx.commit().map_err(err)?;
        Ok(moved)
    }

    /// Whether `uuid` has an account.
    fn has_account(&self, uuid: u128) -> bool {
        matches!(
            self.conn
                .query_row("SELECT 1 FROM accounts WHERE uuid = ?1", params![key(uuid)], |_| Ok(()))
                .optional(),
            Ok(Some(()))
        )
    }

    /// A new account with a random salt and the stretched hash. False when
    /// the account exists already (the insert is ignored, never overwritten).
    fn register(&self, user: &NameAndId, password: &str) -> Result<bool, String> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO accounts (uuid, name, salt, hash) VALUES (?1, ?2, ?3, ?4)",
                params![
                    key(user.id),
                    user.name,
                    salt.to_vec(),
                    hash_password(&salt, password).to_vec()
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(inserted == 1)
    }

    /// Whether `password` is the account's.
    fn check(&self, uuid: u128, password: &str) -> bool {
        let row: rusqlite::Result<(Vec<u8>, Vec<u8>)> = self.conn.query_row(
            "SELECT salt, hash FROM accounts WHERE uuid = ?1",
            params![key(uuid)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match row {
            Ok((salt, hash)) => same(&hash, &hash_password(&salt, password)),
            Err(_) => false,
        }
    }

    /// A new salt and hash for the account (the old password verified by the caller).
    fn set_password(&self, uuid: u128, password: &str) -> Result<bool, String> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
        let changed = self
            .conn
            .execute(
                "UPDATE accounts SET salt = ?2, hash = ?3 WHERE uuid = ?1",
                params![key(uuid), salt.to_vec(), hash_password(&salt, password).to_vec()],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed == 1)
    }

    /// Whom `viewer` ignores, with the names they were known by.
    fn ignores_of(&self, viewer: u128) -> Result<Vec<(u128, String)>, String> {
        let err = |e: rusqlite::Error| e.to_string();
        let mut stmt = self
            .conn
            .prepare("SELECT target, name FROM ignores WHERE viewer = ?1 ORDER BY name")
            .map_err(err)?;
        let rows = stmt
            .query_map(params![key(viewer)], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(err)?;
        let mut out = Vec::new();
        for row in rows {
            let (target, name) = row.map_err(err)?;
            if let Some(target) = from_key(&target) {
                out.push((target, name));
            }
        }
        Ok(out)
    }

    /// Adds or takes back whom `viewer` ignores; whether it is ignored now.
    fn toggle_ignore(&self, viewer: u128, user: &NameAndId) -> Result<bool, String> {
        let err = |e: rusqlite::Error| e.to_string();
        let removed = self
            .conn
            .execute(
                "DELETE FROM ignores WHERE viewer = ?1 AND target = ?2",
                params![key(viewer), key(user.id)],
            )
            .map_err(err)?;
        if removed > 0 {
            return Ok(false);
        }
        self.conn
            .execute(
                "INSERT INTO ignores (viewer, target, name) VALUES (?1, ?2, ?3)",
                params![key(viewer), key(user.id), user.name],
            )
            .map_err(err)?;
        Ok(true)
    }

    /// A player's first join (unix seconds) and total playtime (seconds), from their first join.
    fn ensure_stats(&self, owner: u128, now: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO stats (owner, first_join, playtime) VALUES (?1, ?2, 0)",
                params![key(owner), now],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Adds a finished session's seconds to a player's playtime.
    fn add_playtime(&self, owner: u128, secs: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE stats SET playtime = playtime + ?2 WHERE owner = ?1",
                params![key(owner), secs],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// `(first join, playtime)` of a player, if they have joined.
    fn stats_of(&self, owner: u128) -> Result<Option<(i64, i64)>, String> {
        self.conn
            .query_row(
                "SELECT first_join, playtime FROM stats WHERE owner = ?1",
                params![key(owner)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    /// `homes`: the names a player has, A to Z.
    fn home_names(&self, owner: u128) -> Result<Vec<String>, String> {
        let err = |e: rusqlite::Error| e.to_string();
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM homes WHERE owner = ?1 ORDER BY name")
            .map_err(err)?;
        let rows = stmt
            .query_map(params![key(owner)], |row| row.get::<_, String>(0))
            .map_err(err)?;
        rows.collect::<rusqlite::Result<Vec<String>>>().map_err(err)
    }

    /// One of a player's homes, by name.
    fn get_home(&self, owner: u128, name: &str) -> Result<Option<Home>, String> {
        self.conn
            .query_row(
                "SELECT name, dim, x, y, z, yaw, pitch FROM homes WHERE owner = ?1 AND name = ?2",
                params![key(owner), name],
                |row| {
                    Ok(Home {
                        name: row.get(0)?,
                        dim: row.get(1)?,
                        at: [row.get(2)?, row.get(3)?, row.get(4)?],
                        rotation: [row.get::<_, f64>(5)? as f32, row.get::<_, f64>(6)? as f32],
                    })
                },
            )
            .optional()
            .map_err(|e| e.to_string())
    }

    /// Sets a home: a new name when the player has room (`limit`), else moves the
    /// home of that name.
    fn set_home(&self, owner: u128, home: &Home, limit: usize) -> Result<HomeSet, String> {
        let err = |e: rusqlite::Error| e.to_string();
        let exists = matches!(
            self.conn
                .query_row(
                    "SELECT 1 FROM homes WHERE owner = ?1 AND name = ?2",
                    params![key(owner), home.name],
                    |_| Ok(()),
                )
                .optional(),
            Ok(Some(()))
        );
        if !exists {
            let count: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM homes WHERE owner = ?1",
                    params![key(owner)],
                    |row| row.get(0),
                )
                .map_err(err)?;
            if usize::try_from(count).unwrap_or(usize::MAX) >= limit {
                return Ok(HomeSet::Full);
            }
        }
        self.conn
            .execute(
                "INSERT OR REPLACE INTO homes (owner, name, dim, x, y, z, yaw, pitch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    key(owner),
                    home.name,
                    home.dim,
                    home.at[0],
                    home.at[1],
                    home.at[2],
                    f64::from(home.rotation[0]),
                    f64::from(home.rotation[1])
                ],
            )
            .map_err(err)?;
        Ok(if exists { HomeSet::Updated } else { HomeSet::Created })
    }

    /// Deletes one of a player's homes; whether it existed.
    fn delete_home(&self, owner: u128, name: &str) -> Result<bool, String> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM homes WHERE owner = ?1 AND name = ?2",
                params![key(owner), name],
            )
            .map_err(|e| e.to_string())?;
        Ok(removed == 1)
    }

    /// The legacy JSON's accounts and ignores, for the import (tests too).
    #[cfg(test)]
    fn import(&self, text: &str) -> Result<usize, String> {
        self.import_legacy(text)
    }
}

/// The commands' usages in every player's command tree (everyone may run
/// them; `/skin`'s and the rest check nothing more).
pub(super) fn anarchy_usages() -> Vec<NativeCommand> {
    [
        "help",
        "whisper <player:player> <message:text>",
        "ignore <player:player>",
        "ignorelist",
        "register <password:word> <password2:word>",
        "login <password:word>",
        "changepassword <old:word> <new:word>",
        "discord",
        "addsocial",
        "skin set <nickname:word>",
        "worldstats",
        "sethome <name:word>",
        "home <name:word>",
        "delhome <name:word>",
        "homes",
        "rtp",
        "randomtp",
        "playerstats",
        "playerstats <name:word>",
    ]
    .into_iter()
    .map(|usage| {
        // `/help` is ours: it replaces vanilla's, so the list below is what a player sees.
        if usage == "help" {
            NativeCommand::new(usage).replace_vanilla(true)
        } else {
            NativeCommand::new(usage)
        }
    })
    .collect()
}

/// A player's home: where, and the way they faced.
#[derive(Clone)]
pub(super) struct Home {
    pub name: String,
    pub dim: String,
    pub at: [f64; 3],
    pub rotation: [f32; 2],
}

/// The outcome of setting a home.
pub(super) enum HomeSet {
    Created,
    Updated,
    Full,
}

/// Seconds since the Unix epoch.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// (year, month, day, hour, minute) in UTC for Unix seconds.
fn utc_parts(unix: i64) -> (i64, u32, u32, u32, u32) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + (month <= 2) as i64;
    (year, month, day, (secs / 3600) as u32, ((secs % 3600) / 60) as u32)
}

/// `3d 4h 5m` for a number of seconds.
fn duration_text(secs: i64) -> String {
    format!("{}d {}h {}m", secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60)
}

/// Homes per player.
pub(super) const HOME_LIMIT: usize = 5;

/// A home name: 1 to 16 letters, digits, `_` or `-`.
pub(super) fn home_name_ok(name: &str) -> bool {
    (1..=16).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The login bar: a purple boss bar at the top of a held player's screen with the
/// command to use, `/LOGIN` for an account, `/REGISTER` for a first time.
fn login_bar(uuid: u128, account: bool) -> C {
    let command = if account { "USE /LOGIN" } else { "USE /REGISTER" };
    C::BossEvent(play::BossEvent {
        id: uuid,
        action: play::BossAction::Add {
            name: component_nbt(&Text::literal(command).with_color(text::GOLD).bold().component()),
            progress: 1.0,
            // BossBarColor.PURPLE, BossBarOverlay.PROGRESS.
            color: 5,
            overlay: 0,
            darken: false,
            music: false,
            fog: false,
        },
    })
}

/// The login bar's removal, once the hold ends.
fn login_bar_clear(uuid: u128) -> C {
    C::BossEvent(play::BossEvent {
        id: uuid,
        action: play::BossAction::Remove,
    })
}

/// The `[anarchy]` lines a held player gets: the join prompt (or the same
/// line every `PROMPT_EVERY` ticks after) and the timeout kick's reason.
pub(super) fn login_prompt() -> horizon_world::nbt::Tag {
    component_nbt(&Reply::translation("horizon.anarchy.login.prompt", &[]).component())
}

pub(super) fn login_timeout_kick() -> horizon_world::nbt::Tag {
    component_nbt(&Reply::translation("horizon.anarchy.login.timeout", &[]).component())
}

impl Game<'_> {
    // ---- the login hold ------------------------------------------------

    /// The "log in first" line a held player's other commands get.
    pub(super) fn anarchy_login_required(&mut self, host: &mut LanHost, conn: u32) {
        host.hub.send_to(
            conn,
            &C::SystemChat(play::SystemChat {
                content: login_prompt(),
                overlay: false,
            }),
        );
    }

    /// The hold a just-joined player gets, once their player data is in and
    /// before they are placed (`lan_guest_joined`): the login dimension, the
    /// login spot, spectator (invisible, unable to act, safe from the void),
    /// and vanished from the Tab list and the world. The hub's
    /// `login_hold` freezes their packets; the timeout is `anarchy_tick`'s.
    /// Whether the player is held (the placement skips the in-place level).
    pub(super) fn anarchy_hold(&mut self, host: &mut LanHost, conn: u32) -> bool {
        if self.anarchy.is_none() {
            return false;
        };
        let Some(g) = host.hub.guest_mut(conn) else {
            return false;
        };
        let uuid = g.uuid;
        self.anarchy_session_start(uuid);
        // The state the player had before the hold: what the disk keeps (the hold is
        // not saved, so a disconnect while held does not restore the login void).
        let save = HeldSave {
            position: g.position,
            dimension: g.dimension.clone(),
            mode: restored_mode(g.game_mode),
        };
        if let Some(anarchy) = self.anarchy.as_mut() {
            anarchy.held_saves.insert(uuid, save);
        }
        // Players with an account log in; the rest register first. Both are
        // held the same way.
        self.anarchy
            .as_mut()
            .unwrap()
            .held_modes
            .insert(uuid, restored_mode(g.game_mode));
        g.game_mode = horizon_client::lan::guest::SPECTATOR;
        g.dimension = "horizon:login".to_owned();
        g.position = [0.5, 64.0, 0.5];
        g.previous_position = [0.5, 64.0, 0.5];
        g.yaw = 0.0;
        g.pitch = 0.0;
        g.head_yaw = 0.0;
        host.hub.login_hold.insert(uuid);
        host.hub.hidden.insert(uuid, 4);
        self.anarchy_load_ignores(host, uuid);
        let account = self.anarchy.as_ref().is_some_and(|a| a.store.has_account(uuid));
        if let Some(g) = host.hub.guest_mut(conn) {
            g.send(&C::SystemChat(play::SystemChat {
                content: login_prompt(),
                overlay: false,
            }));
            g.send(&login_bar(uuid, account));
        }
        true
    }

    /// The hold's per-second part (`tick_lan`): the reminders and the timeout
    /// kick, and the Tab list's header and footer (game/tablist.rs).
    pub(super) fn anarchy_tick(&mut self, host: &mut LanHost) {
        if let Some(anarchy) = self.anarchy.as_mut() {
            anarchy.grace.retain(|_, ticks| {
                *ticks = ticks.saturating_sub(1);
                *ticks > 0
            });
            anarchy
                .pending_spawn
                .retain(|uuid| host.hub.guests.iter().any(|g| g.uuid == *uuid));
        }
        // A random spawn waiting on its chunk: land the first queued player whose
        // ground has loaded (a held player waits for the release).
        let next = self.anarchy.as_ref().and_then(|a| {
            a.pending_spawn
                .iter()
                .copied()
                .find(|uuid| !host.hub.login_hold.contains(uuid))
        });
        if let Some(uuid) = next
            && let Some(feet) = self.anarchy_random_feet()
            && let Some(i) = host.hub.guests.iter().position(|g| g.uuid == uuid)
        {
            if let Some(anarchy) = self.anarchy.as_mut() {
                anarchy.pending_spawn.retain(|u| *u != uuid);
            }
            self.anarchy_grace_start(uuid);
            host.hub.teleport(i, Some(feet));
        }
        let held: Vec<(u32, u128)> = host
            .hub
            .guests
            .iter()
            .filter(|g| g.placed && host.hub.login_hold.contains(&g.uuid))
            .map(|g| (g.conn, g.uuid))
            .collect();
        if host.hub.ticks % 20 == 0 {
            let timeout = self
                .anarchy
                .as_ref()
                .map_or(120, |a| a.config.login_timeout_seconds)
                .max(5) as u64;
            for (conn, _) in &held {
                let g = host.hub.guests.iter().find(|g| g.conn == *conn);
                let overdue = g.is_some_and(|g| host.hub.ticks.saturating_sub(g.joined_tick)
                    > timeout * 20);
                if overdue {
                    host.hub.kick(*conn, login_timeout_kick());
                }
            }
            if self.tablist.is_some() {
                super::tablist::broadcast(self, host);
            }
        }
        // The reminder, every half minute.
        if host.hub.ticks % PROMPT_EVERY == 0 {
            for (conn, _) in held {
                host.hub.send_to(
                    conn,
                    &C::SystemChat(play::SystemChat {
                        content: login_prompt(),
                        overlay: false,
                    }),
                );
            }
        }
    }

    /// The hold ends: out of `login_hold` and the vanish, back to the game
    /// mode they had, and to the world spawn in the Overworld (the login
    /// dimension never keeps a position worth saving).
    fn anarchy_release(&mut self, host: &mut LanHost, conn: u32) {
        let Some(i) = host.hub.guests.iter().position(|g| g.conn == conn) else {
            return;
        };
        let uuid = host.hub.guests[i].uuid;
        host.hub.send_to(conn, &login_bar_clear(uuid));
        host.hub.login_hold.remove(&uuid);
        host.hub.hidden.remove(&uuid);
        // The inventory the hold kept back, now that they are in.
        host.hub.send_inventory(i);
        let mode = self
            .anarchy
            .as_mut()
            .map_or(0, |a| a.held_modes.remove(&uuid).unwrap_or(0));
        host.hub.set_game_mode(conn, mode, &self.world, Some(self.collision));
        // Back to where they stood before the hold (a new player's random spawn, a
        // returning player's saved spot); else TeleportTransition.createDefault's
        // world spawn column.
        let save = self.anarchy.as_mut().and_then(|a| a.held_saves.remove(&uuid));
        let spot = save
            .filter(|h| h.dimension == host.hub.config.dimension)
            .map(|h| h.position);
        let at = spot.unwrap_or_else(|| {
            self.level.world_spawn.map_or(
                self.spawn.map_or([0.5, 80.0, 0.5], |s| [s.x, s.y, s.z]),
                |[x, y, z]| [f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5],
            )
        });
        self.level_teleports.push(LevelTeleport {
            conn,
            dim: Dim::Overworld,
            at,
            rotation: None,
            vehicle: None,
            cause: None,
        });
        self.run_level_teleports(host);
    }

    /// One viewer's ignore list into the hub (their join, or an `/ignore`).
    pub(super) fn anarchy_load_ignores(&self, host: &mut LanHost, viewer: u128) {
        let Some(anarchy) = &self.anarchy else {
            return;
        };
        match anarchy.store.ignores_of(viewer) {
            Ok(list) => {
                let set = host.hub.ignored.entry(viewer).or_default();
                set.clear();
                set.extend(list.into_iter().map(|(id, _)| id));
            }
            Err(e) => dedicated::log("ERROR", &format!("ignore list not loaded: {e}")),
        }
    }

    // ---- the commands ---------------------------------------------------

    /// `/sethome <name>`: a home where the player stands.
    fn anarchy_sethome(&mut self, host: &mut LanHost, conn: u32, uuid: u128, args: &[&str]) -> Vec<Reply> {
        let Some(name) = args.first().copied().filter(|n| home_name_ok(n)) else {
            return vec![Reply::translated("horizon.anarchy.home.invalid", &[])];
        };
        let Some(g) = host.hub.guest(conn) else {
            return vec![];
        };
        let home = Home {
            name: name.to_owned(),
            dim: g.dimension.clone(),
            at: g.position,
            rotation: [g.yaw, g.pitch],
        };
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        match anarchy.store.set_home(uuid, &home, HOME_LIMIT) {
            Ok(HomeSet::Created) => vec![Reply::translated("horizon.anarchy.home.set", &[Text::literal(name)])],
            Ok(HomeSet::Updated) => vec![Reply::translated("horizon.anarchy.home.updated", &[Text::literal(name)])],
            Ok(HomeSet::Full) => vec![Reply::translated("horizon.anarchy.home.full", &[Text::literal(HOME_LIMIT.to_string())])],
            Err(e) => {
                dedicated::log("ERROR", &format!("sethome failed: {e}"));
                vec![unknown()]
            }
        }
    }

    /// `/home <name>`: to one of the player's homes, in its dimension.
    fn anarchy_home(&mut self, host: &mut LanHost, conn: u32, uuid: u128, args: &[&str]) -> Vec<Reply> {
        let Some(name) = args.first().copied() else {
            return vec![Reply::translated("horizon.anarchy.home.invalid", &[])];
        };
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        let home = match anarchy.store.get_home(uuid, name) {
            Ok(Some(home)) => home,
            Ok(None) => return vec![Reply::translated("horizon.anarchy.home.notfound", &[])],
            Err(e) => {
                dedicated::log("ERROR", &format!("home failed: {e}"));
                return vec![unknown()];
            }
        };
        self.level_teleports.push(LevelTeleport {
            conn,
            dim: Dim::from_id(&home.dim),
            at: home.at,
            rotation: Some(home.rotation),
            vehicle: None,
            cause: None,
        });
        self.run_level_teleports(host);
        vec![Reply::translated("horizon.anarchy.home.went", &[Text::literal(&home.name)])]
    }

    /// `/delhome <name>`.
    fn anarchy_delhome(&self, uuid: u128, args: &[&str]) -> Vec<Reply> {
        let Some(name) = args.first().copied() else {
            return vec![Reply::translated("horizon.anarchy.home.invalid", &[])];
        };
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        match anarchy.store.delete_home(uuid, name) {
            Ok(true) => vec![Reply::translated("horizon.anarchy.home.deleted", &[Text::literal(name)])],
            Ok(false) => vec![Reply::translated("horizon.anarchy.home.notfound", &[])],
            Err(e) => {
                dedicated::log("ERROR", &format!("delhome failed: {e}"));
                vec![unknown()]
            }
        }
    }

    /// `/homes`: the player's home names.
    fn anarchy_homes(&self, uuid: u128) -> Vec<Reply> {
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        match anarchy.store.home_names(uuid) {
            Ok(names) if names.is_empty() => vec![Reply::translated("horizon.anarchy.home.empty", &[])],
            Ok(names) => vec![Reply::translated(
                "horizon.anarchy.home.list",
                &[
                    Text::literal(names.len().to_string()),
                    Text::literal(HOME_LIMIT.to_string()),
                    Text::literal(names.join(", ")),
                ],
            )],
            Err(e) => {
                dedicated::log("ERROR", &format!("homes failed: {e}"));
                vec![unknown()]
            }
        }
    }

    /// One of `ANARCHY_COMMANDS` from a player: the replies, one SystemChat
    /// line each.
    pub(super) fn guest_anarchy_command(&mut self, host: &mut LanHost, conn: u32, words: &[&str]) {
        let Some(g) = host.hub.guest(conn) else {
            return;
        };
        let (uuid, name) = (g.uuid, g.name.clone());
        let replies: Vec<Reply> = match words[0] {
            "help" => self.anarchy_help(host, conn),
            "register" => self.anarchy_register(host, conn, &name, &words[1..]),
            "login" => self.anarchy_login(host, conn, uuid, &words[1..]),
            "changepassword" => self.anarchy_change_password(host, conn, uuid, &words[1..]),
            "ignore" | "ignorelist" => self.anarchy_ignore(host, conn, uuid, words),
            "sethome" => self.anarchy_sethome(host, conn, uuid, &words[1..]),
            "home" => self.anarchy_home(host, conn, uuid, &words[1..]),
            "delhome" => self.anarchy_delhome(uuid, &words[1..]),
            "homes" => self.anarchy_homes(uuid),
            "rtp" | "randomtp" => self.anarchy_rtp(host, conn, uuid),
            "playerstats" => self.anarchy_playerstats(host, uuid, &name, &words[1..]),
            "discord" => self.anarchy_discord(),
            "addsocial" => self.anarchy_social(),
            "skin" => self.anarchy_skin(host, conn, uuid, name.clone(), &words[1..]),
            "worldstats" => self.anarchy_worldstats(),
            _ => vec![unknown()],
        };
        for reply in replies {
            host.hub.send_to(
                conn,
                &C::SystemChat(play::SystemChat {
                    content: reply.tag(),
                    overlay: false,
                }),
            );
        }
    }

    /// `/help`: a minimal, clean list of commands.
    fn anarchy_help(&self, host: &LanHost, conn: u32) -> Vec<Reply> {
        let is_held = host
            .hub
            .guest(conn)
            .is_some_and(|g| host.hub.login_hold.contains(&g.uuid));
        if is_held {
            vec![
                Reply::literal("--- Login Commands ---").with_color(horizon_client::text::GOLD),
                Reply::literal("/register <password> <password> - Create an account")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/login <password> - Log into your account")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/changepassword <old> <new> - Change your password")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/help - Show available commands")
                    .with_color(horizon_client::text::GRAY),
            ]
        } else {
            vec![
                Reply::literal("--- Commands ---").with_color(horizon_client::text::GOLD),
                Reply::literal("/help - Show available commands")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/msg <player> <message> - Private message")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/whisper <player> <message> - Private message (alias)")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/ignore <player> - Toggle ignore")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/ignorelist - List ignored players")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/changepassword <old> <new> - Change your password")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/discord - Get Discord link")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/skin set <nickname> - Change your skin")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/worldstats - View server statistics")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/sethome <name> - Save this spot as a home (5 max)")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/home <name> - Teleport to one of your homes")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/delhome <name> - Delete a home")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/homes - List your homes")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/rtp (or /randomtp) - Teleport to a random spot near spawn")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/playerstats [player] - Join date, playtime and homes")
                    .with_color(horizon_client::text::GRAY),
                Reply::literal("/kill - Commit suicide")
                    .with_color(horizon_client::text::GRAY),
            ]
        }
    }

    /// `/register <password> <password>`: a new account, then in (the hold
    /// ends with the registration: the password was their answer).
    fn anarchy_register(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        name: &str,
        words: &[&str],
    ) -> Vec<Reply> {
        let [password, again] = words else {
            return vec![Reply::translated("horizon.anarchy.register.usage", &[])];
        };
        let Some(uuid) = host.hub.guest(conn).map(|g| g.uuid) else {
            return vec![];
        };
        let Some(anarchy) = self.anarchy.as_ref() else {
            return vec![];
        };
        if anarchy.store.has_account(uuid) {
            return vec![Reply::translated("horizon.anarchy.register.taken", &[])];
        }
        if password != again {
            return vec![Reply::translated("horizon.anarchy.register.mismatch", &[])];
        }
        if !password_ok(password) {
            return vec![Reply::translated("horizon.anarchy.register.short", &[])];
        }
        let user = NameAndId {
            id: uuid,
            name: name.to_owned(),
        };
        let result = anarchy.store.register(&user, password);
        match result {
            Ok(true) => {
                dedicated::log("INFO", &format!("{name} registered an account"));
                self.anarchy_release(host, conn);
                vec![Reply::translated("horizon.anarchy.register.success", &[])]
            }
            Ok(false) => vec![Reply::translated("horizon.anarchy.register.taken", &[])],
            Err(e) => {
                dedicated::log("ERROR", &format!("register for {name} failed: {e}"));
                vec![unknown()]
            }
        }
    }

    /// `/login <password>`: the hold ends when the password is right. The brakes
    /// (`LoginGuard`) come first: a blocked address or a locked account is not checked.
    fn anarchy_login(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        uuid: u128,
        words: &[&str],
    ) -> Vec<Reply> {
        let Some(password) = words.first() else {
            return vec![Reply::translated("horizon.anarchy.login.usage", &[])];
        };
        let Some((name, ip)) = host
            .hub
            .guest(conn)
            .map(|g| (g.name.clone(), g.address.ip()))
        else {
            return vec![];
        };
        let Some(anarchy) = &mut self.anarchy else {
            return vec![];
        };
        if !anarchy.store.has_account(uuid) {
            dedicated::log("INFO", &format!("{name} tried /login with no account (register first)"));
            return vec![Reply::translated("horizon.anarchy.login.none", &[])];
        }
        let now = Instant::now();
        match anarchy.login.gate(ip, uuid, now) {
            Brake::Account => {
                dedicated::log("INFO", &format!("{name} ({ip}) tried /login on a locked account"));
                return vec![Reply::translated("horizon.anarchy.login.account_locked", &[])];
            }
            Brake::Address => {
                dedicated::log("INFO", &format!("{name} ({ip}) tried /login while its address is blocked"));
                return vec![Reply::translated("horizon.anarchy.login.address_blocked", &[])];
            }
            Brake::None => {}
        }
        if anarchy.store.check(uuid, password) {
            anarchy.login.succeeded(ip, uuid);
        } else {
            let brake = anarchy.login.failed(ip, uuid, now);
            dedicated::log("INFO", &format!("{name} ({ip}) gave the wrong password to /login"));
            return match brake {
                Brake::Account => {
                    dedicated::log("INFO", &format!("account of {name} locked: several addresses guessed at it"));
                    vec![Reply::translated("horizon.anarchy.login.account_locked", &[])]
                }
                Brake::Address => {
                    dedicated::log("INFO", &format!("{ip} blocked from /login for {} minutes", IP_BLOCK.as_secs() / 60));
                    vec![Reply::translated("horizon.anarchy.login.address_blocked", &[])]
                }
                Brake::None => vec![Reply::translated("horizon.anarchy.login.wrong", &[])],
            };
        }
        self.anarchy_release(host, conn);
        vec![Reply::translated("horizon.anarchy.login.success", &[])]
    }

    /// `/changepassword <old> <new>`: an account, and the old password right.
    fn anarchy_change_password(
        &mut self,
        _host: &mut LanHost,
        _conn: u32,
        uuid: u128,
        words: &[&str],
    ) -> Vec<Reply> {
        let [old, new] = words else {
            return vec![Reply::translated("horizon.anarchy.password.usage", &[])];
        };
        let Some(anarchy) = &self.anarchy else {
            return vec![];
        };
        if !anarchy.store.has_account(uuid) {
            return vec![Reply::translated("horizon.anarchy.login.none", &[])];
        }
        if !anarchy.store.check(uuid, old) {
            return vec![Reply::translated("horizon.anarchy.login.wrong", &[])];
        }
        if !password_ok(new) {
            return vec![Reply::translated("horizon.anarchy.register.short", &[])];
        }
        match anarchy.store.set_password(uuid, new) {
            Ok(true) => vec![Reply::translated("horizon.anarchy.password.changed", &[])],
            Ok(false) => vec![Reply::translated("horizon.anarchy.login.none", &[])],
            Err(e) => {
                dedicated::log("ERROR", &format!("changepassword failed: {e}"));
                vec![unknown()]
            }
        }
    }

    /// `/ignore <player>` (a toggle: once ignored, again not) and
    /// `/ignorelist`. An ignore list needs no account.
    fn anarchy_ignore(
        &mut self,
        host: &mut LanHost,
        _conn: u32,
        uuid: u128,
        words: &[&str],
    ) -> Vec<Reply> {
        if words[0] == "ignorelist" {
            let Some(anarchy) = &self.anarchy else {
                return vec![];
            };
            let list = match anarchy.store.ignores_of(uuid) {
                Ok(list) => list,
                Err(e) => {
                    dedicated::log("ERROR", &format!("ignorelist failed: {e}"));
                    return vec![unknown()];
                }
            };
            if list.is_empty() {
                return vec![Reply::translated("horizon.anarchy.ignorelist.empty", &[])];
            }
            let mut out = vec![Reply::translated(
                "horizon.anarchy.ignorelist.count",
                &[Text::literal(list.len().to_string())],
            )];
            for (_, name) in &list {
                out.push(Reply::translated(
                    "horizon.anarchy.ignorelist.entry",
                    &[Text::literal(name)],
                ));
            }
            return out;
        }
        let Some(target) = words.get(1) else {
            return vec![Reply::translated("horizon.anarchy.ignore.usage", &[])];
        };
        // A player on, else one the server has seen (the user cache).
        let user = host
            .hub
            .guests
            .iter()
            .find(|g| g.placed && g.name.eq_ignore_ascii_case(target))
            .map(|g| NameAndId {
                id: g.uuid,
                name: g.name.clone(),
            })
            .or_else(|| self.resolve_name(target));
        let Some(user) = user else {
            return vec![Reply::translated("argument.entity.notfound.player", &[])];
        };
        if user.id == uuid {
            return vec![Reply::translated("horizon.anarchy.ignore.self", &[])];
        }
        let toggled = match self.anarchy.as_ref().map(|a| a.store.toggle_ignore(uuid, &user)) {
            Some(Ok(ignored)) => ignored,
            Some(Err(e)) => {
                dedicated::log("ERROR", &format!("ignore failed: {e}"));
                return vec![unknown()];
            }
            None => return vec![],
        };
        self.anarchy_load_ignores(host, uuid);
        vec![Reply::translated(
            if toggled {
                "horizon.anarchy.ignore.added"
            } else {
                "horizon.anarchy.ignore.removed"
            },
            &[Text::literal(&user.name)],
        )]
    }

    /// `/discord`: the link `[anarchy] discord` carries.
    fn anarchy_discord(&self) -> Vec<Reply> {
        let link = self
            .anarchy
            .as_ref()
            .map_or("", |a| a.config.discord.trim());
        if link.is_empty() {
            return vec![Reply::translated("horizon.anarchy.discord.none", &[])];
        }
        vec![Reply::translated(
            "horizon.anarchy.discord.link",
            &[Text::literal(link).with_color(text::AQUA)],
        )]
    }

    /// `/addsocial`: the links `[anarchy] social` carries.
    fn anarchy_social(&self) -> Vec<Reply> {
        let links: Vec<&str> = self
            .anarchy
            .as_ref()
            .map_or(Vec::new(), |a| {
                a.config.social.iter().map(|s| s.trim()).collect()
            });
        let links: Vec<&str> = links.into_iter().filter(|l| !l.is_empty()).collect();
        if links.is_empty() {
            return vec![Reply::translated("horizon.anarchy.social.none", &[])];
        }
        links
            .into_iter()
            .map(|link| {
                Reply::translated(
                    "horizon.anarchy.social.link",
                    &[Text::literal(link).with_color(text::AQUA)],
                )
            })
            .collect()
    }

    /// `/skin set <nickname>`: the named profile's skin. The textures
    /// property (with its signature) replaces the player's own, and the
    /// player lists get the entry again (a remove and a re-add, as a
    /// property change travels only with `ADD_PLAYER`).
    fn anarchy_skin(
        &mut self,
        host: &mut LanHost,
        conn: u32,
        uuid: u128,
        name: String,
        words: &[&str],
    ) -> Vec<Reply> {
        let Some(nickname) = words.get(1) else {
            return vec![Reply::translated("horizon.anarchy.skin.usage", &[])];
        };
        let Some(anarchy) = &mut self.anarchy else {
            return vec![];
        };
        // One lookup a player at a time: the services rate-limit, and the
        // server thread waits for the answer.
        if anarchy
            .skin_attempts
            .get(&uuid)
            .is_some_and(|since| since.elapsed() < SKIN_COOLDOWN)
        {
            return vec![Reply::translated("horizon.anarchy.skin.wait", &[])];
        }
        anarchy.skin_attempts.insert(uuid, Instant::now());
        drop(name);
        let property = fetch_skin(nickname);
        let Some((value, signature)) = property else {
            return vec![Reply::translated(
                "horizon.anarchy.skin.failure",
                &[Text::literal(*nickname)],
            )];
        };
        let Some(g) = host.hub.guests.iter_mut().find(|g| g.conn == conn) else {
            return vec![];
        };
        g.properties
            .retain(|p| p.name != "textures");
        g.properties.push(horizon_protocol::packets::login::Property {
            name: "textures".to_owned(),
            value,
            signature,
        });
        // The entry is removed and re-added, so every client learns the new skin. The
        // re-add is the join's own tab entry (listed, with its game mode).
        let Some((name, mode)) = host
            .hub
            .guests
            .iter()
            .find(|g| g.conn == conn)
            .map(|g| (g.name.clone(), g.game_mode))
        else {
            return vec![];
        };
        let remove = C::PlayerInfoRemove(play::PlayerInfoRemove { uuids: vec![uuid] });
        host.hub.broadcast(&remove);
        let add = host.hub.tab_add(uuid, &name, mode);
        host.hub.broadcast(&add);
        vec![Reply::translated(
            "horizon.anarchy.skin.set",
            &[Text::literal(nickname.to_string())],
        )]
    }

    /// `/worldstats`: the players the server has spawned at least once (one player data
    /// file each, so names only looked up don't count), how old the map is (the world
    /// folder's creation) and what the world weighs on disk.
    fn anarchy_worldstats(&self) -> Vec<Reply> {
        let gold = |n: String| Text::literal(n).with_color(text::GOLD);
        let dir = PathBuf::from("server.toml")
            .parent()
            .map_or_else(PathBuf::new, PathBuf::from);
        let dir = dir.join(world_name());
        let mut out = vec![Reply::translated(
            "horizon.worldstats.players",
            &[gold(players_joined(&dir).to_string())],
        )];
        let (years, months, days) = map_age(&dir);
        out.push(Reply::translated(
            "horizon.worldstats.map_age",
            &[
                gold(years.to_string()),
                gold(months.to_string()),
                gold(days.to_string()),
            ],
        ));
        let size = dir_size(&dir);
        out.push(Reply::translated(
            "horizon.worldstats.size",
            &[gold(format!("{:.2}", size as f64 / 1_000_000_000.0))],
        ));
        out
    }
}

/// The players who have joined a world: one player data file each (`players/data/<uuid>.dat`,
/// the same file that says a player has played before). A name only looked up leaves none.
fn players_joined(world: &Path) -> usize {
    std::fs::read_dir(world.join("players").join("data")).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "dat"))
            .count()
    })
}

/// A password's length rule.
fn password_ok(password: &str) -> bool {
    (PASSWORD_MIN..=PASSWORD_MAX).contains(&password.chars().count())
}

/// Vanilla's unknown-command line (the fallback for a broken store).
fn unknown() -> Reply {
    Reply::translated("command.unknown.command", &[])
}

/// `level-name` of server.properties: the world folder the game opened.
fn world_name() -> String {
    std::fs::read_to_string("server.properties")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "level-name").then(|| value.trim().to_owned())
            })
        })
        .unwrap_or_else(|| "world".to_owned())
}

/// The calendar age of `created` against now: whole years, months and days.
fn calendar_age(created: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> (i64, i64, i64) {
    use chrono::Datelike;
    let mut years = i64::from(now.year() - created.year());
    let mut months = i64::from(now.month() as i32 - created.month() as i32);
    let mut days = i64::from(now.day() as i32 - created.day() as i32);
    if days < 0 {
        months -= 1;
        // The days of the month before `now`'s (December of the year before
        // when `now` is in January).
        let previous_month = if now.month() == 1 { 12 } else { now.month() - 1 };
        let previous = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
            [previous_month as usize - 1];
        days += i64::from(previous);
        if previous_month == 2 {
            let leap = now.year() % 4 == 0 && (now.year() % 100 != 0 || now.year() % 400 == 0);
            if leap {
                days += 1;
            }
        }
    }
    if months < 0 {
        years -= 1;
        months += 12;
    }
    (years.max(0), months.max(0), days.max(0))
}

/// How old the map is: the world folder's (or its level.dat's) creation.
fn map_age(dir: &PathBuf) -> (i64, i64, i64) {
    let created = std::fs::metadata(dir)
        .ok()
        .filter(|m| m.created().is_ok())
        .and_then(|m| m.created().ok())
        .or_else(|| {
            std::fs::metadata(dir.join("level.dat"))
                .ok()
                .and_then(|m| m.created().ok())
        });
    let Some(created) = created else {
        return (0, 0, 0);
    };
    let created: chrono::DateTime<chrono::Utc> = created.into();
    calendar_age(created, chrono::Utc::now())
}

/// The bytes of every file under `dir` (what the region files weigh on disk,
/// compressed as they are stored).
fn dir_size(dir: &PathBuf) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.clone()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// The textures property of the profile named `nickname` (the services'
/// profile lookup, then its signed textures), or `None` when there is none.
fn fetch_skin(nickname: &str) -> Option<(String, Option<String>)> {
    let name = nickname.trim();
    if !(3..=16).contains(&name.chars().count()) {
        return None;
    }
    let body = http_json(&format!(
        "https://api.mojang.com/users/profiles/minecraft/{name}"
    ))?;
    let id = body.get("id")?.as_str()?.to_owned();
    let body = http_json(&format!(
        "https://sessionserver.mojang.com/session/minecraft/profile/{id}?unsigned=false"
    ))?;
    let properties = body.get("properties")?.as_array()?;
    let textures = properties.iter().find(|p| p.get("name").and_then(|n| n.as_str()) == Some("textures"))?;
    let value = textures.get("value")?.as_str()?.to_owned();
    let signature = textures
        .get("signature")
        .and_then(|s| s.as_str())
        .map(str::to_owned);
    Some((value, signature))
}

/// A GET whose answer must be JSON; `None` on any failure (no reason leaves
/// the server: the reply is the failure line).
fn http_json(url: &str) -> Option<serde_json::Value> {
    let response = ureq::get(url)
        .set("User-Agent", "horizon")
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let text = response.into_string().ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic sequence for the search tests.
    fn sequence(values: &[f64]) -> impl FnMut() -> f64 + '_ {
        let mut i = 0;
        move || {
            let v = values[i % values.len()];
            i += 1;
            v
        }
    }

    #[test]
    fn a_random_point_stays_in_the_radius() {
        let mut unit = sequence(&[0.0, 0.25, 0.5, 0.75, 0.999, 0.1234]);
        for _ in 0..6 {
            let [x, z] = disc_point(RANDOM_SPAWN_RADIUS, &mut unit);
            let d = f64::from(x).hypot(f64::from(z));
            assert!(d <= RANDOM_SPAWN_RADIUS + 1.0, "{x},{z} is {d} out");
        }
    }

    #[test]
    fn the_search_lands_three_above_the_first_safe_ground() {
        let mut unit = sequence(&[0.5, 0.5]);
        let pick = pick_random_spawn(&mut unit, |_, _| Column::Ground(70));
        match pick {
            Pick::Found([_, y, _]) => assert_eq!(y, 70.0 + 1.0 + RANDOM_SPAWN_LIFT),
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    #[test]
    fn unsafe_columns_are_skipped_until_ground_is_found() {
        let mut unit = sequence(&[0.5, 0.1, 0.9, 0.3, 0.7]);
        let seen = std::cell::Cell::new(0);
        let pick = pick_random_spawn(&mut unit, |_, _| {
            seen.set(seen.get() + 1);
            if seen.get() < 3 { Column::Unsafe } else { Column::Ground(64) }
        });
        assert!(matches!(pick, Pick::Found(_)));
        assert_eq!(seen.get(), 3);
    }

    #[test]
    fn an_unloaded_column_asks_for_its_chunk() {
        let mut unit = sequence(&[0.5, 0.5]);
        let pick = pick_random_spawn(&mut unit, |_, _| Column::Loading);
        assert!(matches!(pick, Pick::Waiting(_)));
    }

    #[test]
    fn a_world_of_water_gives_up() {
        let mut unit = sequence(&[0.5, 0.2]);
        let pick = pick_random_spawn(&mut unit, |_, _| Column::Unsafe);
        assert_eq!(pick, Pick::Exhausted);
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn three_wrong_passwords_block_the_address() {
        let mut guard = LoginGuard::default();
        let now = Instant::now();
        let a = ip("10.0.0.1");
        assert_eq!(guard.failed(a, 1, now), Brake::None);
        assert_eq!(guard.failed(a, 1, now), Brake::None);
        assert_eq!(guard.failed(a, 1, now), Brake::Address);
        assert_eq!(guard.gate(a, 2, now), Brake::Address);
        assert_eq!(guard.gate(a, 2, now + IP_BLOCK + Duration::from_secs(1)), Brake::None);
    }

    #[test]
    fn two_addresses_guessing_lock_the_account_for_an_hour() {
        let mut guard = LoginGuard::default();
        let now = Instant::now();
        for _ in 0..3 {
            guard.failed(ip("10.0.0.1"), 7, now);
        }
        let last = (0..3).map(|_| guard.failed(ip("10.0.0.2"), 7, now)).last();
        assert_eq!(last, Some(Brake::Account));
        assert_eq!(guard.gate(ip("10.0.0.3"), 7, now), Brake::Account);
        assert_eq!(guard.gate(ip("10.0.0.3"), 7, now + ACCOUNT_LOCK - Duration::from_secs(1)), Brake::Account);
        assert_eq!(guard.gate(ip("10.0.0.3"), 7, now + ACCOUNT_LOCK + Duration::from_secs(1)), Brake::None);
    }

    #[test]
    fn one_address_alone_does_not_lock_the_account() {
        let mut guard = LoginGuard::default();
        let now = Instant::now();
        let a = ip("10.0.0.1");
        for _ in 0..3 {
            guard.failed(a, 9, now);
        }
        assert_eq!(guard.gate(ip("10.0.0.2"), 9, now), Brake::None);
    }

    #[test]
    fn a_right_password_forgives_the_address() {
        let mut guard = LoginGuard::default();
        let now = Instant::now();
        let a = ip("10.0.0.1");
        guard.failed(a, 3, now);
        guard.failed(a, 3, now);
        guard.succeeded(a, 3);
        assert_eq!(guard.failed(a, 3, now), Brake::None);
        assert_eq!(guard.gate(a, 3, now), Brake::None);
    }

    fn account_with_password(password: &str) -> (Store, u128) {
        let uuid = 0x1234567890abcdefu128;
        let store = Store::memory().unwrap();
        let user = NameAndId {
            id: uuid,
            name: "Steve".to_owned(),
        };
        assert_eq!(store.register(&user, password), Ok(true));
        (store, uuid)
    }

    #[test]
    fn a_password_checks_only_itself() {
        let (store, uuid) = account_with_password("hunter2");
        assert!(store.check(uuid, "hunter2"));
        assert!(!store.check(uuid, "hunter3"));
        assert!(!store.check(0x999u128, "hunter2"));
    }

    #[test]
    fn a_second_registration_never_overwrites() {
        let (store, uuid) = account_with_password("hunter2");
        let user = NameAndId {
            id: uuid,
            name: "Steve".to_owned(),
        };
        assert_eq!(store.register(&user, "other123"), Ok(false));
        assert!(store.check(uuid, "hunter2"));
    }

    #[test]
    fn changing_a_password_keeps_the_old_out() {
        let (store, uuid) = account_with_password("hunter2");
        assert_eq!(store.set_password(uuid, "newpass"), Ok(true));
        assert!(store.check(uuid, "newpass"));
        assert!(!store.check(uuid, "hunter2"));
    }

    #[test]
    fn the_ignore_list_toggles_without_an_account() {
        let store = Store::memory().unwrap();
        let viewer = 0x1234u128;
        let other = NameAndId {
            id: 42,
            name: "Alex".to_owned(),
        };
        assert_eq!(store.toggle_ignore(viewer, &other), Ok(true));
        assert_eq!(
            store.ignores_of(viewer).unwrap(),
            vec![(42u128, "Alex".to_owned())]
        );
        assert!(!store.has_account(viewer));
        assert_eq!(store.toggle_ignore(viewer, &other), Ok(false));
        assert!(store.ignores_of(viewer).unwrap().is_empty());
    }

    #[test]
    fn a_legacy_json_store_imports_once() {
        let store = Store::memory().unwrap();
        let salt = "00".repeat(16);
        let hash = "00".repeat(32);
        let text = format!(
            r#"{{"players":[{{"id":"0000000000000000000000000000002a","name":"Alex","salt":"{salt}","hash":"{hash}","ignores":[{{"id":"00000000000000000000000000000007","name":"Bo"}}]}}]}}"#
        );
        assert_eq!(store.import(&text), Ok(1));
        assert!(store.has_account(42));
        assert_eq!(store.ignores_of(42).unwrap(), vec![(7u128, "Bo".to_owned())]);
        // A second import changes nothing.
        assert_eq!(store.import(&text), Ok(1));
        assert_eq!(store.ignores_of(42).unwrap().len(), 1);
    }

    #[test]
    fn a_short_password_is_refused() {
        assert!(!password_ok("abc"));
        assert!(!password_ok(&"a".repeat(65)));
        assert!(password_ok("abcd"));
    }

    #[test]
    fn the_calendar_age_counts_years_months_and_days() {
        use chrono::TimeZone;
        let created = chrono::Utc.with_ymd_and_hms(2020, 8, 29, 12, 0, 0).unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
        assert_eq!(calendar_age(created, now), (6, 2, 17));
        // A month's end rolls into the next unit, never negative.
        let created = chrono::Utc.with_ymd_and_hms(2026, 11, 30, 12, 0, 0).unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 12, 1, 12, 0, 0).unwrap();
        let (years, months, days) = calendar_age(created, now);
        assert_eq!((years, months, days), (0, 0, 1));
    }

    #[test]
    fn a_directorys_files_sum_to_its_size() {
        let dir = std::env::temp_dir().join(format!("horizon-anarchy-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("region")).unwrap();
        std::fs::write(dir.join("level.dat"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("region/r.0.0.mca"), [0u8; 500]).unwrap();
        assert_eq!(dir_size(&dir), 600);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

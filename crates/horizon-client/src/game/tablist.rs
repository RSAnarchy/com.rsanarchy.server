//! `[tablist]`, the Tab list's header and footer, off unless server.toml
//! turns it on: a header (the server's name, in its color) and a footer that
//! reads like the anarchy servers' — `20.0 TPS - 10 players - 15 ping -
//! 1w 6d 21h 50m 34s uptime` — with the Discord link under it. Sent to every
//! placed player once a second (the anarchy tick, `game/anarchy.rs`), so the
//! numbers follow the server. Vanilla sends an empty header and footer; all
//! the text is translation keys with the game's English as their fallback
//! (`game/chat_filter.rs`'s `Reply::translation`).
use super::{chat_filter::Reply, Game, lan::LanHost};
use horizon_client::lan::data::component_nbt;
use horizon_client::text::Text;
use horizon_protocol::packets::play::{self, Clientbound as C};
use std::time::Instant;

/// The header's color when server.toml names none: the rust of Rust Anarchy.
const RUST: u32 = 0xB7410E;
/// The footer's color: the grey of the servers' screenshot.
const FOOTER: u32 = horizon_client::text::GRAY;

/// server.toml's `[tablist]` section.
#[derive(Clone, Debug, serde::Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct TablistFile {
    pub tablist: TablistConfig,
}
impl Default for TablistFile {
    fn default() -> Self {
        Self {
            tablist: TablistConfig::default(),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, PartialEq)]
#[serde(default)]
pub(super) struct TablistConfig {
    enabled: bool,
    header: String,
    /// The header's color, six hex digits; "" is the rust color.
    header_color: String,
    /// The link shown under the footer; "" omits the line.
    discord: String,
}
impl Default for TablistConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            header: "Rust Anarchy".to_owned(),
            header_color: String::new(),
            discord: String::new(),
        }
    }
}

/// The config and the session's start (the uptime's zero).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Tablist {
    pub header: String,
    /// The header's color, parsed from six hex digits.
    pub header_color: u32,
    pub discord: String,
    /// The server's session start: what "uptime" counts from.
    started: Instant,
}

impl Tablist {
    /// server.toml's `[tablist]`, or `None` while it is off.
    pub(super) fn load() -> Option<Self> {
        let file: TablistFile = std::fs::read_to_string("server.toml")
            .ok()
            .and_then(|text| toml::from_str(&text).ok())?;
        if !file.tablist.enabled {
            return None;
        }
        let color = if file.tablist.header_color.is_empty() {
            RUST
        } else {
            u32::from_str_radix(&file.tablist.header_color, 16).unwrap_or(RUST)
        };
        Some(Self {
            header: file.tablist.header,
            header_color: color,
            discord: file.tablist.discord,
            started: Instant::now(),
        })
    }
}

/// The header and footer sent to every placed player (`hub.broadcast`).
pub(super) fn broadcast(game: &Game, host: &mut LanHost) {
    let Some(tablist) = &game.tablist else {
        return;
    };
    let tps = game.clock.tps()[0];
    let latencies: Vec<i32> = host
        .hub
        .guests
        .iter()
        .filter(|g| g.placed)
        .map(|g| g.latency.load(std::sync::atomic::Ordering::Relaxed))
        .collect();
    let players = latencies.len();
    let ping = if players == 0 {
        0
    } else {
        latencies.iter().sum::<i32>() / players as i32
    };
    let uptime = format_uptime(tablist.started.elapsed());
    // The footer: the numbers gold, the words grey, as the screenshot reads.
    let footer = Reply::translation(
        "horizon.tablist.footer",
        &[
            Text::literal(format!("{tps:.1}")).with_color(horizon_client::text::GOLD),
            Text::literal(players.to_string()).with_color(horizon_client::text::GOLD),
            Text::literal(ping.to_string()).with_color(horizon_client::text::GOLD),
            Text::literal(uptime).with_color(horizon_client::text::GOLD),
        ],
    )
    .with_color(FOOTER);
    // The header: the server's name in its color.
    let header = Reply::translation(
        "horizon.tablist.header",
        &[Text::literal(&tablist.header)],
    )
    .with_color(tablist.header_color);
    let packet = C::TabList(play::TabList {
        header: component_nbt(&header.component()),
        footer: component_nbt(&footer.component()),
    });
    host.hub.broadcast(&packet);
}

/// The session's length as `1w 6d 21h 50m 34s`.
fn format_uptime(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    let weeks = seconds / 604_800;
    let days = seconds % 604_800 / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    format!("{weeks}w {days}d {hours}h {minutes}m {seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uptime_reads_weeks_to_seconds() {
        assert_eq!(format_uptime(std::time::Duration::from_secs(0)), "0w 0d 0h 0m 0s");
        assert_eq!(
            format_uptime(std::time::Duration::from_secs(1_201_834)),
            "1w 6d 21h 50m 34s"
        );
    }
}

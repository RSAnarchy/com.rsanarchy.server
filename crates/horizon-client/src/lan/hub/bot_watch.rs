//! Bot detection: a player who chats at a steady clip while barely moving, for longer than an AFK
//! chat session would. Such a player is kicked with a plain message. This is a heuristic, so its
//! limits are the constants below: a steady clip is `MIN_CHATS` messages within `CHAT_WINDOW`, and
//! "still" is staying within `MOVE_BLOCKS` of one spot for `STILL_FOR`. Operators are not checked.

use super::{HostEvent, Hub, chat::now_millis};
use horizon_protocol::text::literal;
use std::collections::VecDeque;

/// Messages within `CHAT_WINDOW` that make a steady clip (about four a minute, or more).
const MIN_CHATS: usize = 8;
const CHAT_WINDOW: i64 = 120_000;
/// How long a player must stay near one spot while chatting at that clip.
const STILL_FOR: i64 = 120_000;
/// How far a player may wander from the spot and still count as still.
const MOVE_BLOCKS: f64 = 1.0;
/// Operators (permission level 2 and up) are not checked.
const EXEMPT_PERMISSION: u8 = 2;

/// One player's watch: which connection it belongs to, the spot they have held since
/// `still_since`, and the times of their recent messages.
pub struct Watch {
    conn: u32,
    anchor: [f64; 3],
    still_since: i64,
    chats: VecDeque<i64>,
}

impl Watch {
    fn new(conn: u32, pos: [f64; 3], now: i64) -> Self {
        Self {
            conn,
            anchor: pos,
            still_since: now,
            chats: VecDeque::new(),
        }
    }

    /// Where the player is now. A new connection starts a new watch, so a rejoin is never
    /// measured against the old session.
    fn follow(&mut self, conn: u32, pos: [f64; 3], now: i64) {
        if self.conn != conn {
            *self = Self::new(conn, pos, now);
            return;
        }
        if distance(pos, self.anchor) > MOVE_BLOCKS {
            self.anchor = pos;
            self.still_since = now;
        }
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

impl Hub {
    /// A position update: the player is still only while they stay near one spot.
    pub(super) fn bot_moved(&mut self, i: usize, pos: [f64; 3]) {
        let (uuid, conn) = (self.guests[i].uuid, self.guests[i].conn);
        let now = now_millis();
        self.bot_watch
            .entry(uuid)
            .or_insert_with(|| Watch::new(conn, pos, now))
            .follow(conn, pos, now);
    }

    /// A chat message from the player. Returns true when they were kicked for it, and the message
    /// must then go no further.
    pub(super) fn bot_chat(&mut self, i: usize, events: &mut Vec<HostEvent>) -> bool {
        let g = &self.guests[i];
        if g.permission >= EXEMPT_PERMISSION {
            return false;
        }
        let (uuid, conn, pos, name) = (g.uuid, g.conn, g.position, g.name.clone());
        let now = now_millis();
        let flagged = {
            let watch = self
                .bot_watch
                .entry(uuid)
                .or_insert_with(|| Watch::new(conn, pos, now));
            watch.follow(conn, pos, now);
            watch.chats.push_back(now);
            while watch.chats.front().is_some_and(|at| *at < now - CHAT_WINDOW) {
                watch.chats.pop_front();
            }
            watch.chats.len() >= MIN_CHATS && now - watch.still_since >= STILL_FOR
        };
        if !flagged {
            return false;
        }
        self.bot_watch.remove(&uuid);
        events.push(HostEvent::Warn(format!(
            "{name} was kicked by bot detection: chatting at a steady clip while still"
        )));
        self.kick(conn, literal("Spam bot detection - Stop!"));
        true
    }
}

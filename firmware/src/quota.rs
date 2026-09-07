//! The subscription channel: how much of each AI plan's rate-limit window is
//! gone, and how long until it comes back.
//!
//! Same shape as [`crate::gantry`] and [`crate::media`] - the puck is a *view*
//! of the host, never a second copy of it. The host (see `tools/pico2joy.py
//! quota`) reads the Claude and Codex credentials already sitting on disk, asks
//! each vendor's usage endpoint what the account has spent, and pushes the
//! answer down the machine channel. Nothing here talks to either service, and
//! nothing here decides what a percentage means - it only shows what the bridge
//! reported and asks it to look again.
//!
//! One thing *is* computed locally, and only one: the countdown. The bridge
//! sends the seconds left in a window at the moment it asked, and the puck turns
//! that into a deadline on its own clock so the minutes tick down between polls
//! rather than freezing between them. Every poll re-syncs it, so the host stays
//! the authority and the puck is only interpolating - which is the same bargain
//! the gantry makes with the toolhead position.
//!
//! Wire format, all on the machine channel (`#`-lines):
//!
//! | dir | line | meaning |
//! |-----|------|---------|
//! | host -> puck | `#qz <count>` | how many accounts there are; also the heartbeat |
//! | host -> puck | `#qa <slot> <kind> <short> <long> <label>` | slot identity: 0 claude / 1 codex, what to call each window, and a name |
//! | host -> puck | `#qu <slot> <p> <reset> <p> <reset> <sessions> <state>` | the two windows, what's running, and the account's state |
//! | puck -> host | `#q r` | go and look again, now |
//!
//! Each window is two numbers: percent spent (0-100, or [`UNKNOWN`]) and seconds
//! until it resets, negative for "didn't say". What a window is *called* comes
//! from the bridge rather than from a constant in here, because the vendor knows
//! and the puck doesn't: "5h" today, "6h" if one of them moves, and "7d/Fable"
//! when the binding weekly cap is the one scoped to a model. `sessions` is how
//! many of that account's CLI sessions are running, or negative if the bridge
//! couldn't tell. `state` is [`STATE_OK`], [`STATE_AUTH`] (that account's token
//! has expired - log in again), [`STATE_ERROR`] or [`STATE_WAIT`].

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use heapless::String;

use crate::proto;

/// How many accounts the puck will hold. Three, because the puck has three
/// buttons and the screen has room for three rows - the limit is the hardware,
/// not the protocol, and a fourth would have to displace one of them. Two Claude
/// subscriptions and a Codex one is exactly the case this was built for.
pub const MAX_ACCOUNTS: usize = 3;

pub const KIND_CLAUDE: u8 = 0;
pub const KIND_CODEX: u8 = 1;

/// A percent the vendor didn't report. Distinct from 0, which is a real answer.
pub const UNKNOWN: u8 = 255;

pub const STATE_OK: u8 = 0;
pub const STATE_AUTH: u8 = 1;
pub const STATE_ERROR: u8 = 2;
/// Nobody has asked the vendor yet - the state a slot starts in, and the one the
/// bridge reports for the second between connecting and its first answer. Its
/// own state because "we haven't looked" and "we looked and it broke" deserve
/// different words on a screen you are reading to decide whether to keep working.
pub const STATE_WAIT: u8 = 3;

/// How long a `#qz` stays good. Generous next to the gantry's 1.2 s because the
/// bridge's heartbeat is once a second and its useful data changes over minutes,
/// so there is nothing to gain from calling it offline the moment one is missed.
const STALE_MS: u32 = 3000;

/// One rate-limit window: how much of it is spent, and when it starts again.
#[derive(Clone, Copy)]
pub struct Window {
    /// 0-100, or [`UNKNOWN`].
    pub percent: u8,
    /// Milliseconds on *our* clock when the window resets, valid only while
    /// `resets` is set. Never zero, so zero can mean "unset".
    deadline_ms: u32,
    resets: bool,
}

impl Window {
    const fn new() -> Self {
        Self { percent: UNKNOWN, deadline_ms: 0, resets: false }
    }

    /// Seconds until this window resets, or `None` if the bridge didn't say.
    /// Clamped at zero: a deadline that has passed means the reset is due, not
    /// that time ran backwards.
    pub fn remaining_s(&self) -> Option<u32> {
        if !self.resets {
            return None;
        }
        let now = Instant::now().as_millis() as u32;
        // Signed, so a deadline that has just gone by reads as zero rather than
        // wrapping round to seven weeks.
        let left = self.deadline_ms.wrapping_sub(now) as i32;
        Some(if left > 0 { left as u32 / 1000 } else { 0 })
    }

    /// The bar fraction, 0-100. An unknown percent draws empty rather than full,
    /// because "we don't know" should never look like "you're fine".
    pub fn filled(&self) -> u32 {
        if self.percent == UNKNOWN { 0 } else { self.percent.min(100) as u32 }
    }
}

/// One subscription, as the bridge described it.
#[derive(Clone)]
pub struct Account {
    pub kind: u8,
    /// What to call it on screen. Two Claude plans need telling apart, so the
    /// bridge names them rather than the puck guessing from the vendor.
    pub label: String<14>,
    /// The short window - five hours on both vendors today, but the puck neither
    /// knows nor cares how long it is; it shows what it is told.
    pub five: Window,
    /// The long one, a week on both.
    pub week: Window,
    /// What to call each window on its bar, as the bridge named them.
    pub five_name: String<8>,
    pub week_name: String<8>,
    /// How many of this account's CLI sessions are running, or `None` when the
    /// bridge has no way to tell - which is every account it can only reach
    /// through an API, since neither vendor reports it.
    pub sessions: Option<u16>,
    pub state: u8,
}

impl Account {
    /// Whose plan this is. Two Claude subscriptions may well be named after
    /// people rather than vendors, so the screen says which service a row is
    /// spending rather than leaving it to the label to imply.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            KIND_CODEX => "codex",
            _ => "claude",
        }
    }

    const fn new() -> Self {
        Self {
            kind: KIND_CLAUDE,
            label: String::new(),
            five: Window::new(),
            week: Window::new(),
            five_name: String::new(),
            week_name: String::new(),
            sessions: None,
            state: STATE_WAIT,
        }
    }
}

struct Accounts {
    slots: [Account; MAX_ACCOUNTS],
    count: usize,
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<Accounts>> =
    Mutex::new(RefCell::new(Accounts {
        slots: [const { Account::new() }; MAX_ACCOUNTS],
        count: 0,
    }));

/// Milliseconds of the last `#qz`, never zero. Zero means "never heard from".
static SEEN_MS: AtomicU32 = AtomicU32::new(0);
/// Bumped on every change worth redrawing for, so the render loop keeps its
/// draw-on-change rule without diffing account labels.
static GENERATION: AtomicU32 = AtomicU32::new(0);
fn touched() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Is the bridge still talking to us?
pub fn online() -> bool {
    let seen = SEEN_MS.load(Ordering::Relaxed);
    seen != 0 && (Instant::now().as_millis() as u32).wrapping_sub(seen) < STALE_MS
}

pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// A coarse clock for the render loop's change key. The screen shows the
/// countdown in whole minutes, so once every half minute is enough to keep it
/// honest and cheap enough to sit in the key unconditionally.
pub fn tick() -> u32 {
    Instant::now().as_secs() as u32 / 30
}

/// Read every reported account under the lock.
pub fn with_accounts<R>(f: impl FnOnce(&[Account]) -> R) -> R {
    STATE.lock(|cell| {
        let s = cell.borrow();
        f(&s.slots[..s.count])
    })
}

// ---- puck -> host ----

/// Ask the bridge to go and look again rather than wait out its poll interval.
/// The question this screen answers is "has it reset yet?", and the honest
/// answer to that is worth a round trip.
pub fn request_refresh() {
    proto!("#q r");
}

// ---- host -> puck ----

/// Parse one machine-channel line. Returns whether it was ours, so the caller
/// can fall through to another handler for lines it doesn't recognise.
pub fn handle_line(line: &str) -> bool {
    let mut fields = line.split_ascii_whitespace();
    let cmd = match fields.next() {
        Some(c) => c.strip_prefix('#').unwrap_or(c),
        None => return false,
    };
    match cmd {
        "qz" => {
            // The count is also the heartbeat: it arrives every tick whether or
            // not anything moved, which is what lets a quiet bridge read as
            // offline instead of as a screen full of stale percentages.
            let count = match fields.next().and_then(|f| f.parse::<usize>().ok()) {
                Some(count) => count.min(MAX_ACCOUNTS),
                None => return true,
            };
            STATE.lock(|cell| {
                let mut s = cell.borrow_mut();
                // Shrinking wipes the slots that fell off the end, so an account
                // the bridge stopped reporting can't linger as a ghost row.
                for slot in &mut s.slots[count..] {
                    *slot = Account::new();
                }
                if s.count != count {
                    s.count = count;
                    touched();
                }
            });
            let now = Instant::now().as_millis() as u32;
            SEEN_MS.store(if now == 0 { 1 } else { now }, Ordering::Relaxed);
            true
        }
        "qa" => {
            let slot = fields.next().and_then(|f| f.parse::<usize>().ok());
            let kind = fields.next().and_then(|f| f.parse::<u8>().ok());
            let (slot, kind) = match (slot, kind) {
                (Some(slot), Some(kind)) if slot < MAX_ACCOUNTS => (slot, kind),
                _ => return true,
            };
            let five_name = fields.next().unwrap_or("");
            let week_name = fields.next().unwrap_or("");
            // The label is the rest of the line, kept verbatim - a name may well
            // have a space in it, and re-joining the split would eat runs. It is
            // last in the format for exactly that reason.
            let label = rest_after(line, 5);
            STATE.lock(|cell| {
                let mut s = cell.borrow_mut();
                let account = &mut s.slots[slot];
                account.kind = kind;
                // Truncate rather than reject: a long name still wants to show.
                fill(&mut account.label, label);
                fill(&mut account.five_name, five_name);
                fill(&mut account.week_name, week_name);
            });
            touched();
            true
        }
        "qu" => {
            let slot = match fields.next().and_then(|f| f.parse::<usize>().ok()) {
                Some(slot) if slot < MAX_ACCOUNTS => slot,
                _ => return true,
            };
            let mut values = [0i64; 6];
            for value in values.iter_mut() {
                match fields.next().and_then(|f| f.parse::<i64>().ok()) {
                    Some(parsed) => *value = parsed,
                    // A short line is a bridge we don't understand; leave the
                    // slot as it was rather than half-updating it.
                    None => return true,
                }
            }
            let [p5, r5, p7, r7, sessions, state] = values;
            STATE.lock(|cell| {
                let mut s = cell.borrow_mut();
                let account = &mut s.slots[slot];
                account.five = window_from(p5, r5);
                account.week = window_from(p7, r7);
                account.sessions = (sessions >= 0).then(|| sessions.min(999) as u16);
                account.state = state.clamp(0, 255) as u8;
            });
            touched();
            true
        }
        _ => false,
    }
}

/// Everything after the first `fields` whitespace-separated fields, verbatim.
/// The wire format puts free text last precisely so this works: splitting and
/// re-joining would turn a run of spaces in a name into one.
fn rest_after(line: &str, fields: usize) -> &str {
    let mut rest = line.trim_start();
    for _ in 0..fields {
        rest = match rest.find(|c: char| c.is_ascii_whitespace()) {
            Some(at) => rest[at..].trim_start(),
            None => return "",
        };
    }
    rest
}

/// Copy as much of `text` as fits, dropping the rest. A name too long for the
/// row was never going to be read whole anyway.
fn fill<const N: usize>(field: &mut String<N>, text: &str) {
    field.clear();
    for ch in text.chars() {
        if field.push(ch).is_err() {
            break;
        }
    }
}

/// Turn "percent, and seconds until it resets" into a window on our own clock.
fn window_from(percent: i64, seconds: i64) -> Window {
    let percent = if (0..=100).contains(&percent) { percent as u8 } else { UNKNOWN };
    if seconds < 0 {
        return Window { percent, deadline_ms: 0, resets: false };
    }
    // A week is 604800 s, so the millisecond deadline fits u32 (49 days) with
    // room to spare; clamp anyway rather than wrap into the past.
    let ahead = (seconds as u64 * 1000).min(u32::MAX as u64 / 2) as u32;
    let now = Instant::now().as_millis() as u32;
    Window { percent, deadline_ms: now.wrapping_add(ahead), resets: true }
}

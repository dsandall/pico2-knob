//! The now-playing channel: a view of whatever the host is playing.
//!
//! Same shape as [`crate::gantry`] - the puck is a *view* of the host, never a
//! second copy of it. The host (see `tools/pico2joy.py spotify`) reads the
//! active MPRIS player and pushes title, artist, play state, volume, and a
//! 128x128 one-bit cover down the machine channel; the buttons and knob send
//! transport and volume requests back up. Nothing here talks to Spotify - it
//! only mirrors what the bridge reports and asks for changes.
//!
//! Wire format, all on the machine channel (`#`-lines):
//!
//! | dir | line | meaning |
//! |-----|------|---------|
//! | host -> puck | `#ns <state> <vol>` | 0 stopped / 1 playing / 2 paused, volume 0-100 |
//! | host -> puck | `#nt <text>` | track title |
//! | host -> puck | `#na <text>` | artist |
//! | host -> puck | `#ab` | begin a cover transfer (clears the staging buffer) |
//! | host -> puck | `#a <seq> <hex>` | one row of cover bytes, `seq * CHUNK` in |
//! | host -> puck | `#ae` | cover complete: show it |
//! | puck -> host | `#m p` / `#m n` / `#m b` | play-pause / next / previous |
//! | puck -> host | `#m v <steps>` | nudge the volume by `steps` detents |

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use heapless::String;

use crate::proto;

/// One cover, in the panel's own framebuffer layout so [`crate::display::Display::blit_raw`]
/// is a straight copy. 16 pages of 128 columns.
pub const ART_BYTES: usize = 128 * 128 / 8;
/// Cover bytes per `#a` line. 40 bytes is 80 hex chars, which with the `#a <seq> `
/// prefix stays inside the 96-char line the parser accepts.
pub const CHUNK: usize = 40;

/// How long a `#ns` stays good, matching the gantry's staleness window: a bridge
/// that stops talking shows "no bridge", not a frozen title.
const STALE_MS: u32 = 2000;

pub const STATE_STOPPED: u8 = 0;
pub const STATE_PLAYING: u8 = 1;
pub const STATE_PAUSED: u8 = 2;

struct State {
    status: u8,
    /// 0-100, or 255 for "the bridge didn't say".
    volume: u8,
    title: String<64>,
    artist: String<64>,
    /// The cover on screen, and the one being received. Double-buffered so a
    /// half-arrived transfer never shows.
    art: [u8; ART_BYTES],
    staging: [u8; ART_BYTES],
    art_valid: bool,
}

static STATE: Mutex<CriticalSectionRawMutex, RefCell<State>> = Mutex::new(RefCell::new(State {
    status: STATE_STOPPED,
    volume: 255,
    title: String::new(),
    artist: String::new(),
    art: [0; ART_BYTES],
    staging: [0; ART_BYTES],
    art_valid: false,
}));

/// Milliseconds of the last `#ns`, never zero. Zero means "never heard from".
static SEEN_MS: AtomicU32 = AtomicU32::new(0);
/// Bumped on every change the screen should redraw for, so the render loop can
/// keep drawing only on change without diffing a title string.
static GENERATION: AtomicU32 = AtomicU32::new(0);

fn touched() {
    let now = Instant::now().as_millis() as u32;
    SEEN_MS.store(if now == 0 { 1 } else { now }, Ordering::Relaxed);
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Is the bridge still talking to us?
pub fn online() -> bool {
    let seen = SEEN_MS.load(Ordering::Relaxed);
    seen != 0 && (Instant::now().as_millis() as u32).wrapping_sub(seen) < STALE_MS
}

/// A monotonic counter of visible changes, for the render loop's change key.
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// Read the current now-playing state under the lock. The cover is handed to the
/// closure only when one has fully arrived.
pub fn with_state<R>(f: impl FnOnce(u8, u8, &str, &str, Option<&[u8; ART_BYTES]>) -> R) -> R {
    STATE.lock(|cell| {
        let s = cell.borrow();
        let art = if s.art_valid { Some(&s.art) } else { None };
        f(s.status, s.volume, &s.title, &s.artist, art)
    })
}

// ---- puck -> host ----

pub fn play_pause() {
    proto!("#m p");
}
pub fn next() {
    proto!("#m n");
}
pub fn prev() {
    proto!("#m b");
}
/// Nudge the volume by `steps` detents; the host decides how loud a step is.
pub fn volume(steps: i32) {
    proto!("#m v {steps}");
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
        "ns" => {
            let status = fields.next().and_then(|f| f.parse::<u8>().ok());
            let volume = fields.next().and_then(|f| f.parse::<u8>().ok());
            if let Some(status) = status {
                STATE.lock(|cell| {
                    let mut s = cell.borrow_mut();
                    s.status = status;
                    if let Some(v) = volume {
                        s.volume = v;
                    }
                });
                touched();
            }
            true
        }
        "nt" | "na" => {
            // The text is the rest of the line after the command, kept verbatim
            // (spaces and all) rather than re-joined from the split.
            let text = line
                .trim_start()
                .strip_prefix('#')
                .unwrap_or(line.trim_start())
                .get(cmd.len()..)
                .unwrap_or("")
                .trim_start();
            STATE.lock(|cell| {
                let mut s = cell.borrow_mut();
                let field = if cmd == "nt" { &mut s.title } else { &mut s.artist };
                field.clear();
                // Truncate rather than reject: a long title still wants to show.
                for ch in text.chars() {
                    if field.push(ch).is_err() {
                        break;
                    }
                }
            });
            touched();
            true
        }
        "ab" => {
            STATE.lock(|cell| cell.borrow_mut().staging.fill(0));
            true
        }
        "a" => {
            let seq = fields.next().and_then(|f| f.parse::<usize>().ok());
            let hex = fields.next();
            if let (Some(seq), Some(hex)) = (seq, hex) {
                let start = seq * CHUNK;
                STATE.lock(|cell| {
                    let mut s = cell.borrow_mut();
                    let mut i = start;
                    let bytes = hex.as_bytes();
                    let mut j = 0;
                    while j + 1 < bytes.len() && i < ART_BYTES {
                        if let (Some(hi), Some(lo)) =
                            (hexval(bytes[j]), hexval(bytes[j + 1]))
                        {
                            s.staging[i] = (hi << 4) | lo;
                        }
                        i += 1;
                        j += 2;
                    }
                });
            }
            true
        }
        "ae" => {
            STATE.lock(|cell| {
                let mut s = cell.borrow_mut();
                let staging = s.staging;
                s.art.copy_from_slice(&staging);
                s.art_valid = true;
            });
            touched();
            true
        }
        _ => false,
    }
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

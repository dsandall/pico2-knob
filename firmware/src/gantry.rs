//! The real machines: what a gantry says its toolhead is doing, and the jog
//! requests going back the other way.
//!
//! The device is a *view* of the machine, never a second copy of it. Positions
//! and limits arrive from the host bridge (`tools/pico2joy.py`, which talks to
//! Moonraker for the printer and CNCJS for the X-Carve) and are only ever
//! displayed; a knob detent produces a jog request in micrometres, and the
//! position on screen doesn't move until the machine reports that it did. That
//! way a jog that gets refused - unhomed, mid-job, past a limit - simply doesn't
//! show up, instead of leaving the puck lying about where the head is.
//!
//! There are two of them, [`GANTRY`] (Klipper) and [`XCARVE`] (GRBL), each with
//! its own screen. They are the same machine as far as the puck is concerned, so
//! they are one type; only the message prefix and the state names differ.
//!
//! Wire format, one line each way over the same CDC-ACM port the console uses.
//! Everything is integer micrometres, so there is no float parsing down here and
//! no rounding drift between the two ends. `<p>` is the machine's prefix: nothing
//! for the printer (`#s`), `x` for the X-Carve (`#xs`):
//!
//! ```text
//! host -> puck   #<p>s <x> <y> <z> <homed-bits> <state>     toolhead state
//!                #<p>l <xmin> <xmax> <ymin> <ymax> <zmin> <zmax>   travel limits
//! puck -> host   #<p>j <axis> <delta-um>                     jog request
//!                #<p>c <command>                             named command
//! ```
//!
//! Position and limits only have to share a frame, not be machine coordinates:
//! the X-Carve bridge sends both in work coordinates, so the numbers match CNCJS
//! and the head still lands in the right place in the frame.
//!
//! `#` is what separates the machine channel from the human one: every other
//! byte on the port is still a single-key console command.

use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};

use embassy_time::Instant;

use crate::proto;

/// Axis order everywhere in this module, and in the wire format.
pub const AXES: usize = 3;

/// The gain the last detent got, and when - so the screen can say "x6" while
/// it is happening and go quiet afterwards, rather than showing a multiplier
/// that hasn't applied to anything for a minute.
static GAIN: AtomicU8 = AtomicU8::new(1);
static GAIN_MS: AtomicU32 = AtomicU32::new(0);
/// How long a gain stays on screen after the detent that earned it.
const GAIN_SHOWN_MS: u32 = 500;

pub fn note_gain(steps: i32) {
    GAIN.store(steps.clamp(1, 255) as u8, Ordering::Relaxed);
    let now = Instant::now().as_millis() as u32;
    GAIN_MS.store(if now == 0 { 1 } else { now }, Ordering::Relaxed);
}

/// The multiplier worth showing right now, if any.
pub fn recent_gain() -> Option<u8> {
    let gain = GAIN.load(Ordering::Relaxed);
    let at = GAIN_MS.load(Ordering::Relaxed);
    let fresh = at != 0 && (Instant::now().as_millis() as u32).wrapping_sub(at) < GAIN_SHOWN_MS;
    if gain > 1 && fresh { Some(gain) } else { None }
}

/// How long a `#s` stays good. Two and a half bridge ticks, so one dropped
/// update doesn't blink the screen to "offline".
const STALE_MS: u32 = 1200;

/// Jog step sizes, coarsest last. The menu cycles these. One step size for the
/// knob, whichever machine it is driving: it is a feel, not a machine setting.
pub const STEPS_UM: [i32; 4] = [10, 100, 1_000, 10_000];
static STEP: AtomicU8 = AtomicU8::new(1);

pub fn step_index() -> usize {
    STEP.load(Ordering::Relaxed) as usize % STEPS_UM.len()
}

pub fn step_um() -> i32 {
    STEPS_UM[step_index()]
}

/// Pick a step size outright, which is what the wheel does.
pub fn set_step(index: usize) -> i32 {
    let index = index % STEPS_UM.len();
    STEP.store(index as u8, Ordering::Relaxed);
    STEPS_UM[index]
}

pub fn next_step() -> i32 {
    let next = (STEP.load(Ordering::Relaxed) as usize + 1) % STEPS_UM.len();
    STEP.store(next as u8, Ordering::Relaxed);
    STEPS_UM[next]
}

/// State 2 is "in the middle of a job" on every machine: that is the one a jog
/// is never sent in.
const BUSY: u8 = 2;

/// Klipper's printer.
pub static GANTRY: Machine = Machine::new("", ["idle", "ready", "printing", "paused", "error"]);
/// The X-Carve, through CNCJS. GRBL's Alarm is what an unhomed machine sits in.
pub static XCARVE: Machine = Machine::new("x", ["idle", "ready", "running", "hold", "alarm"]);

pub struct Machine {
    /// What this machine's message types start with, after the `#`.
    prefix: &'static str,
    /// [`Machine::state_label`] for each state the bridge can send.
    states: [&'static str; 5],
    /// Toolhead position, and the travel limits it moves between. Micrometres.
    pos_um: [AtomicI32; AXES],
    min_um: [AtomicI32; AXES],
    max_um: [AtomicI32; AXES],
    /// Bit per axis: 1 = that axis has been homed, so a jog is allowed to move it.
    homed: AtomicU8,
    state: AtomicU8,
    /// `Instant::now().as_millis()` of the last `#s` we accepted. Stale means the
    /// bridge went away, and the screen says so rather than showing a frozen pose.
    seen_ms: AtomicU32,
    /// Jog requested but not yet sent, per axis, in micrometres. The knob adds to
    /// this and the render loop drains it, so spinning fast coalesces into one move
    /// instead of a queue of them.
    pending_um: [AtomicI32; AXES],
}

impl Machine {
    const fn new(prefix: &'static str, states: [&'static str; 5]) -> Self {
        Self {
            prefix,
            states,
            pos_um: [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)],
            min_um: [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)],
            max_um: [
                AtomicI32::new(200_000),
                AtomicI32::new(200_000),
                AtomicI32::new(200_000),
            ],
            homed: AtomicU8::new(0),
            state: AtomicU8::new(0),
            seen_ms: AtomicU32::new(0),
            pending_um: [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)],
        }
    }

    pub fn position(&self, axis: usize) -> i32 {
        self.pos_um[axis].load(Ordering::Relaxed)
    }

    pub fn limits(&self, axis: usize) -> (i32, i32) {
        (
            self.min_um[axis].load(Ordering::Relaxed),
            self.max_um[axis].load(Ordering::Relaxed),
        )
    }

    pub fn homed(&self, axis: usize) -> bool {
        self.homed.load(Ordering::Relaxed) & (1 << axis) != 0
    }

    /// Is the bridge still talking to us? Everything else on the screen is only as
    /// true as this is.
    pub fn online(&self) -> bool {
        let seen = self.seen_ms.load(Ordering::Relaxed);
        seen != 0 && (Instant::now().as_millis() as u32).wrapping_sub(seen) < STALE_MS
    }

    pub fn state_label(&self) -> &'static str {
        if !self.online() {
            return "offline";
        }
        let state = self.state.load(Ordering::Relaxed) as usize;
        self.states.get(state).copied().unwrap_or(self.states[0])
    }

    /// What a jog refused for being mid-job is called on this machine.
    pub fn busy_label(&self) -> &'static str {
        self.states[BUSY as usize]
    }

    /// A jog is only worth asking for when someone is listening and the axis knows
    /// where it is; anything else would just bounce off the machine.
    pub fn can_jog(&self, axis: usize) -> bool {
        self.online() && self.homed(axis) && self.state.load(Ordering::Relaxed) != BUSY
    }

    /// Queue a jog of `steps` detents on `axis`, clamped to the axis's travel so the
    /// puck doesn't ask for a move the machine will refuse.
    ///
    /// The clamp can only shorten a jog, never turn it round: a head reported
    /// outside its limits - a GRBL machine unlocked without homing - would
    /// otherwise have every detent "clamp" it back towards the limit, whichever
    /// way the knob went.
    pub fn jog(&self, axis: usize, steps: i32) -> i32 {
        let (min, max) = self.limits(axis);
        let want = steps * step_um();
        let from = self.position(axis) + self.pending_um[axis].load(Ordering::Relaxed);
        let target = (from + want).clamp(min, max);
        let delta = target - from;
        if delta == 0 || delta.signum() != want.signum() {
            return 0;
        }
        self.pending_um[axis].fetch_add(delta, Ordering::Relaxed);
        delta
    }

    /// Hand every queued jog to the host. Called once a frame.
    pub fn flush_jogs(&self) {
        for axis in 0..AXES {
            let delta = self.pending_um[axis].swap(0, Ordering::Relaxed);
            if delta != 0 {
                proto!("#{}j {axis} {delta}", self.prefix);
            }
        }
    }

    /// Ask the machine to home, say. The bridge decides what that means.
    pub fn request(&self, command: &str) {
        proto!("#{}c {command}", self.prefix);
    }

    /// Parse one `#`-line from the host, if it is this machine's. Unknown lines
    /// are ignored on purpose: it costs nothing and it keeps a newer bridge from
    /// confusing an older puck.
    pub fn handle_line(&self, line: &str) -> bool {
        let mut fields = line.split_ascii_whitespace();
        let Some(kind) = fields.next() else {
            return false;
        };
        let kind = kind.strip_prefix('#').unwrap_or(kind);
        let Some(kind) = kind.strip_prefix(self.prefix) else {
            return false;
        };
        match kind {
            "s" => {
                let mut values = [0i32; AXES];
                for value in values.iter_mut() {
                    match fields.next().and_then(|f| f.parse::<i32>().ok()) {
                        Some(um) => *value = um,
                        None => return true,
                    }
                }
                for (axis, um) in values.iter().enumerate() {
                    self.pos_um[axis].store(*um, Ordering::Relaxed);
                }
                if let Some(bits) = fields.next().and_then(|f| f.parse::<u8>().ok()) {
                    self.homed.store(bits & 0b111, Ordering::Relaxed);
                }
                if let Some(state) = fields.next().and_then(|f| f.parse::<u8>().ok()) {
                    self.state.store(state, Ordering::Relaxed);
                }
                // Non-zero, always: 0 is what "never heard from" looks like.
                let now = Instant::now().as_millis() as u32;
                self.seen_ms
                    .store(if now == 0 { 1 } else { now }, Ordering::Relaxed);
                true
            }
            "l" => {
                for axis in 0..AXES {
                    let min = fields.next().and_then(|f| f.parse::<i32>().ok());
                    let max = fields.next().and_then(|f| f.parse::<i32>().ok());
                    if let (Some(min), Some(max)) = (min, max) {
                        // A zero-width axis would divide by zero in the bar.
                        if max > min {
                            self.min_um[axis].store(min, Ordering::Relaxed);
                            self.max_um[axis].store(max, Ordering::Relaxed);
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }
}

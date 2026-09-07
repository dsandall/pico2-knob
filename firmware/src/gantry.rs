//! The real gantry: what the printer says its toolhead is doing, and the jog
//! requests going back the other way.
//!
//! The device is a *view* of the machine, never a second copy of it. Positions
//! and limits arrive from the host bridge (`tools/gantry_bridge.py`, which talks
//! to Moonraker) and are only ever displayed; a knob detent produces a jog
//! request in micrometres, and the position on screen doesn't move until the
//! printer reports that it did. That way a jog that Klipper refuses - unhomed,
//! mid-print, past a limit - simply doesn't show up, instead of leaving the puck
//! lying about where the head is.
//!
//! Wire format, one line each way over the same CDC-ACM port the console uses.
//! Everything is integer micrometres, so there is no float parsing down here and
//! no rounding drift between the two ends:
//!
//! ```text
//! host -> puck   #s <x> <y> <z> <homed-bits> <state>       toolhead state
//!                #l <xmin> <xmax> <ymin> <ymax> <zmin> <zmax>   travel limits
//!                #?                                        identify
//! puck -> host   #j <axis> <delta-um>                       jog request
//!                #c <command>                              named command
//!                #v 1                                      identify reply
//! ```
//!
//! `#` is what separates the machine channel from the human one: every other
//! byte on the port is still a single-key console command.

use core::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};

use embassy_time::Instant;

use crate::proto;

/// Axis order everywhere in this module, and in the wire format.
pub const AXES: usize = 3;

/// Toolhead position, and the travel limits it moves between. Micrometres.
static POS_UM: [AtomicI32; AXES] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static MIN_UM: [AtomicI32; AXES] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static MAX_UM: [AtomicI32; AXES] = [
    AtomicI32::new(200_000),
    AtomicI32::new(200_000),
    AtomicI32::new(200_000),
];
/// Bit per axis: 1 = that axis has been homed, so a jog is allowed to move it.
static HOMED: AtomicU8 = AtomicU8::new(0);
/// Printer state, as [`state_label`] spells it.
static STATE: AtomicU8 = AtomicU8::new(0);
/// `Instant::now().as_millis()` of the last `#s` we accepted. Stale means the
/// bridge went away, and the screen says so rather than showing a frozen pose.
static SEEN_MS: AtomicU32 = AtomicU32::new(0);
/// Jog requested but not yet sent, per axis, in micrometres. The knob adds to
/// this and the render loop drains it, so spinning fast coalesces into one move
/// instead of a queue of them.
static PENDING_UM: [AtomicI32; AXES] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];

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

/// Jog step sizes, coarsest last. The menu cycles these.
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

pub fn position(axis: usize) -> i32 {
    POS_UM[axis].load(Ordering::Relaxed)
}

pub fn limits(axis: usize) -> (i32, i32) {
    (
        MIN_UM[axis].load(Ordering::Relaxed),
        MAX_UM[axis].load(Ordering::Relaxed),
    )
}

pub fn homed(axis: usize) -> bool {
    HOMED.load(Ordering::Relaxed) & (1 << axis) != 0
}

/// Is the bridge still talking to us? Everything else on the screen is only as
/// true as this is.
pub fn online() -> bool {
    let seen = SEEN_MS.load(Ordering::Relaxed);
    seen != 0 && (Instant::now().as_millis() as u32).wrapping_sub(seen) < STALE_MS
}

pub fn state_label() -> &'static str {
    if !online() {
        return "offline";
    }
    match STATE.load(Ordering::Relaxed) {
        1 => "ready",
        2 => "printing",
        3 => "paused",
        4 => "error",
        _ => "idle",
    }
}

/// A jog is only worth asking for when someone is listening and the axis knows
/// where it is; anything else would just bounce off Klipper.
pub fn can_jog(axis: usize) -> bool {
    online() && homed(axis) && STATE.load(Ordering::Relaxed) != 2
}

/// Queue a jog of `steps` detents on `axis`, clamped to the axis's travel so the
/// puck doesn't ask for a move the printer will refuse.
pub fn jog(axis: usize, steps: i32) -> i32 {
    let (min, max) = limits(axis);
    let want = steps * step_um();
    let target = (position(axis) + PENDING_UM[axis].load(Ordering::Relaxed) + want).clamp(min, max);
    let delta = target - position(axis) - PENDING_UM[axis].load(Ordering::Relaxed);
    if delta != 0 {
        PENDING_UM[axis].fetch_add(delta, Ordering::Relaxed);
    }
    delta
}

/// Hand every queued jog to the host. Called once a frame.
pub fn flush_jogs() {
    for axis in 0..AXES {
        let delta = PENDING_UM[axis].swap(0, Ordering::Relaxed);
        if delta != 0 {
            proto!("#j {axis} {delta}");
        }
    }
}

/// Ask the printer to home. The bridge decides what that means in G-code.
pub fn request(command: &str) {
    proto!("#c {command}");
}

/// Parse one `#`-line from the host. Unknown lines are ignored on purpose: it
/// costs nothing and it keeps a newer bridge from confusing an older puck.
pub fn handle_line(line: &str) {
    let mut fields = line.split_ascii_whitespace();
    match fields.next() {
        Some("s") | Some("#s") => {
            let mut values = [0i32; AXES];
            for value in values.iter_mut() {
                match fields.next().and_then(|f| f.parse::<i32>().ok()) {
                    Some(um) => *value = um,
                    None => return,
                }
            }
            for (axis, um) in values.iter().enumerate() {
                POS_UM[axis].store(*um, Ordering::Relaxed);
            }
            if let Some(bits) = fields.next().and_then(|f| f.parse::<u8>().ok()) {
                HOMED.store(bits & 0b111, Ordering::Relaxed);
            }
            if let Some(state) = fields.next().and_then(|f| f.parse::<u8>().ok()) {
                STATE.store(state, Ordering::Relaxed);
            }
            // Non-zero, always: 0 is what "never heard from" looks like.
            let now = Instant::now().as_millis() as u32;
            SEEN_MS.store(if now == 0 { 1 } else { now }, Ordering::Relaxed);
        }
        Some("l") | Some("#l") => {
            for axis in 0..AXES {
                let min = fields.next().and_then(|f| f.parse::<i32>().ok());
                let max = fields.next().and_then(|f| f.parse::<i32>().ok());
                if let (Some(min), Some(max)) = (min, max) {
                    // A zero-width axis would divide by zero in the bar.
                    if max > min {
                        MIN_UM[axis].store(min, Ordering::Relaxed);
                        MAX_UM[axis].store(max, Ordering::Relaxed);
                    }
                }
            }
        }
        Some("?") | Some("#?") => proto!("#v 1"),
        _ => {}
    }
}

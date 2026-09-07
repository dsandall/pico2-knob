//! Scroll acceleration: how much one detent is worth when the knob is moving.
//!
//! A jog knob wants two incompatible things - fine placement and long travel -
//! and a fixed step size can only ever be good at one of them. Acceleration is
//! the usual answer: turn slowly and a detent is exactly the step you chose,
//! turn quickly and each detent is worth more, so crossing the bed is a flick
//! rather than three hundred clicks.
//!
//! The curve is a power law on the *rate* of detents, which is one line of
//! arithmetic and three numbers you can argue about:
//!
//! ```text
//! gain(rate) = clamp((rate / floor) ^ exponent, 1, ceiling)
//! ```
//!
//! - `floor` is the rate below which nothing happens at all, so slow careful
//!   work is never surprised by a gain it didn't ask for.
//! - `exponent` is the shape: 1.0 is proportional, below that is gentle and
//!   forgiving, above it is aggressive and wants a confident hand.
//! - `ceiling` caps the whole thing, because there is a speed past which more
//!   gain is just a way to overshoot.
//!
//! Anything expressible as "how much faster, how soon" fits in those three, so
//! a new feel is a new row in [`PROFILES`] rather than new code.

use core::sync::atomic::{AtomicU8, Ordering};

use libm::powf;

/// One acceleration curve. See the module docs for what the three numbers do.
pub struct Curve {
    /// Detents per second below which every detent is worth exactly one step.
    pub floor: f32,
    /// Power the rate is raised to. 1.0 is proportional.
    pub exponent: f32,
    /// The most a single detent can ever be multiplied by.
    pub ceiling: f32,
}

/// The curves on offer, in the order the menu cycles them. `off` is first
/// because "why did it move that far" should always have an answer one press
/// away.
pub const PROFILES: [(&str, Curve); 4] = [
    (
        "off",
        Curve {
            floor: f32::MAX,
            exponent: 1.0,
            ceiling: 1.0,
        },
    ),
    (
        "soft",
        Curve {
            floor: 8.0,
            exponent: 0.8,
            ceiling: 6.0,
        },
    ),
    (
        "medium",
        Curve {
            floor: 6.0,
            exponent: 1.0,
            ceiling: 12.0,
        },
    ),
    (
        "hard",
        Curve {
            floor: 4.0,
            exponent: 1.3,
            ceiling: 24.0,
        },
    ),
];

/// Index into [`PROFILES`]. Off by default. Acceleration is a preference, and
/// an unasked-for one is just a knob that moves further than you meant it to -
/// turn it on from the menu when you want it.
static PROFILE: AtomicU8 = AtomicU8::new(0);

fn curve() -> &'static Curve {
    &PROFILES[PROFILE.load(Ordering::Relaxed) as usize % PROFILES.len()].1
}

pub fn profile_label() -> &'static str {
    PROFILES[PROFILE.load(Ordering::Relaxed) as usize % PROFILES.len()].0
}

pub fn next_profile() -> &'static str {
    let next = (PROFILE.load(Ordering::Relaxed) as usize + 1) % PROFILES.len();
    PROFILE.store(next as u8, Ordering::Relaxed);
    PROFILES[next].0
}

/// How many steps one detent is worth at this rate, never less than one.
///
/// Whole steps rather than a fraction, so the jog stays a multiple of the step
/// size you picked - "5 times 0.1 mm" is a number you can reason about on a
/// machine, and 0.4783 mm is not.
pub fn steps_for(detents_per_second: f32) -> i32 {
    let curve = curve();
    if detents_per_second <= curve.floor {
        return 1;
    }
    let gain = powf(detents_per_second / curve.floor, curve.exponent);
    let capped = if gain > curve.ceiling {
        curve.ceiling
    } else {
        gain
    };
    // Round to nearest, floor of one: a gain of 1.4 that truncated to 1 would
    // make the gentler curves do nothing at all.
    let steps = (capped + 0.5) as i32;
    if steps < 1 { 1 } else { steps }
}

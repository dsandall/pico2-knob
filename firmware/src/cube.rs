//! A rotating wireframe cube with the hidden edges actually hidden.
//!
//! Hidden-line removal on a convex solid is just backface culling: an edge is
//! visible exactly when at least one of the two faces meeting at it faces the
//! camera. So this draws the edges of the front-facing faces and nothing else -
//! silhouette edges come out once (one face front, one back), the three edges
//! meeting at the far corner never get drawn at all. Shared edges get drawn
//! twice, which costs nothing on a one-bit display.
//!
//! f32 rather than fixed point because the Cortex-M4F has a single-precision
//! FPU, so this is cheap and stays readable.
//!
//! The cube is a view of the device's real control model, not its own toy: it
//! renders [`crate::AXIS_COUNTS`] - the per-axis jog counters the knob drives
//! and the buttons select between - as three rotations. A gantry view reads the
//! same counters as positions.
//!
//! The knob applies *torque*, not position: a detent adds angular momentum the
//! cube keeps spinning on afterwards, so a flick sets it going and a counter-
//! flick stops it. That means the cube carries integration state of its own,
//! and jog counts drive it through their deltas rather than their absolute
//! value - see [`Cube::kick`].

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use libm::{cosf, fabsf, powf, sinf};

use crate::display::Display;

/// Radians per detent: 7.5 degrees, the angle one detent used to move the cube
/// when the knob drove position directly. Now it sets the size of the kick.
pub const STEP: f32 = core::f32::consts::PI / 24.0;

const TAU: f32 = core::f32::consts::TAU;

/// Angular velocity one detent adds, in radians per second: four detents of
/// position per second, so a single detent is a slow drift and a flick of half
/// a turn spins it at a bit over one revolution per second.
const KICK: f32 = 4.0 * STEP;

/// Ceiling on spin rate, so a fast sweep of the knob can't alias the frame rate
/// into a cube that looks like it's going backwards.
const MAX_RATE: f32 = 8.0;

/// Momentum leaks away at this fraction per second - light enough that the cube
/// visibly coasts (about a five-second half-life), heavy enough that it does
/// eventually settle instead of spinning until the battery goes flat.
const DRAG: f32 = 0.14;

/// Below this rate there's no visible motion left, so stop: it lets the render
/// loop go back to drawing only on change.
const STILL: f32 = 0.004;

/// Camera sits at -Z looking towards +Z.
const CAMERA_DISTANCE: f32 = 3.2;
const FOCAL: f32 = 105.0;
/// Middle of the gantry viewport: below the title bar rule at y=13, above the
/// axis boxes at y=106.
const CENTRE: (i32, i32) = (64, 60);

/// Zoom detents, and what one is worth. The limits are one detent short of the
/// cube filling the viewport corner to corner, and of it shrinking to a smear.
pub const ZOOM_MIN: i32 = -10;
pub const ZOOM_MAX: i32 = 14;
const ZOOM_PER_DETENT: f32 = 1.08;

/// Magnification for a zoom counter, as a multiple of the resting size.
pub fn zoom_scale(steps: i32) -> f32 {
    powf(ZOOM_PER_DETENT, steps.clamp(ZOOM_MIN, ZOOM_MAX) as f32)
}

/// The eight corners of a cube, indexed so bit 0 is x, bit 1 is y, bit 2 is z.
fn corner(index: usize) -> [f32; 3] {
    let axis = |bit: usize| if index & (1 << bit) != 0 { 1.0 } else { -1.0 };
    [axis(0), axis(1), axis(2)]
}

/// Each face as its four corner indices plus its outward normal.
const FACES: [([usize; 4], [f32; 3]); 6] = [
    ([1, 3, 7, 5], [1.0, 0.0, 0.0]),
    ([0, 4, 6, 2], [-1.0, 0.0, 0.0]),
    ([2, 6, 7, 3], [0.0, 1.0, 0.0]),
    ([0, 1, 5, 4], [0.0, -1.0, 0.0]),
    ([4, 5, 7, 6], [0.0, 0.0, 1.0]),
    ([0, 2, 3, 1], [0.0, 0.0, -1.0]),
];

/// Off-axis by default so a cube at rest still reads as a cube rather than a
/// square. Jogging an axis rotates from here.
const REST: [f32; 3] = [0.45, 0.6, 0.0];

pub struct Cube {
    /// Rotation about X, Y, Z in radians.
    angles: [f32; 3],
    /// Angular velocity about X, Y, Z in radians per second.
    rates: [f32; 3],
}

impl Cube {
    /// At rest, centred, not spinning.
    pub const fn new() -> Self {
        Self {
            angles: REST,
            rates: [0.0; 3],
        }
    }

    /// Apply `detents` of torque about `axis`. The knob calls this with the
    /// change in that axis's jog counter, so console jogs kick the cube exactly
    /// the way the knob does and there's still only one copy of the counters.
    pub fn kick(&mut self, axis: usize, detents: i32) {
        let rate = self.rates[axis] + detents as f32 * KICK;
        self.rates[axis] = if rate > MAX_RATE {
            MAX_RATE
        } else if rate < -MAX_RATE {
            -MAX_RATE
        } else {
            rate
        };
    }

    /// Advance by `dt` seconds. Returns whether it's still moving, which is
    /// what tells the render loop to keep drawing frames when nothing else has
    /// changed.
    pub fn step(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for axis in 0..3 {
            let keep = 1.0 - DRAG * dt;
            let mut rate = if keep > 0.0 { self.rates[axis] * keep } else { 0.0 };
            if fabsf(rate) < STILL {
                rate = 0.0;
            } else {
                moving = true;
                // Wrapped, so a cube left spinning for an hour keeps the same
                // resolution as one that just started.
                let mut angle = self.angles[axis] + rate * dt;
                if angle > TAU {
                    angle -= TAU;
                } else if angle < -TAU {
                    angle += TAU;
                }
                self.angles[axis] = angle;
            }
            self.rates[axis] = rate;
        }
        moving
    }

    fn rotated(&self, p: [f32; 3]) -> [f32; 3] {
        let [ax, ay, az] = self.angles;
        let (sx, cx) = (sinf(ax), cosf(ax));
        let (sy, cy) = (sinf(ay), cosf(ay));
        let (sz, cz) = (sinf(az), cosf(az));

        // X, then Y, then Z.
        let (x1, y1, z1) = (p[0], p[1] * cx - p[2] * sx, p[1] * sx + p[2] * cx);
        let (x2, y2, z2) = (x1 * cy + z1 * sy, y1, -x1 * sy + z1 * cy);
        let (x3, y3, z3) = (x2 * cz - y2 * sz, x2 * sz + y2 * cz, z2);
        [x3, y3, z3]
    }

    /// Perspective projection to screen pixels. Zoom scales the focal length
    /// rather than walking the camera in, so hard zoom can't put the near face
    /// through the lens.
    fn project(p: [f32; 3], focal: f32) -> Point {
        let depth = p[2] + CAMERA_DISTANCE;
        // The cube can't reach the camera, but never divide by zero anyway.
        let depth = if depth < 0.1 { 0.1 } else { depth };
        Point::new(
            CENTRE.0 + (p[0] * focal / depth) as i32,
            CENTRE.1 - (p[1] * focal / depth) as i32,
        )
    }

    pub fn wireframe(&self, d: &mut Display<'_>, zoom: i32) {
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
        let focal = FOCAL * zoom_scale(zoom);

        let mut projected = [Point::zero(); 8];
        let mut rotated = [[0.0f32; 3]; 8];
        for index in 0..8 {
            rotated[index] = self.rotated(corner(index));
            projected[index] = Self::project(rotated[index], focal);
        }

        for (corners, normal) in FACES {
            let n = self.rotated(normal);

            // Face centre, and the vector from the camera to it. Facing us when
            // the normal points back along that vector.
            let mut centre = [0.0f32; 3];
            for &index in corners.iter() {
                for axis in 0..3 {
                    centre[axis] += rotated[index][axis] / 4.0;
                }
            }
            let to_face = [centre[0], centre[1], centre[2] + CAMERA_DISTANCE];
            let facing = n[0] * to_face[0] + n[1] * to_face[1] + n[2] * to_face[2];
            if facing >= 0.0 {
                continue;
            }

            for edge in 0..4 {
                let from = projected[corners[edge]];
                let to = projected[corners[(edge + 1) % 4]];
                let _ = Line::new(from, to).into_styled(style).draw(d);
            }
        }
    }
}

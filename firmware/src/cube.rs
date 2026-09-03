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

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle};
use libm::{cosf, sinf};

use crate::display::Display;

/// Radians per detent: 7.5 degrees, so a full turn of a 24-detent encoder is
/// most of a revolution.
pub const STEP: f32 = core::f32::consts::PI / 24.0;

/// Camera sits at -Z looking towards +Z.
const CAMERA_DISTANCE: f32 = 3.2;
const FOCAL: f32 = 105.0;
const CENTRE: (i32, i32) = (64, 70);

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
}

impl Cube {
    /// Derive the orientation from the jog counters, so there is no second copy
    /// of the state to keep in step.
    pub fn from_counts(counts: [i32; 3]) -> Self {
        let mut angles = REST;
        for axis in 0..3 {
            angles[axis] += counts[axis] as f32 * STEP;
        }
        Self { angles }
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

    /// Perspective projection to screen pixels.
    fn project(p: [f32; 3]) -> Point {
        let depth = p[2] + CAMERA_DISTANCE;
        // The cube can't reach the camera, but never divide by zero anyway.
        let depth = if depth < 0.1 { 0.1 } else { depth };
        Point::new(
            CENTRE.0 + (p[0] * FOCAL / depth) as i32,
            CENTRE.1 - (p[1] * FOCAL / depth) as i32,
        )
    }

    pub fn wireframe(&self, d: &mut Display<'_>) {
        let style = PrimitiveStyle::with_stroke(BinaryColor::On, 1);

        let mut projected = [Point::zero(); 8];
        let mut rotated = [[0.0f32; 3]; 8];
        for index in 0..8 {
            rotated[index] = self.rotated(corner(index));
            projected[index] = Self::project(rotated[index]);
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

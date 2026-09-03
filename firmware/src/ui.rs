//! What the puck draws: title bar, encoder ring with the detent count, button
//! pips, and a VPP flag. All of it doubles as an input tester - if a button pip
//! doesn't light, that switch or its trace is the problem.

use core::fmt::Write as _;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_10X20};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};
use heapless::String;

use crate::display::Display;

/// 24 dots on a circle of radius 34 about (64, 58), index 0 at the top, going
/// clockwise - so the lit dot tracks the knob the way your hand expects.
const RING: [(i32, i32); 24] = [
    (64, 24), (73, 25), (81, 29), (88, 34), (93, 41), (97, 49), (98, 58), (97, 67),
    (93, 75), (88, 82), (81, 87), (73, 91), (64, 92), (55, 91), (47, 87), (40, 82),
    (35, 75), (31, 67), (30, 58), (31, 49), (35, 41), (40, 34), (47, 29), (55, 25),
];

const LABELS: [&str; 4] = ["1", "2", "3", "SW"];

pub struct State {
    pub detents: i32,
    pub pressed: [bool; 4],
    pub vpp_on: bool,
}

pub fn draw(d: &mut Display<'_>, state: &State) {
    d.clear();

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    let _ = Text::with_text_style("pico2joy", Point::new(2, 1), small, top_left).draw(d);
    if state.vpp_on {
        let right = TextStyleBuilder::new()
            .baseline(Baseline::Top)
            .alignment(Alignment::Right)
            .build();
        let _ = Text::with_text_style("12V", Point::new(126, 1), small, right).draw(d);
    }
    let _ = Line::new(Point::new(0, 13), Point::new(127, 13))
        .into_styled(on)
        .draw(d);

    // Encoder ring: every detent moves the filled dot one position.
    let active = state.detents.rem_euclid(RING.len() as i32) as usize;
    for (i, (x, y)) in RING.iter().enumerate() {
        let rect = if i == active {
            Rectangle::new(Point::new(x - 2, y - 2), Size::new(5, 5))
        } else {
            Rectangle::new(Point::new(x - 1, y - 1), Size::new(3, 3))
        };
        let _ = rect.into_styled(if i == active { fill } else { on }).draw(d);
    }

    let mut count: String<12> = String::new();
    let _ = write!(count, "{}", state.detents);
    let _ = Text::with_text_style(&count, Point::new(64, 58), big, centred).draw(d);

    // Button pips: outlined when up, solid with inverted label when down.
    for (i, label) in LABELS.iter().enumerate() {
        let origin = Point::new(3 + 32 * i as i32, 104);
        let rect = Rectangle::new(origin, Size::new(26, 20));
        let down = state.pressed[i];
        let _ = rect.into_styled(if down { fill } else { on }).draw(d);
        let style = if down { small_inv } else { small };
        let _ = Text::with_text_style(
            label,
            Point::new(origin.x + 13, origin.y + 10),
            style,
            TextStyleBuilder::new()
                .baseline(Baseline::Middle)
                .alignment(Alignment::Center)
                .build(),
        )
        .draw(d);
    }
}

/// Every pixel lit: maximum draw on VPP and the least ambiguous "is this panel
/// alive at all" test there is. 100% display area is still only ~25 uA of IPP.
pub fn all_on(d: &mut Display<'_>) {
    d.clear();
    let _ = Rectangle::new(Point::zero(), Size::new(128, 128))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(d);
}

/// Unambiguous orientation check: the solid block and "TL" belong in the top-left
/// corner. Mirrored means segment remap is wrong; rotated means the page/column
/// mapping is.
pub fn test_pattern(d: &mut Display<'_>) {
    d.clear();

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    let _ = Rectangle::new(Point::zero(), Size::new(128, 128))
        .into_styled(on)
        .draw(d);
    let _ = Line::new(Point::new(0, 0), Point::new(127, 127))
        .into_styled(on)
        .draw(d);
    let _ = Line::new(Point::new(127, 0), Point::new(0, 127))
        .into_styled(on)
        .draw(d);
    let _ = Rectangle::new(Point::new(4, 4), Size::new(14, 14))
        .into_styled(fill)
        .draw(d);
    let _ = Text::with_text_style(
        "TL",
        Point::new(22, 5),
        small,
        TextStyleBuilder::new().baseline(Baseline::Top).build(),
    )
    .draw(d);
    let _ = Text::with_text_style(
        "128x128",
        Point::new(64, 64),
        small,
        TextStyleBuilder::new()
            .baseline(Baseline::Middle)
            .alignment(Alignment::Center)
            .build(),
    )
    .draw(d);
}

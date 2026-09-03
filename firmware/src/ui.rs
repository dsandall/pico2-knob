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
    pub millivolts: u16,
    pub link: &'static str,
}

/// What the menu offers. The order is the order on screen, and
/// [`MenuItem::COUNT`] is what the knob wraps around.
#[derive(Copy, Clone, PartialEq)]
pub enum MenuItem {
    Link,
    Rail,
    Led,
    Screen,
    Battery,
    Exit,
}

impl MenuItem {
    pub const COUNT: u8 = 6;

    pub fn from_index(index: u8) -> Self {
        match index % Self::COUNT {
            0 => Self::Link,
            1 => Self::Rail,
            2 => Self::Led,
            3 => Self::Screen,
            4 => Self::Battery,
            _ => Self::Exit,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Link => "ble",
            Self::Rail => "12V rail",
            Self::Led => "led",
            Self::Screen => "screen",
            Self::Battery => "battery",
            Self::Exit => "exit",
        }
    }
}

pub struct MenuState {
    pub selected: u8,
    pub link: &'static str,
    pub vpp_on: bool,
    pub led: &'static str,
    pub view: &'static str,
    pub millivolts: u16,
}

/// Title bar: name, the 12 V flag when it's up, and the battery gauge.
fn header(d: &mut Display<'_>, millivolts: u16, vpp_on: bool) {
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();

    let _ = Text::with_text_style("pico2joy", Point::new(2, 1), small, top_left).draw(d);
    if vpp_on {
        let _ = Text::with_text_style("12V", Point::new(54, 1), small, top_left).draw(d);
    }

    // Battery: an outline with a nub, filled proportionally, percent to its left.
    let percent = crate::state::percent_from_mv(millivolts);
    let mut label: String<8> = String::new();
    let _ = write!(label, "{percent}%");
    let _ = Text::with_text_style(
        &label,
        Point::new(103, 1),
        small,
        TextStyleBuilder::new()
            .baseline(Baseline::Top)
            .alignment(Alignment::Right)
            .build(),
    )
    .draw(d);

    let body = Rectangle::new(Point::new(106, 1), Size::new(19, 9));
    let _ = body.into_styled(on).draw(d);
    let _ = Rectangle::new(Point::new(125, 4), Size::new(2, 3))
        .into_styled(fill)
        .draw(d);
    let bar = (17 * percent as u32 / 100).min(17);
    if bar > 0 {
        let _ = Rectangle::new(Point::new(107, 2), Size::new(bar, 7))
            .into_styled(fill)
            .draw(d);
    }

    let _ = Line::new(Point::new(0, 13), Point::new(127, 13))
        .into_styled(on)
        .draw(d);
}

/// The knob-driven menu: turn to move, press the knob to act, BTN3 to leave.
pub fn draw_menu(d: &mut Display<'_>, menu: &MenuState) {
    d.clear();
    header(d, menu.millivolts, menu.vpp_on);

    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let top_right = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(Alignment::Right)
        .build();

    for index in 0..MenuItem::COUNT {
        let item = MenuItem::from_index(index);
        let y = 18 + 12 * index as i32;
        let selected = index == menu.selected % MenuItem::COUNT;
        if selected {
            let _ = Rectangle::new(Point::new(0, y - 1), Size::new(128, 12))
                .into_styled(fill)
                .draw(d);
        }
        let style = if selected { small_inv } else { small };
        let _ = Text::with_text_style(item.label(), Point::new(4, y), style, top_left).draw(d);

        let mut value: String<12> = String::new();
        match item {
            MenuItem::Link => {
                let _ = write!(value, "{}", menu.link);
            }
            MenuItem::Rail => {
                let _ = write!(value, "{}", if menu.vpp_on { "on" } else { "off" });
            }
            MenuItem::Led => {
                let _ = write!(value, "{}", menu.led);
            }
            MenuItem::Screen => {
                let _ = write!(value, "{}", menu.view);
            }
            MenuItem::Battery => {
                let _ = write!(value, "{}.{:02}V", menu.millivolts / 1000, (menu.millivolts % 1000) / 10);
            }
            MenuItem::Exit => {}
        }
        if !value.is_empty() {
            let _ = Text::with_text_style(&value, Point::new(124, y), style, top_right).draw(d);
        }
    }
}

pub fn draw(d: &mut Display<'_>, state: &State) {
    d.clear();
    header(d, state.millivolts, state.vpp_on);

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

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

    // Link state gets the strip between the ring and the pips.
    let _ = Text::with_text_style(
        state.link,
        Point::new(64, 98),
        small,
        TextStyleBuilder::new()
            .baseline(Baseline::Middle)
            .alignment(Alignment::Center)
            .build(),
    )
    .draw(d);

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

/// Axis names, indexed the way the jog counters are.
pub const AXIS_NAMES: [&str; 3] = ["X", "Y", "Z"];

/// The gantry screen: a wireframe cube standing in for the machine, and the
/// three jog counters with the selected axis highlighted. Turn the knob to jog
/// the selected axis; BTN1/2/3 pick which one.
pub fn draw_cube(d: &mut Display<'_>, counts: [i32; 3], selected: usize, millivolts: u16, vpp_on: bool) {
    d.clear();
    header(d, millivolts, vpp_on);

    crate::cube::Cube::from_counts(counts).wireframe(d);

    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    // One box per axis, in button order, showing its jog count.
    for (slot, &axis) in crate::BUTTON_AXIS.iter().enumerate() {
        let origin = Point::new(2 + 42 * slot as i32, 106);
        let rect = Rectangle::new(origin, Size::new(40, 20));
        let active = axis == selected;
        let _ = rect.into_styled(if active { fill } else { on }).draw(d);
        let style = if active { small_inv } else { small };

        let mut label: String<12> = String::new();
        let _ = write!(label, "{}{:+}", AXIS_NAMES[axis], counts[axis]);
        let _ = Text::with_text_style(
            &label,
            Point::new(origin.x + 20, origin.y + 10),
            style,
            centred,
        )
        .draw(d);
    }
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

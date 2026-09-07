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
    Step,
    Accel,
    Home,
    Battery,
    Exit,
}

impl MenuItem {
    pub const COUNT: u8 = 9;

    pub fn from_index(index: u8) -> Self {
        match index % Self::COUNT {
            0 => Self::Link,
            1 => Self::Rail,
            2 => Self::Led,
            3 => Self::Screen,
            4 => Self::Step,
            5 => Self::Accel,
            6 => Self::Home,
            7 => Self::Battery,
            _ => Self::Exit,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Link => "ble",
            Self::Rail => "12V rail",
            Self::Led => "led",
            Self::Screen => "screen",
            Self::Step => "jog step",
            Self::Accel => "jog accel",
            Self::Home => "home all",
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
    pub step_um: i32,
    pub accel: &'static str,
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

/// The knob-driven menu: turn to move, any of BTN1/2/3 to select, knob to leave.
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
            MenuItem::Step => {
                let _ = write!(value, "{}", Millimetres(menu.step_um));
            }
            MenuItem::Accel => {
                let _ = write!(value, "{}", menu.accel);
            }
            // Both are actions, not readings: the row is the whole story.
            MenuItem::Home => {}
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

/// What the puck shows while it hands itself to the bootloader. A flash takes a
/// few seconds and the screen would otherwise just go dark, which looks exactly
/// like a board that died - so say what is happening, and say not to unplug.
pub fn draw_flashing(d: &mut Display<'_>, mode: &str) {
    d.clear();

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    let _ = Rectangle::new(Point::new(6, 34), Size::new(116, 60))
        .into_styled(on)
        .draw(d);
    let _ = Text::with_text_style("FLASHING", Point::new(64, 54), big, centred).draw(d);
    let _ = Text::with_text_style(mode, Point::new(64, 72), small, centred).draw(d);
    let _ = Text::with_text_style("keep it plugged in", Point::new(64, 84), small, centred)
        .draw(d);
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

/// Micrometres as millimetres with two decimals - as fine as a jog step goes,
/// and as much as a 128 px row has room for.
pub struct Millimetres(pub i32);

impl core::fmt::Display for Millimetres {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let um = self.0;
        let sign = if um < 0 { "-" } else { "" };
        let abs = um.unsigned_abs();
        write!(f, "{sign}{}.{:02}", abs / 1000, (abs % 1000) / 10)
    }
}

/// The step wheel: hold any axis button and the step sizes fan out around the
/// knob, turn to pick one, let go to take it.
///
/// A wheel rather than a list because the input is a wheel - the thing in your
/// hand goes round, so the choices go round, and "two clicks anticlockwise" is
/// a gesture you can make without reading the screen twice.
pub fn draw_wheel(d: &mut Display<'_>, selected: usize, millivolts: u16, vpp_on: bool) {
    d.clear();
    header(d, millivolts, vpp_on);

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    // Four seats around the knob, clockwise from the top. Hard-coded rather
    // than trigonometry: four points do not need `sinf`.
    const SEATS: [(i32, i32); 4] = [(64, 44), (98, 72), (64, 100), (30, 72)];
    let steps = crate::gantry::STEPS_UM;

    let _ = Text::with_text_style("jog step", Point::new(64, 72), small, centred).draw(d);

    for (index, seat) in SEATS.iter().enumerate().take(steps.len()) {
        let (x, y) = *seat;
        let active = index == selected % steps.len();

        let mut label: String<12> = String::new();
        let _ = write!(label, "{}", Millimetres(steps[index]));
        // 6 px a character, plus a little air either side.
        let width = 6 * label.len() as u32 + 8;
        let box_ = Rectangle::new(
            Point::new(x - width as i32 / 2, y - 7),
            Size::new(width, 14),
        );
        let _ = box_.into_styled(if active { fill } else { on }).draw(d);
        let _ = Text::with_text_style(
            &label,
            Point::new(x, y),
            if active { small_inv } else { small },
            centred,
        )
        .draw(d);
    }

    let _ = Text::with_text_style("let go to keep it", Point::new(64, 118), small, centred)
        .draw(d);
    let _ = Rectangle::new(Point::new(0, 108), Size::new(128, 1))
        .into_styled(on)
        .draw(d);
}

/// Isometric projection of the build volume. Takes a point as a fraction of
/// each axis's travel and gives back pixels: x goes right-and-down, y
/// left-and-down, z straight up. Not a calibrated view of anything - it is a
/// picture of where the head is in the frame, and that only has to be right
/// enough to read at a glance.
fn iso(unit: [f32; 3]) -> Point {
    const CENTRE: (i32, i32) = (64, 58);
    const SCALE: f32 = 32.0;
    /// cos(30 degrees): the ordinary isometric, near enough.
    const COS30: f32 = 0.866;

    let (x, y, z) = (unit[0] - 0.5, unit[1] - 0.5, unit[2] - 0.5);
    let across = (x - y) * COS30;
    let down = (x + y) * 0.5 - z;
    Point::new(
        CENTRE.0 + (across * SCALE) as i32,
        CENTRE.1 + (down * SCALE) as i32,
    )
}

/// The eight corners of the volume, indexed so bit 0 is x, bit 1 is y, bit 2 z.
fn volume_corner(index: usize) -> [f32; 3] {
    [
        (index & 1) as f32,
        ((index >> 1) & 1) as f32,
        ((index >> 2) & 1) as f32,
    ]
}

/// Bottom square, top square, then the four uprights.
const VOLUME_EDGES: [(usize, usize); 12] = [
    (0, 1), (0, 2), (1, 3), (2, 3),
    (4, 5), (4, 6), (5, 7), (6, 7),
    (0, 4), (1, 5), (2, 6), (3, 7),
];

/// Where the head sits in its travel, per axis, as 0..1.
fn head_unit() -> [f32; 3] {
    let mut unit = [0.0f32; 3];
    for axis in 0..crate::gantry::AXES {
        let (min, max) = crate::gantry::limits(axis);
        let span = (max - min) as f32;
        if span > 0.0 {
            let along = (crate::gantry::position(axis) - min) as f32 / span;
            unit[axis] = if along < 0.0 {
                0.0
            } else if along > 1.0 {
                1.0
            } else {
                along
            };
        }
    }
    unit
}

/// The gantry screen: the build volume in isometric with the printhead where
/// the printer says it is, and the exact numbers a double-tap away.
///
/// Everything here is the host's truth - see [`crate::gantry`] - so an unhomed
/// machine gets an empty frame rather than a head drawn where nobody knows it
/// is, and a bridge that stops talking says so instead of leaving a stale pose
/// on screen.
pub fn draw_gantry(
    d: &mut Display<'_>,
    selected: usize,
    show_numbers: bool,
    millivolts: u16,
    vpp_on: bool,
) {
    d.clear();
    header(d, millivolts, vpp_on);

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let clear = PrimitiveStyle::with_fill(BinaryColor::Off);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let top_right = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(Alignment::Right)
        .build();
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    // Status row: what the printer is doing, and which axes know where they are.
    let _ = Text::with_text_style(
        crate::gantry::state_label(),
        Point::new(2, 15),
        small,
        top_left,
    )
    .draw(d);
    for axis in 0..crate::gantry::AXES {
        let origin = Point::new(98 + 10 * axis as i32, 14);
        let homed = crate::gantry::homed(axis);
        let _ = Rectangle::new(origin, Size::new(9, 11))
            .into_styled(if homed { fill } else { on })
            .draw(d);
        let _ = Text::with_text_style(
            AXIS_NAMES[axis],
            Point::new(origin.x + 2, origin.y + 1),
            if homed { small_inv } else { small },
            top_left,
        )
        .draw(d);
    }

    // The frame itself.
    for (from, to) in VOLUME_EDGES {
        let _ = Line::new(iso(volume_corner(from)), iso(volume_corner(to)))
            .into_styled(on)
            .draw(d);
    }

    let known =
        crate::gantry::online() && (0..crate::gantry::AXES).all(crate::gantry::homed);
    if known {
        let unit = head_unit();
        let head = iso(unit);
        let below = iso([unit[0], unit[1], 0.0]);

        // The two rails the head rides, drawn at its height: this is what makes
        // a dot in a box read as a machine.
        let _ = Line::new(iso([0.0, unit[1], unit[2]]), iso([1.0, unit[1], unit[2]]))
            .into_styled(on)
            .draw(d);
        let _ = Line::new(iso([unit[0], 0.0, unit[2]]), iso([unit[0], 1.0, unit[2]]))
            .into_styled(on)
            .draw(d);

        // Where it is over the bed, and how far above it.
        let _ = Line::new(head, below).into_styled(on).draw(d);
        let _ = Rectangle::new(Point::new(below.x - 1, below.y - 1), Size::new(3, 3))
            .into_styled(on)
            .draw(d);
        let _ = Rectangle::new(Point::new(head.x - 2, head.y - 2), Size::new(5, 5))
            .into_styled(fill)
            .draw(d);
    } else {
        let missing = if crate::gantry::online() {
            "not homed"
        } else {
            "no bridge"
        };
        let _ = Rectangle::new(Point::new(28, 52), Size::new(72, 13))
            .into_styled(clear)
            .draw(d);
        let _ = Text::with_text_style(missing, Point::new(64, 58), small, centred).draw(d);
    }

    // The numbers, when asked for: cleared out of the drawing rather than laid
    // over it, because one-bit text on top of wireframe is neither.
    if show_numbers {
        let _ = Rectangle::new(Point::new(0, 90), Size::new(128, 22))
            .into_styled(clear)
            .draw(d);
        let mut first: String<24> = String::new();
        let mut second: String<24> = String::new();
        if known {
            let _ = write!(
                first,
                "X{} Y{}",
                Millimetres(crate::gantry::position(0)),
                Millimetres(crate::gantry::position(1))
            );
            let _ = write!(second, "Z{}", Millimetres(crate::gantry::position(2)));
        } else {
            let _ = write!(first, "X --.-- Y --.--");
            let _ = write!(second, "Z --.--");
        }
        let _ = Text::with_text_style(&first, Point::new(2, 91), small, top_left).draw(d);
        let _ = Text::with_text_style(&second, Point::new(2, 101), small, top_left).draw(d);
    }

    // Which button drives which axis, with the selected one filled. The mapping
    // is [`crate::BUTTON_AXIS`] - the same one the cube uses - and this row is
    // where you read it off without going to the source.
    for (slot, &axis) in crate::BUTTON_AXIS.iter().enumerate() {
        let origin = Point::new(2 + 22 * slot as i32, 114);
        let active = axis == selected;
        let _ = Rectangle::new(origin, Size::new(20, 12))
            .into_styled(if active { fill } else { on })
            .draw(d);
        let mut label: String<4> = String::new();
        let _ = write!(label, "{}{}", slot + 1, AXIS_NAMES[axis]);
        let _ = Text::with_text_style(
            &label,
            Point::new(origin.x + 4, origin.y + 1),
            if active { small_inv } else { small },
            top_left,
        )
        .draw(d);
    }

    let mut step: String<16> = String::new();
    match crate::gantry::recent_gain() {
        // Mid-flick: say what the knob is actually doing, not what it does at
        // rest. The multiplier vanishing again is the point - it means the
        // acceleration stopped applying.
        Some(gain) => {
            let _ = write!(step, "{}mm x{gain}", Millimetres(crate::gantry::step_um()));
        }
        None => {
            let _ = write!(step, "{}mm", Millimetres(crate::gantry::step_um()));
        }
    }
    let _ = Text::with_text_style(&step, Point::new(126, 115), small, top_right).draw(d);
}

/// The gantry screen: a wireframe cube standing in for the machine, and the
/// three jog counters with the held axis highlighted. Here the buttons are
/// momentary - hold BTN1/2/3 and the knob jogs that axis and spins the cube
/// about it; with nothing held the knob zooms instead.
pub fn draw_cube(
    d: &mut Display<'_>,
    cube: &crate::cube::Cube,
    counts: [i32; 3],
    held: Option<usize>,
    zoom: i32,
    millivolts: u16,
    vpp_on: bool,
) {
    d.clear();
    header(d, millivolts, vpp_on);

    cube.wireframe(d, zoom);

    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    // Zoom only when it's off its default, so the usual screen stays clean.
    if zoom != 0 {
        let hundredths = (crate::cube::zoom_scale(zoom) * 100.0) as i32;
        let mut label: String<12> = String::new();
        let _ = write!(label, "x{}.{:02}", hundredths / 100, hundredths % 100);
        let _ = Text::with_text_style(
            &label,
            Point::new(126, 99),
            small,
            TextStyleBuilder::new()
                .baseline(Baseline::Middle)
                .alignment(Alignment::Right)
                .build(),
        )
        .draw(d);
    }

    // One box per axis, in button order, showing its jog count.
    for (slot, &axis) in crate::BUTTON_AXIS.iter().enumerate() {
        let origin = Point::new(2 + 42 * slot as i32, 106);
        let rect = Rectangle::new(origin, Size::new(40, 20));
        let active = Some(axis) == held;
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

/// The now-playing screen: the cover as a full-screen background with a dark
/// footer carrying title, artist and volume. A view of the host's player - see
/// [`crate::media`] - so with no bridge talking it says so rather than showing a
/// stale track. Buttons are transport (prev / play-pause / next), the knob is
/// volume.
pub fn draw_nowplaying(
    d: &mut Display<'_>,
    status: u8,
    volume: u8,
    title: &str,
    artist: &str,
    art: Option<&[u8; crate::media::ART_BYTES]>,
    online: bool,
) {
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let black = PrimitiveStyle::with_fill(BinaryColor::Off);
    let white = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    match art {
        Some(bitmap) => d.blit_raw(bitmap),
        None => d.clear(),
    }

    if !online {
        // Wipe any old cover so the message stands alone.
        let _ = Rectangle::new(Point::new(0, 0), Size::new(128, 128))
            .into_styled(black)
            .draw(d);
        let _ = Text::with_text_style("no bridge", Point::new(64, 58), small, centred).draw(d);
        let _ = Text::with_text_style(
            "run pico2joy.py spotify",
            Point::new(64, 72),
            small,
            centred,
        )
        .draw(d);
        return;
    }

    // A dark footer so the text reads over any cover.
    const TOP: i32 = 97;
    let _ = Rectangle::new(Point::new(0, TOP + 1), Size::new(128, 30))
        .into_styled(black)
        .draw(d);
    let _ = Line::new(Point::new(0, TOP), Point::new(127, TOP))
        .into_styled(on)
        .draw(d);

    // A transport glyph, drawn rather than lettered so it reads at a glance.
    match status {
        crate::media::STATE_PLAYING => {
            for i in 0..7 {
                let _ = Line::new(Point::new(2 + i / 2, 100 + i), Point::new(2 + i / 2, 106 - i))
                    .into_styled(on)
                    .draw(d);
            }
        }
        crate::media::STATE_PAUSED => {
            let _ = Rectangle::new(Point::new(2, 100), Size::new(2, 7)).into_styled(white).draw(d);
            let _ = Rectangle::new(Point::new(6, 100), Size::new(2, 7)).into_styled(white).draw(d);
        }
        _ => {
            let _ = Rectangle::new(Point::new(2, 100), Size::new(6, 6)).into_styled(white).draw(d);
        }
    }

    // Title next to the glyph, artist below. Both clip at the right edge.
    let _ = Text::with_text_style(title, Point::new(12, 100), small, top_left).draw(d);
    let _ = Text::with_text_style(artist, Point::new(2, 110), small, top_left).draw(d);

    // Volume as a bar along the bottom, when the bridge reports one.
    if volume <= 100 {
        let _ = Rectangle::new(Point::new(2, 122), Size::new(124, 4))
            .into_styled(on)
            .draw(d);
        let fill = (124u32 * volume as u32 / 100).max(1);
        let _ = Rectangle::new(Point::new(2, 122), Size::new(fill, 4))
            .into_styled(white)
            .draw(d);
    }
}

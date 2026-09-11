//! What the puck draws: title bar, encoder ring with the detent count, button
//! pips, and a VPP flag. All of it doubles as an input tester - if a button pip
//! doesn't light, that switch or its trace is the problem.

use core::fmt::Write as _;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::{FONT_6X10, FONT_10X20};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::draw_target::DrawTargetExt;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};
use heapless::String;

use crate::display::Display;

/// What the render loop reads once a frame and hands to whichever screen is up.
pub struct State {
    pub detents: i32,
    pub pressed: [bool; 4],
    pub millivolts: u16,
}

/// What the menu offers. Which rows, in which order, is a list per screen:
/// [`MENU`] everywhere, and [`XCARVE_MENU`] on the X-Carve.
#[derive(Copy, Clone, PartialEq)]
pub enum MenuItem {
    /// Work zero on the selected axis, here. Takes a second press.
    Zero,
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

pub const MENU: &[MenuItem] = &[
    MenuItem::Link,
    MenuItem::Rail,
    MenuItem::Led,
    MenuItem::Screen,
    MenuItem::Step,
    MenuItem::Accel,
    MenuItem::Home,
    MenuItem::Battery,
    MenuItem::Exit,
];

/// Setting up a job comes first: it is why the menu is open on this screen.
pub const XCARVE_MENU: &[MenuItem] = &[
    MenuItem::Zero,
    MenuItem::Link,
    MenuItem::Rail,
    MenuItem::Led,
    MenuItem::Screen,
    MenuItem::Step,
    MenuItem::Accel,
    MenuItem::Home,
    MenuItem::Battery,
    MenuItem::Exit,
];

/// Rows that fit under the title bar. A longer menu scrolls.
const MENU_ROWS: usize = 9;

impl MenuItem {
    fn label(self) -> &'static str {
        match self {
            Self::Zero => "zero",
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
    pub items: &'static [MenuItem],
    pub selected: u8,
    /// The axis the `zero` row acts on, and whether it is waiting for a second
    /// press.
    pub axis: usize,
    pub zero_armed: bool,
    pub link: &'static str,
    pub vpp_on: bool,
    pub led: &'static str,
    pub view: &'static str,
    pub step_um: i32,
    pub accel: &'static str,
    pub millivolts: u16,
}

/// Title bar: which screen you're on, the radio, and the cell.
///
/// It used to say `pico2joy` and carry a `12V` flag. Neither earned its pixels.
/// You can see that it's the puck, and the rail flag was permanently lit - on
/// this revision Q1 is flipped so the rail can't actually be gated (see the
/// README), and even on a board where it could, VPP being down means the panel
/// is dark and nobody is reading the header anyway. The rail still has its menu
/// row and its line in `p`, which is where a diagnostic belongs.
///
/// What does change is which of seven screens is up, and that is worth the
/// left-hand side now that holding the knob and turning moves between them.
fn header(d: &mut Display<'_>, millivolts: u16) {
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let top_right = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(Alignment::Right)
        .build();

    let _ = Text::with_text_style(crate::view_label(), Point::new(2, 1), small, top_left)
        .draw(d);

    // The radio in three characters, and the cell as a number rather than a
    // picture of one: the icon was the decorative half of that pair.
    let _ = Text::with_text_style(crate::link_short(), Point::new(98, 1), small, top_right)
        .draw(d);
    let mut label: String<8> = String::new();
    let _ = write!(label, "{}%", crate::state::percent_from_mv(millivolts));
    let _ = Text::with_text_style(&label, Point::new(126, 1), small, top_right).draw(d);

    let _ = Line::new(Point::new(0, 13), Point::new(127, 13))
        .into_styled(on)
        .draw(d);
}

/// The knob-driven menu: turn to move, any of BTN1/2/3 to select, knob to leave.
pub fn draw_menu(d: &mut Display<'_>, menu: &MenuState) {
    d.clear();
    header(d, menu.millivolts);

    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let small_inv = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let top_right = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(Alignment::Right)
        .build();

    // The window over the list: the top until the selection runs off its bottom,
    // then following it down.
    let count = menu.items.len();
    let chosen = menu.selected as usize % count;
    let top = chosen.saturating_sub(MENU_ROWS - 1);

    for (row, index) in (top..count.min(top + MENU_ROWS)).enumerate() {
        let item = menu.items[index];
        let y = 18 + 12 * row as i32;
        let selected = index == chosen;
        if selected {
            // Two pixels short of the edge, so the scroll bar still shows.
            let _ = Rectangle::new(Point::new(0, y - 1), Size::new(126, 12))
                .into_styled(fill)
                .draw(d);
        }
        let style = if selected { small_inv } else { small };
        let mut label: String<16> = String::new();
        match item {
            MenuItem::Zero => {
                let _ = write!(label, "{} {}", item.label(), AXIS_NAMES[menu.axis % 3]);
            }
            _ => {
                let _ = label.push_str(item.label());
            }
        }
        let _ = Text::with_text_style(&label, Point::new(4, y), style, top_left).draw(d);

        let mut value: String<12> = String::new();
        match item {
            MenuItem::Zero => {
                if menu.zero_armed {
                    let _ = value.push_str("again?");
                }
            }
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

    // Where the window sits in a list longer than it, down the right-hand edge.
    if count > MENU_ROWS {
        let track = 12 * MENU_ROWS as i32;
        let thumb = track * MENU_ROWS as i32 / count as i32;
        let offset = (track - thumb) * top as i32 / (count - MENU_ROWS) as i32;
        let _ = Rectangle::new(Point::new(127, 17 + offset), Size::new(1, thumb as u32))
            .into_styled(fill)
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
pub fn draw_wheel(
    d: &mut Display<'_>,
    machine: &crate::gantry::Machine,
    selected: usize,
    millivolts: u16,
) {
    d.clear();
    header(d, millivolts);

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
    let steps = machine.steps_um();

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
fn head_unit(machine: &crate::gantry::Machine) -> [f32; 3] {
    let mut unit = [0.0f32; 3];
    for axis in 0..crate::gantry::AXES {
        let (min, max) = machine.limits(axis);
        let span = (max - min) as f32;
        if span > 0.0 {
            let along = (machine.position(axis) - min) as f32 / span;
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

/// A machine screen, printer or X-Carve: the travel volume in isometric with the
/// head where the machine says it is, and the exact numbers a double-tap away.
///
/// Everything here is the host's truth - see [`crate::gantry`] - so an unhomed
/// machine gets an empty frame rather than a head drawn where nobody knows it
/// is, and a bridge that stops talking says so instead of leaving a stale pose
/// on screen.
pub fn draw_gantry(
    d: &mut Display<'_>,
    machine: &crate::gantry::Machine,
    selected: usize,
    show_numbers: bool,
    millivolts: u16,
) {
    d.clear();
    header(d, millivolts);

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
    let _ =
        Text::with_text_style(machine.state_label(), Point::new(2, 15), small, top_left).draw(d);
    for axis in 0..crate::gantry::AXES {
        let origin = Point::new(98 + 10 * axis as i32, 14);
        let homed = machine.homed(axis);
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

    let known = machine.online() && (0..crate::gantry::AXES).all(|axis| machine.homed(axis));
    if known {
        let unit = head_unit(machine);
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
        let missing = if machine.online() {
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
                Millimetres(machine.position(0)),
                Millimetres(machine.position(1))
            );
            let _ = write!(second, "Z{}", Millimetres(machine.position(2)));
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
            let _ = write!(step, "{}mm x{gain}", Millimetres(machine.step_um()));
        }
        None => {
            let _ = write!(step, "{}mm", Millimetres(machine.step_um()));
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
) {
    d.clear();
    header(d, millivolts);

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

/// A duration as the coarsest unit that still says something useful: "6d23h",
/// "1h47m", "43m", "9s". A rate-limit window is not a stopwatch, so the seconds
/// only appear in the last minute, when they are the whole story.
///
/// Five characters at most, deliberately: on the quota screen this sits at the
/// right of a line that also carries a name and a sessions badge, and a
/// six-character form (`59m03s`) would run into the badge.
pub struct Countdown(pub Option<u32>);

impl core::fmt::Display for Countdown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let seconds = match self.0 {
            Some(seconds) => seconds,
            None => return write!(f, "--"),
        };
        let (days, hours) = (seconds / 86_400, seconds / 3600 % 24);
        let minutes = seconds / 60 % 60;
        if days > 0 {
            write!(f, "{days}d{hours}h")
        } else if hours > 0 {
            write!(f, "{hours}h{minutes:02}m")
        } else if minutes > 0 {
            write!(f, "{minutes}m")
        } else {
            write!(f, "{}s", seconds % 60)
        }
    }
}

/// The first `chars` characters of `s`. By character, not by byte: a bridge is
/// free to send a name with an accent in it, and slicing that mid-codepoint
/// would panic the firmware over a label.
fn clip(s: &str, chars: usize) -> &str {
    match s.char_indices().nth(chars) {
        Some((at, _)) => &s[..at],
        None => s,
    }
}

/// One statistic as a full-width bar with its own label riding on it: the name
/// on the left, the number on the right, the fill showing the proportion.
///
/// The text is drawn twice, clipped to either side of the fill boundary - lit
/// over the empty part, dark over the filled part - so it stays legible at every
/// value instead of disappearing into the bar somewhere around half way. That
/// trick is the whole reason a bar can carry its own label here rather than
/// needing a caption line above it, which is what buys the room for six of them
/// on a 128-pixel screen.
fn stat_bar(d: &mut Display<'_>, area: Rectangle, percent: u32, left: &str, right: &str) {
    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let _ = area.into_styled(on).draw(d);

    let inner = Rectangle::new(
        area.top_left + Point::new(1, 1),
        Size::new(area.size.width - 2, area.size.height - 2),
    );
    let filled = inner.size.width * percent.min(100) / 100;
    if filled > 0 {
        let _ = Rectangle::new(inner.top_left, Size::new(filled, inner.size.height))
            .into_styled(fill)
            .draw(d);
    }

    let middle = area.top_left.y + area.size.height as i32 / 2;
    let centred_left = TextStyleBuilder::new().baseline(Baseline::Middle).build();
    let centred_right = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Right)
        .build();
    let at_left = Point::new(area.top_left.x + 3, middle);
    let at_right = Point::new(area.top_left.x + area.size.width as i32 - 3, middle);

    let over_fill = Rectangle::new(inner.top_left, Size::new(filled, inner.size.height));
    let over_gap = Rectangle::new(
        inner.top_left + Point::new(filled as i32, 0),
        Size::new(inner.size.width - filled, inner.size.height),
    );
    for (area, colour) in [(over_fill, BinaryColor::Off), (over_gap, BinaryColor::On)] {
        let style = MonoTextStyle::new(&FONT_6X10, colour);
        let mut region = d.clipped(&area);
        let _ = Text::with_text_style(left, at_left, style, centred_left).draw(&mut region);
        let _ = Text::with_text_style(right, at_right, style, centred_right).draw(&mut region);
    }
}

/// The quota screen: how much of each subscription's rate-limit window is spent,
/// and how long until it starts again.
///
/// A view of the host - see [`crate::quota`] - so with no bridge talking it says
/// so rather than showing percentages from an hour ago.
///
/// Every account is fully drawn, every time: a name line carrying the vendor and
/// the countdown to the short window's reset, then a bar for that window and a
/// thinner one for the week. There is nothing to select and nothing to page
/// through, which is the point - the question this screen answers is "where do I
/// stand", and an answer you have to press a button to finish reading is a worse
/// answer. Holding a button asks the bridge to look again, and that is the only
/// control here.
pub fn draw_quota(
    d: &mut Display<'_>,
    accounts: &[crate::quota::Account],
    online: bool,
    millivolts: u16,
) {
    d.clear();
    header(d, millivolts);

    let on = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let fill = PrimitiveStyle::with_fill(BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let top_left = TextStyleBuilder::new().baseline(Baseline::Top).build();
    let top_right = TextStyleBuilder::new()
        .baseline(Baseline::Top)
        .alignment(Alignment::Right)
        .build();
    let centred = TextStyleBuilder::new()
        .baseline(Baseline::Middle)
        .alignment(Alignment::Center)
        .build();

    if !online || accounts.is_empty() {
        let (what, how) = if online {
            ("no accounts", "check --claude/--codex")
        } else {
            ("no bridge", "run pico2joy.py quota")
        };
        let _ = Text::with_text_style(what, Point::new(64, 58), small, centred).draw(d);
        let _ = Text::with_text_style(how, Point::new(64, 72), small, centred).draw(d);
        return;
    }

    // Below the title bar, split evenly, capped so one lonely account doesn't get
    // a bar you could land a plane on, and centred in what's left over.
    const TOP: i32 = 15;
    const NAME_H: i32 = 10;
    const GAP: i32 = 1;
    let avail = 128 - TOP;
    let count = accounts.len() as i32;
    let block = (avail / count).min(44);
    let top = TOP + (avail - block * count) / 2;

    // The short window gets the fatter bar: it is the one that decides whether
    // you can keep working in the next ten minutes.
    let bars = block - NAME_H - GAP * 3;
    let short_h = (bars * 55 / 100).max(7);
    let long_h = (bars - short_h).max(7);

    for (slot, account) in accounts.iter().enumerate() {
        // No rule between blocks: the bar above one account's name is already
        // the line under the last one's, and drawing both put two strokes in
        // the same two pixels.
        let t = top + block * slot as i32;

        // How many sessions this account has running, as a filled badge - drawn
        // only when there are any, so an idle plan's line stays clean and a busy
        // one announces itself. Neither vendor reports this; the bridge counts
        // the CLI processes on the machine the account is logged in on, which is
        // the only place the answer exists.
        let running = account.sessions.filter(|&n| n > 0);

        // The name takes whatever the rest of the line leaves it: thirteen
        // characters when the badge is absent, eleven when it isn't. The
        // countdown is five at most by construction - see [`Countdown`].
        let mut who: String<24> = String::new();
        let _ = write!(who, "{} {}", account.kind_label(), account.label);
        let room = if running.is_some() { 11 } else { 13 };
        let _ = Text::with_text_style(clip(&who, room), Point::new(2, t), small, top_left)
            .draw(d);

        if account.state != crate::quota::STATE_OK {
            // One box where the bars go, saying what is wrong. A percentage from
            // before the token expired would be worse than no percentage.
            let what = match account.state {
                crate::quota::STATE_AUTH => "log in again",
                crate::quota::STATE_WAIT => "asking...",
                crate::quota::STATE_ERROR => "no reply",
                _ => "?",
            };
            let box_ = Rectangle::new(
                Point::new(0, t + NAME_H + GAP),
                Size::new(128, (short_h + long_h + GAP) as u32),
            );
            let _ = box_.into_styled(on).draw(d);
            let _ = Text::with_text_style(
                what,
                Point::new(64, box_.top_left.y + box_.size.height as i32 / 2),
                small,
                centred,
            )
            .draw(d);
            continue;
        }

        let mut when: String<12> = String::new();
        let _ = write!(when, "{}", Countdown(account.five.remaining_s()));
        let _ = Text::with_text_style(&when, Point::new(126, t), small, top_right).draw(d);

        if let Some(running) = running {
            let badge = Rectangle::new(Point::new(76, t), Size::new(14, 10));
            let _ = badge.into_styled(fill).draw(d);
            let mut count: String<4> = String::new();
            let _ = write!(count, "{}", running.min(99));
            let _ = Text::with_text_style(
                &count,
                Point::new(83, t + 5),
                MonoTextStyle::new(&FONT_6X10, BinaryColor::Off),
                centred,
            )
            .draw(d);
        }

        for (window, name, y, height) in [
            (account.five, &account.five_name, t + NAME_H + GAP, short_h),
            (account.week, &account.week_name, t + NAME_H + GAP * 2 + short_h, long_h),
        ] {
            let mut value: String<8> = String::new();
            if window.percent == crate::quota::UNKNOWN {
                let _ = write!(value, "--");
            } else {
                let _ = write!(value, "{}%", window.percent);
            }
            stat_bar(
                d,
                Rectangle::new(Point::new(0, y), Size::new(128, height as u32)),
                window.filled(),
                name,
                &value,
            );
        }
    }
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

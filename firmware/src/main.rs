//! pico2joy / pico2-knob bring-up firmware.
//!
//! Brings up the nice!nano v2, the encoder, the three buttons and the SH1107
//! ribbon OLED, and streams every input edge over USB serial (CDC-ACM) while
//! mirroring the same state on the screen. No battery support, no BLE.
//!
//! Pinout (nice!nano v2 pads, from `board_pico2knob/pico2-knob.kicad_sch`):
//!
//! | Signal    | nRF52840 | pad | Notes                                       |
//! |-----------|----------|-----|---------------------------------------------|
//! | LED       | P0.15    |  -  | on-module blue LED, active high             |
//! | VCC_EN    | P0.13    |  -  | nice!nano load switch: high = 3V3 pad live  |
//! | ENC_A     | P0.09    | 24  | NFC pin, freed via `nfc-pins-as-gpio`       |
//! | ENC_B     | P0.10    | 23  | likewise                                    |
//! | ENC_SW    | P1.11    | 22  |                                             |
//! | BTN1      | P0.31    | 17  |                                             |
//! | BTN2      | P0.02    | 19  |                                             |
//! | BTN3      | P1.15    | 20  |                                             |
//! | OLED_DC   | P0.06    |  1  | SH1107, 4-wire SPI over the J3 ribbon       |
//! | OLED_SCLK | P0.08    |  2  | SPIM3                                       |
//! | OLED_SDI  | P0.17    |  5  | SPIM3                                       |
//! | OLED_RES  | P0.22    |  7  | 10k pull-up R2: boots out of reset          |
//! | OLED_CS   | P0.24    |  8  | 10k pull-up R6: boots deselected            |
//! | 12V_EN    | P1.06    | 12  | Q1 gate, **active low** - see [`Boost`]     |
//!
//! Switches are all wired to GND, so "pressed" reads low.
//!
//! Four cooperating futures: USB, input polling at 1 kHz, rendering at 25 fps,
//! and the command reader. Log lines go through a channel so a full frame flush
//! never stalls encoder sampling, and so a host that stops reading the port
//! costs us dropped lines rather than missed detents.

#![no_std]
#![no_main]

#[cfg(feature = "ble")]
mod ble;
mod cube;
mod display;
#[cfg(not(feature = "ble"))]
mod radio;
mod state;
mod ui;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU16, Ordering};

use embassy_futures::join::{join, join5};
use embassy_nrf::config::HfclkSource;
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::pwm::{DutyCycle, SimpleConfig, SimplePwm};
use embassy_nrf::saadc::{self, ChannelConfig, Saadc, VddhDiv5Input};
use embassy_nrf::spim::{self, Spim};
#[cfg(not(feature = "ble"))]
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
#[cfg(feature = "ble")]
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::{bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Sender, State};
use embassy_usb::{Builder, Config};
use heapless::String;

use crate::display::Display;

// In the `ble` build, CLOCK_POWER belongs to MPSL (see `ble::Irqs`), so USB
// gives up hardware VBUS detection and is told it is plugged in - which it is,
// or the console wouldn't be there to read.
bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    #[cfg(not(feature = "ble"))]
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
    SAADC => saadc::InterruptHandler;
});

/// Poll interval for switches and the encoder. 1 kHz is far faster than a thumb.
const TICK: Duration = Duration::from_millis(1);
/// Consecutive stable samples (ms) needed to accept a switch edge.
const DEBOUNCE_TICKS: u8 = 4;
/// Heartbeat: a dim 120 ms wink every 5 s. The blue LED is on P0.15 via PWM so
/// "dim" is real dimming, not a shorter blink. (The *red* LED is the LN2054
/// charger's status output - hardware, no GPIO, nothing firmware can do.)
const HEARTBEAT_PERIOD_TICKS: u32 = 5000;
const HEARTBEAT_ON_TICKS: u32 = 120;
/// Out of `SimpleConfig::default()`'s max_duty of 1000, at 1 kHz.
const HEARTBEAT_DUTY: u16 = 30;
const LED_FULL_DUTY: u16 = 1000;
/// Magic the Adafruit UF2 bootloader looks for in GPREGRET to stay in UF2 mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// How long the orientation test pattern stays up at boot.
const SPLASH: Duration = Duration::from_millis(1500);

const SWITCH_NAMES: [&str; 4] = ["BTN1", "BTN2", "BTN3", "ENC_SW"];

const HELP: &str = concat!(
    "\r\ncommands: ? help | p pins | d cycle view (live/gantry/pattern/all-on)\r\n",
    "          i re-init display | f flip 180 | +/- contrast | e 12V rail\r\n",
    "          w wireless | m menu (knob moves, knob press acts, BTN3 backs out)\r\n",
    "          1/2/3 select axis (Z/X/Y, as the buttons do) | , . jog it\r\n",
    "          gantry: hold 1/2/3 to jog+spin that axis, knob alone zooms\r\n",
    "          l led (dark/dim/on) | v verbose | b bootloader\r\n"
);

type Line = String<192>;

/// Log lines waiting for the USB writer. Bounded and lossy on purpose.
static LOG: Channel<CriticalSectionRawMutex, Line, 8> = Channel::new();

static DETENTS: AtomicI32 = AtomicI32::new(0);
static PRESSED: AtomicU8 = AtomicU8::new(0);
static VPP_ON: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);
/// 0 = dark, 1 = dim heartbeat, 2 = full on (for finding the board).
static LED_MODE: AtomicU8 = AtomicU8::new(1);
/// The raw advertiser is harmless, so it self-starts. The BLE stack waits for
/// 'w' - see the note in `ble::run`.
static RADIO_ON: AtomicBool = AtomicBool::new(!cfg!(feature = "ble"));
static SEQ: AtomicU8 = AtomicU8::new(0);
/// BAT+ / VDDH in millivolts, refreshed by the render loop.
static BATT_MV: AtomicU16 = AtomicU16::new(0);
/// 0 = off, 1 = starting, 2 = advertising, 3 = connected.
static LINK_STATE: AtomicU8 = AtomicU8::new(0);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);
static MENU_SEL: AtomicU8 = AtomicU8::new(0);
/// 0 = live inputs, 1 = gantry/cube, 2 = orientation pattern, 3 = all pixels on.
static VIEW: AtomicU8 = AtomicU8::new(0);
const VIEW_GANTRY: u8 = 1;
/// Gantry zoom, in detents off the resting size - see [`cube::zoom_scale`].
static ZOOM: AtomicI32 = AtomicI32::new(0);

/// The device's control model: three jog counters, one selected axis. The knob
/// drives the selected counter, BTN1/2/3 choose which. Everything on screen is
/// a view of this.
static AXIS_COUNTS: [AtomicI32; 3] =
    [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static AXIS: AtomicU8 = AtomicU8::new(0);

/// Which axis each button selects, in button order. The cube wanted Z, X, Y;
/// a gantry build that prefers X, Y, Z only has to change this line.
pub const BUTTON_AXIS: [usize; 3] = [2, 0, 1];
static REQ_HELP: AtomicBool = AtomicBool::new(false);
static REQ_PINS: AtomicBool = AtomicBool::new(false);
static REQ_BOOST: AtomicBool = AtomicBool::new(false);
static REQ_VIEW: AtomicBool = AtomicBool::new(false);
static REQ_REINIT: AtomicBool = AtomicBool::new(false);
static REQ_FLIP: AtomicBool = AtomicBool::new(false);
static REQ_BRIGHTER: AtomicBool = AtomicBool::new(false);
static REQ_DIMMER: AtomicBool = AtomicBool::new(false);

fn link_label() -> &'static str {
    match LINK_STATE.load(Ordering::Relaxed) {
        1 => "starting",
        2 => "adv",
        3 => "connected",
        _ => "off",
    }
}

fn led_label() -> &'static str {
    match LED_MODE.load(Ordering::Relaxed) {
        0 => "dark",
        2 => "on",
        _ => "dim",
    }
}

fn view_label() -> &'static str {
    match VIEW.load(Ordering::Relaxed) {
        1 => "gantry",
        2 => "pattern",
        3 => "all-on",
        _ => "live",
    }
}

/// Which axis the knob is driving right now on the gantry screen: the first of
/// BTN1/2/3 held down, or nothing, which is what makes the knob a zoom.
fn held_axis(pressed: &[bool; 4]) -> Option<usize> {
    (0..3).find(|&i| pressed[i]).map(|i| BUTTON_AXIS[i])
}

fn axis_counts() -> [i32; 3] {
    [
        AXIS_COUNTS[0].load(Ordering::Relaxed),
        AXIS_COUNTS[1].load(Ordering::Relaxed),
        AXIS_COUNTS[2].load(Ordering::Relaxed),
    ]
}

/// One state snapshot for whichever radio is built in.
fn snapshot() -> state::Payload {
    state::Payload {
        seq: SEQ.fetch_add(1, Ordering::Relaxed),
        buttons: PRESSED.load(Ordering::Relaxed),
        detents: DETENTS.load(Ordering::Relaxed) as i16,
        uptime_s: Instant::now().as_secs() as u16,
        flags: VPP_ON.load(Ordering::Relaxed) as u8,
        millivolts: BATT_MV.load(Ordering::Relaxed),
    }
}

fn log_fmt(args: core::fmt::Arguments) {
    let mut line: Line = String::new();
    let ms = Instant::now().as_millis();
    let _ = write!(line, "[{:5}.{:03}] ", ms / 1000, ms % 1000);
    let _ = line.write_fmt(args);
    let _ = line.push_str("\r\n");
    // Drop the line rather than block if the host isn't draining the port.
    let _ = LOG.try_send(line);
}

#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => { crate::log_fmt(format_args!($($arg)*)) };
}

/// The 12 V panel rail (U2, MC34063) hangs off +BATT behind Q1, a P-channel FET
/// whose gate R8 pulls up to +BATT. So the rail is off when the gate sits at
/// +BATT and on when it is pulled down - and 3V3 is *not* a clean "off": it would
/// leave Vgs around -0.9 V, right in the DMG3415U's threshold band. Drive the pin
/// low to enable, and let it float (disconnected) to disable; never drive it high.
///
/// On this board revision Q1's drain is on +BATT rather than its source, so its
/// body diode feeds U2 anyway and the rail is likely live regardless. See README.
struct Boost<'d> {
    pin: Flex<'d>,
    on: bool,
}

impl<'d> Boost<'d> {
    fn new(pin: Flex<'d>) -> Self {
        let mut me = Self { pin, on: true };
        me.set(false);
        me
    }

    fn set(&mut self, on: bool) {
        if on {
            self.pin.set_low();
            self.pin.set_as_output(OutputDrive::Standard);
        } else {
            self.pin.set_as_disconnected();
        }
        self.on = on;
        VPP_ON.store(on, Ordering::Relaxed);
    }

    /// Describes the *pin*, not the rail - see the type-level note.
    fn state(&self) -> &'static str {
        if self.on {
            "12V_EN driven low (Q1 fully on)"
        } else {
            "12V_EN high-Z (gate pulled to +BATT)"
        }
    }
}

#[embassy_executor::main]
async fn main(_spawner: embassy_executor::Spawner) {
    let mut config = embassy_nrf::config::Config::default();
    // USBD needs the 64 MHz clock derived from HFXO; the nice!nano's MDBT50Q module
    // has the 32 MHz crystal. (Enabling `nfc-pins-as-gpio` also makes the first boot
    // after flashing write UICR and reset once - that is expected.)
    config.hfclk_source = HfclkSource::ExternalXtal;
    // MPSL puts RADIO/RTC0/TIMER0 at P0 and expects nothing else to compete;
    // the app's own interrupts sit below it in the ble build.
    #[cfg(feature = "ble")]
    {
        config.time_interrupt_priority = embassy_nrf::interrupt::Priority::P2;
    }
    let p = embassy_nrf::init(config);
    #[cfg(feature = "ble")]
    {
        use embassy_nrf::interrupt::{self, InterruptExt};
        interrupt::USBD.set_priority(interrupt::Priority::P2);
        interrupt::SPIM3.set_priority(interrupt::Priority::P2);
        interrupt::RNG.set_priority(interrupt::Priority::P2);
        interrupt::SAADC.set_priority(interrupt::Priority::P2);
    }

    // The 3V3 pad (OLED VDD, and the R2/R6 pull-ups) sits behind a load switch on
    // P0.13. Nothing on the puck outside the module is powered until this is high.
    let _vcc_en = Output::new(p.P0_13, Level::High, OutputDrive::Standard);
    let mut led = SimplePwm::new_1ch(p.PWM0, p.P0_15, &SimpleConfig::default());
    let mut boost = Boost::new(Flex::new(p.P1_06));

    // 4 MHz is well inside the SH1107's serial timing and pushes a whole 2 KB
    // frame in about 4 ms.
    let mut spi_config = spim::Config::default();
    spi_config.frequency = spim::Frequency::M4;
    let spi = Spim::new_txonly(p.SPI3, Irqs, p.P0_08, p.P0_17, spi_config);
    let mut screen = Display::new(
        spi,
        Output::new(p.P0_24, Level::High, OutputDrive::Standard), // CS, idle high
        Output::new(p.P0_06, Level::Low, OutputDrive::Standard),  // DC
        Output::new(p.P0_22, Level::High, OutputDrive::Standard), // RES, out of reset
    );

    // The nice!nano v2 senses the cell through VDDH rather than a divider pin,
    // so this reads VDDH/5 against the internal 0.6 V reference at gain 1/6.
    let mut battery = Saadc::new(
        p.SAADC,
        Irqs,
        saadc::Config::default(),
        [ChannelConfig::single_ended(VddhDiv5Input)],
    );

    let enc_a = Input::new(p.P0_09, Pull::Up);
    let enc_b = Input::new(p.P0_10, Pull::Up);
    // Order matches SWITCH_NAMES and the PRESSED bitmask.
    let switches = [
        Input::new(p.P0_31, Pull::Up),
        Input::new(p.P0_02, Pull::Up),
        Input::new(p.P1_15, Pull::Up),
        Input::new(p.P1_11, Pull::Up),
    ];

    // ---- USB CDC-ACM ----
    #[cfg(not(feature = "ble"))]
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    #[cfg(feature = "ble")]
    let driver = {
        static VBUS: static_cell::StaticCell<SoftwareVbusDetect> = static_cell::StaticCell::new();
        Driver::new(p.USBD, Irqs, VBUS.init(SoftwareVbusDetect::new(true, true)) as &_)
    };
    let mut usb_config = Config::new(0x1209, 0x0001); // pid.codes prototype VID/PID
    usb_config.manufacturer = Some("softek");
    usb_config.product = Some("pico2joy bring-up");
    usb_config.serial_number = Some("pico2joy-1");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut msos_descriptor = [0u8; 0];
    let mut control_buf = [0u8; 64];
    let mut cdc_state = State::new();

    let mut builder = Builder::new(
        driver,
        usb_config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );
    let class = CdcAcmClass::new(&mut builder, &mut cdc_state, 64);
    let mut usb = builder.build();
    let (mut tx, mut rx) = class.split();

    // ---- host commands ----
    let commands = async {
        let mut buf = [0u8; 64];
        loop {
            rx.wait_connection().await;
            while let Ok(n) = rx.read_packet(&mut buf).await {
                for &byte in &buf[..n] {
                    match byte {
                        b'?' | b'h' => REQ_HELP.store(true, Ordering::Relaxed),
                        b'p' => REQ_PINS.store(true, Ordering::Relaxed),
                        // Handy without hands on the puck.
                        b'm' => {
                            let open = !MENU_OPEN.fetch_xor(true, Ordering::Relaxed);
                            logln!("menu: {}", if open { "open" } else { "closed" });
                        }
                        b'e' => REQ_BOOST.store(true, Ordering::Relaxed),
                        // Axis select and jog from the console: the same model
                        // the buttons and knob drive, for testing without hands
                        // on the puck (and a hook for driving it from a host).
                        b'1' | b'2' | b'3' => {
                            let axis = BUTTON_AXIS[(byte - b'1') as usize];
                            AXIS.store(axis as u8, Ordering::Relaxed);
                            logln!("axis: {}", ui::AXIS_NAMES[axis]);
                        }
                        b',' | b'.' => {
                            let step = if byte == b'.' { 1 } else { -1 };
                            let axis = AXIS.load(Ordering::Relaxed) as usize % 3;
                            let jogged =
                                AXIS_COUNTS[axis].fetch_add(step, Ordering::Relaxed) + step;
                            logln!("jog {}={jogged}", ui::AXIS_NAMES[axis]);
                        }
                        b'd' => REQ_VIEW.store(true, Ordering::Relaxed),
                        b'i' => REQ_REINIT.store(true, Ordering::Relaxed),
                        b'f' => REQ_FLIP.store(true, Ordering::Relaxed),
                        b'+' | b'=' => REQ_BRIGHTER.store(true, Ordering::Relaxed),
                        b'-' | b'_' => REQ_DIMMER.store(true, Ordering::Relaxed),
                        b'v' => {
                            VERBOSE.fetch_xor(true, Ordering::Relaxed);
                        }
                        b'l' => {
                            let mode = (LED_MODE.load(Ordering::Relaxed) + 1) % 3;
                            LED_MODE.store(mode, Ordering::Relaxed);
                            logln!(
                                "led: {}",
                                match mode {
                                    0 => "dark",
                                    2 => "on",
                                    _ => "dim heartbeat",
                                }
                            );
                        }
                        b'w' => {
                            let on = !RADIO_ON.fetch_xor(true, Ordering::Relaxed);
                            logln!("radio: advertising {}", if on { "on" } else { "off" });
                        }
                        // Reboot into UF2 mode, so reflashing doesn't need the
                        // reset button under the puck.
                        b'b' => reboot_to_uf2(),
                        _ => {}
                    }
                }
            }
        }
    };

    // ---- log lines -> host ----
    let writer = async {
        let mut was_open = false;
        loop {
            // A host opening the port asserts DTR; greet it and dump the inputs.
            let open = tx.dtr();

            // The 1200-baud touch: open at 1200, then close. Trigger on DTR
            // *falling* rather than on "DTR is low", because a host sets the line
            // coding and raises DTR as two separate requests - a level test catches
            // the gap between them and reboots the board just for being opened,
            // which is exactly the trap a stale 1200 on a recycled ttyACM node
            // sets. Requiring a high-then-low transition means only a real
            // open-and-close can do it.
            if was_open && !open && tx.line_coding().data_rate() == 1200 {
                reboot_to_uf2();
            }
            if open && !was_open {
                emit(&mut tx, "\r\npico2joy bring-up (nice!nano v2, embassy)\r\n").await;
                emit(&mut tx, HELP).await;
                REQ_PINS.store(true, Ordering::Relaxed);
            }
            was_open = open;

            // Timed out receive so DTR edges are still noticed while idle.
            if let Ok(line) = with_timeout(Duration::from_millis(50), LOG.receive()).await {
                emit(&mut tx, &line).await;
            }
        }
    };

    // ---- inputs ----
    let inputs = async {
        let mut ticker = Ticker::every(TICK);
        let mut ticks: u32 = 0;

        // Quadrature: index by (previous AB << 2 | current AB), +/-1 per quarter step.
        const QUAD: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];
        let ab = |a: &Input, b: &Input| (a.is_high() as u8) << 1 | b.is_high() as u8;
        let mut prev_ab = ab(&enc_a, &enc_b);
        let mut quarters: i8 = 0;
        let mut detents: i32 = 0;

        let mut pressed = [false; 4];
        let mut stable = [0u8; 4];
        let mut was_verbose = false;
        let mut last_duty = u16::MAX;

        loop {
            let verbose = VERBOSE.load(Ordering::Relaxed);
            if verbose != was_verbose {
                was_verbose = verbose;
                logln!("verbose {}", if verbose { "on" } else { "off" });
            }

            // Encoder.
            let now_ab = ab(&enc_a, &enc_b);
            if now_ab != prev_ab {
                let step = QUAD[(prev_ab << 2 | now_ab) as usize];
                if verbose {
                    logln!("ENC raw {prev_ab:02b}->{now_ab:02b} step={step:+}");
                }
                prev_ab = now_ab;
                if step == 0 {
                    // Both phases changed between samples: a missed sample, or one
                    // phase is noisy - a cold joint on A or B looks like this.
                    quarters = 0;
                } else {
                    quarters += step;
                    if quarters.abs() >= 4 {
                        let direction = quarters.signum() as i32;
                        quarters = 0;
                        if MENU_OPEN.load(Ordering::Relaxed) {
                            // In the menu the knob moves the selection instead of
                            // spinning the counter.
                            let count = ui::MenuItem::COUNT as i32;
                            let selected = MENU_SEL.load(Ordering::Relaxed) as i32;
                            let next = (selected + direction).rem_euclid(count);
                            MENU_SEL.store(next as u8, Ordering::Relaxed);
                        } else {
                            detents += direction;
                            DETENTS.store(detents, Ordering::Relaxed);
                            // On the gantry screen the buttons are momentary:
                            // the knob only jogs while an axis is held down, and
                            // with nothing held it zooms the view instead.
                            // Everywhere else BTN1/2/3 stay a latched select.
                            let gantry = VIEW.load(Ordering::Relaxed) == VIEW_GANTRY;
                            let held = held_axis(&pressed);
                            let axis = match (gantry, held) {
                                (true, None) => None,
                                (true, some) => some,
                                (false, _) => {
                                    Some(AXIS.load(Ordering::Relaxed) as usize % 3)
                                }
                            };
                            match axis {
                                Some(axis) => {
                                    // The knob's real job: jog the chosen axis.
                                    let jogged = AXIS_COUNTS[axis]
                                        .fetch_add(direction, Ordering::Relaxed)
                                        + direction;
                                    logln!(
                                        "ENC {} {}={jogged} detents={detents}",
                                        if direction > 0 { "cw " } else { "ccw" },
                                        ui::AXIS_NAMES[axis]
                                    );
                                }
                                None => {
                                    let zoom = (ZOOM.load(Ordering::Relaxed) + direction)
                                        .clamp(cube::ZOOM_MIN, cube::ZOOM_MAX);
                                    ZOOM.store(zoom, Ordering::Relaxed);
                                    logln!(
                                        "ENC {} zoom={zoom} detents={detents}",
                                        if direction > 0 { "cw " } else { "ccw" }
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Switches, with a few ms of "must stay put" debounce.
            for i in 0..switches.len() {
                let down = switches[i].is_low();
                if down == pressed[i] {
                    stable[i] = 0;
                } else {
                    stable[i] += 1;
                    if stable[i] >= DEBOUNCE_TICKS {
                        stable[i] = 0;
                        pressed[i] = down;
                        let mut bits = 0u8;
                        for (j, &d) in pressed.iter().enumerate() {
                            bits |= (d as u8) << j;
                        }
                        PRESSED.store(bits, Ordering::Relaxed);
                        logln!(
                            "{} {}",
                            SWITCH_NAMES[i],
                            if down { "down" } else { "up" }
                        );

                        // BTN1/2/3 pick the axis the knob jogs, unless the menu
                        // has the buttons.
                        if down && i < 3 && !MENU_OPEN.load(Ordering::Relaxed) {
                            let axis = BUTTON_AXIS[i];
                            AXIS.store(axis as u8, Ordering::Relaxed);
                            logln!("axis: {}", ui::AXIS_NAMES[axis]);
                        }

                        // The knob press is the menu key; BTN3 backs out of it.
                        if down {
                            let open = MENU_OPEN.load(Ordering::Relaxed);
                            match (i, open) {
                                (3, false) => {
                                    MENU_OPEN.store(true, Ordering::Relaxed);
                                    logln!("menu: open");
                                }
                                (3, true) => activate_menu_item(),
                                (2, true) => {
                                    MENU_OPEN.store(false, Ordering::Relaxed);
                                    logln!("menu: closed");
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }

            if REQ_HELP.swap(false, Ordering::Relaxed) {
                let _ = LOG.try_send(Line::try_from(HELP).unwrap_or_default());
            }
            if REQ_PINS.swap(false, Ordering::Relaxed) {
                let mut line: String<128> = String::new();
                let _ = write!(
                    line,
                    "pins ENC_A={} ENC_B={}",
                    enc_a.is_high() as u8,
                    enc_b.is_high() as u8
                );
                for (name, pin) in SWITCH_NAMES.iter().zip(switches.iter()) {
                    let _ = write!(line, " {name}={}", pin.is_high() as u8);
                }
                let millivolts = BATT_MV.load(Ordering::Relaxed);
                logln!(
                    "{line} | detents={} | 12V_EN {} | bat {}.{:02}V {}% | link {}",
                    DETENTS.load(Ordering::Relaxed),
                    if VPP_ON.load(Ordering::Relaxed) { "low" } else { "hi-Z" },
                    millivolts / 1000,
                    (millivolts % 1000) / 10,
                    state::percent_from_mv(millivolts),
                    link_label()
                );
                let counts = axis_counts();
                logln!(
                    "axis {} | X{:+} Y{:+} Z{:+}",
                    ui::AXIS_NAMES[AXIS.load(Ordering::Relaxed) as usize % 3],
                    counts[0],
                    counts[1],
                    counts[2]
                );
            }

            // Heartbeat.
            let duty = match LED_MODE.load(Ordering::Relaxed) {
                0 => 0,
                2 => LED_FULL_DUTY,
                _ if ticks % HEARTBEAT_PERIOD_TICKS < HEARTBEAT_ON_TICKS => HEARTBEAT_DUTY,
                _ => 0,
            };
            if duty != last_duty {
                last_duty = duty;
                // Inverted polarity is the intuitive one here: the pin is high
                // while the counter is *below* the value, so duty == brightness.
                led.set_duty(0, DutyCycle::inverted(duty));
            }

            ticks = ticks.wrapping_add(1);
            ticker.next().await;
        }
    };

    // ---- screen ----
    let render = async {
        // VDD is already up (P0.13). Init with the display off, bring VPP up, then
        // turn the panel on - and unwind in the opposite order.
        screen.init().await;
        boost.set(true);
        Timer::after_millis(100).await;
        screen.on().await;
        logln!("display: SH1107 128x128 up, {}", boost.state());

        ui::test_pattern(&mut screen);
        screen.flush().await;
        Timer::after(SPLASH).await;

        const VIEWS: u8 = 4;
        const FRAME_MS: u64 = 40;
        let mut ticker = Ticker::every(Duration::from_millis(FRAME_MS));
        let mut last = None;
        let mut ticks: u32 = 0;

        // The cube keeps the only state the counters can't express: how fast it
        // is spinning. Jogs reach it as deltas, so the knob and the console `,`
        // `.` keys both land as torque and the counters stay the single copy of
        // position.
        let mut cube = cube::Cube::new();
        let mut spun_counts = axis_counts();

        loop {
            let mut force = false;

            if REQ_REINIT.swap(false, Ordering::Relaxed) {
                screen.init().await;
                screen.on().await;
                logln!("display re-initialised");
                force = true;
            }
            if REQ_FLIP.swap(false, Ordering::Relaxed) {
                let flipped = !screen.flipped();
                screen.set_flipped(flipped).await;
                logln!("display {}", if flipped { "flipped 180" } else { "upright" });
                force = true;
            }
            if REQ_BRIGHTER.swap(false, Ordering::Relaxed) {
                let c = screen.contrast().saturating_add(0x10);
                screen.set_contrast(c).await;
                logln!("contrast 0x{c:02x}");
            }
            if REQ_DIMMER.swap(false, Ordering::Relaxed) {
                let c = screen.contrast().saturating_sub(0x10);
                screen.set_contrast(c).await;
                logln!("contrast 0x{c:02x}");
            }
            if REQ_VIEW.swap(false, Ordering::Relaxed) {
                let view = (VIEW.load(Ordering::Relaxed) + 1) % VIEWS;
                VIEW.store(view, Ordering::Relaxed);
                logln!("view: {}", view_label());
                force = true;
            }

            // The cell, every couple of seconds. Cheap, and it barely moves.
            if ticks % 50 == 0 {
                let mut sample = [0i16; 1];
                battery.sample(&mut sample).await;
                // VDDH/5 at gain 1/6 against the 0.6 V reference, 12-bit:
                // mV = raw * 0.6 * 6 * 5 * 1000 / 4096.
                let millivolts = (sample[0].max(0) as u32 * 18_000 / 4096) as u16;
                BATT_MV.store(millivolts, Ordering::Relaxed);
            }
            ticks = ticks.wrapping_add(1);
            if REQ_BOOST.swap(false, Ordering::Relaxed) {
                if boost.on {
                    // Panel off before its rail, per the usual OLED ordering.
                    screen.off().await;
                    boost.set(false);
                } else {
                    boost.set(true);
                    Timer::after_millis(100).await;
                    screen.on().await;
                }
                logln!("{} - meter TP4 for 12V", boost.state());
                force = true;
            }

            let state = ui::State {
                detents: DETENTS.load(Ordering::Relaxed),
                pressed: {
                    let bits = PRESSED.load(Ordering::Relaxed);
                    [
                        bits & 1 != 0,
                        bits & 2 != 0,
                        bits & 4 != 0,
                        bits & 8 != 0,
                    ]
                },
                vpp_on: boost.on,
                millivolts: BATT_MV.load(Ordering::Relaxed),
                link: link_label(),
            };

            let menu_open = MENU_OPEN.load(Ordering::Relaxed);
            let view = VIEW.load(Ordering::Relaxed);
            let counts = axis_counts();
            let axis = AXIS.load(Ordering::Relaxed) as usize % 3;
            let zoom = ZOOM.load(Ordering::Relaxed);
            let held = held_axis(&state.pressed);

            // Every jog since the last frame is a kick of torque. Sample the
            // deltas whatever view is up, so switching to the gantry doesn't
            // dump a hoarded spin into it.
            for axis in 0..3 {
                let delta = counts[axis] - spun_counts[axis];
                if delta != 0 && view == VIEW_GANTRY && !menu_open {
                    cube.kick(axis, delta);
                }
            }
            spun_counts = counts;
            let spinning = cube.step(FRAME_MS as f32 / 1000.0);

            let key = (
                state.detents,
                PRESSED.load(Ordering::Relaxed),
                state.vpp_on,
                view,
                state.millivolts,
                menu_open,
                MENU_SEL.load(Ordering::Relaxed),
                LINK_STATE.load(Ordering::Relaxed),
                counts,
                axis,
                zoom,
            );
            // A coasting cube changes with nothing else changing, so it gets a
            // frame of its own; every other view still draws only on change.
            let coasting = spinning && view == VIEW_GANTRY && !menu_open;
            if force || coasting || last != Some(key) {
                last = Some(key);
                match (menu_open, view) {
                    (true, _) => ui::draw_menu(
                        &mut screen,
                        &ui::MenuState {
                            selected: MENU_SEL.load(Ordering::Relaxed),
                            link: link_label(),
                            vpp_on: state.vpp_on,
                            led: led_label(),
                            view: view_label(),
                            millivolts: state.millivolts,
                        },
                    ),
                    (false, VIEW_GANTRY) => ui::draw_cube(
                        &mut screen,
                        &cube,
                        counts,
                        held,
                        zoom,
                        state.millivolts,
                        state.vpp_on,
                    ),
                    (false, 2) => ui::test_pattern(&mut screen),
                    (false, 3) => ui::all_on(&mut screen),
                    (false, _) => ui::draw(&mut screen, &state),
                }
                // Nothing to push while the panel has no rail.
                if boost.on {
                    screen.flush().await;
                }
            }

            ticker.next().await;
        }
    };

    // ---- wireless ----
    // Raw non-connectable advertising, or the connectable trouble-host link:
    // both want RADIO, so the build picks one.
    #[cfg(not(feature = "ble"))]
    let wireless = async {
        radio::init();
        let mut adv = radio::Adv::new();
        let a = adv.address();
        logln!(
            "radio: \"pico2joy\" advertising as {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}, non-connectable",
            a[5], a[4], a[3], a[2], a[1], a[0]
        );

        let mut ticker = Ticker::every(Duration::from_millis(500));
        loop {
            let on = RADIO_ON.load(Ordering::Relaxed);
            LINK_STATE.store(if on { 2 } else { 0 }, Ordering::Relaxed);
            if on {
                adv.update(&snapshot());
                adv.transmit().await;
            }
            ticker.next().await;
        }
    };

    #[cfg(feature = "ble")]
    let wireless = async {
        let a = state::device_address();
        logln!(
            "ble: \"pico2joy\" connectable as {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}, no pairing",
            a[5], a[4], a[3], a[2], a[1], a[0]
        );
        ble::run(
            ble::Claimed {
                rtc0: p.RTC0,
                timer0: p.TIMER0,
                temp: p.TEMP,
                rng: p.RNG,
                ppi_ch17: p.PPI_CH17,
                ppi_ch18: p.PPI_CH18,
                ppi_ch19: p.PPI_CH19,
                ppi_ch20: p.PPI_CH20,
                ppi_ch21: p.PPI_CH21,
                ppi_ch22: p.PPI_CH22,
                ppi_ch23: p.PPI_CH23,
                ppi_ch24: p.PPI_CH24,
                ppi_ch25: p.PPI_CH25,
                ppi_ch26: p.PPI_CH26,
                ppi_ch27: p.PPI_CH27,
                ppi_ch28: p.PPI_CH28,
                ppi_ch29: p.PPI_CH29,
                ppi_ch30: p.PPI_CH30,
                ppi_ch31: p.PPI_CH31,
            },
            snapshot,
        )
        .await
    };

    join5(usb.run(), writer, inputs, render, join(commands, wireless)).await;
}

/// Act on the highlighted menu row. Everything routes through the same request
/// flags the console commands use, so there is one implementation of each action.
fn activate_menu_item() {
    match ui::MenuItem::from_index(MENU_SEL.load(Ordering::Relaxed)) {
        ui::MenuItem::Link => {
            let on = !RADIO_ON.fetch_xor(true, Ordering::Relaxed);
            logln!("menu: link {}", if on { "on" } else { "off" });
        }
        ui::MenuItem::Rail => REQ_BOOST.store(true, Ordering::Relaxed),
        ui::MenuItem::Led => {
            let mode = (LED_MODE.load(Ordering::Relaxed) + 1) % 3;
            LED_MODE.store(mode, Ordering::Relaxed);
        }
        ui::MenuItem::Screen => REQ_VIEW.store(true, Ordering::Relaxed),
        // Nothing to activate: the row is the reading.
        ui::MenuItem::Battery => {}
        ui::MenuItem::Exit => {
            MENU_OPEN.store(false, Ordering::Relaxed);
            logln!("menu: closed");
        }
    }
}

/// A panic leaves the board in the bootloader rather than halted: halting kills
/// the executor, so USB never comes up and the board looks dead until someone
/// finds the reset button. This way a bad build is always one copy away from a
/// good one.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    reboot_to_uf2()
}

/// Reset into the bootloader's UF2 mode instead of back into this app.
fn reboot_to_uf2() -> ! {
    embassy_nrf::pac::POWER
        .gpregret()
        .write(|w| w.set_gpregret(DFU_MAGIC_UF2_RESET));
    cortex_m::peripheral::SCB::sys_reset()
}

/// Write to the host, dropping the text if nobody is draining the port.
async fn emit<'d, D: embassy_usb::driver::Driver<'d>>(tx: &mut Sender<'d, D>, s: &str) {
    let max = tx.max_packet_size() as usize;
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(max) {
        if with_timeout(Duration::from_millis(20), tx.write_packet(chunk))
            .await
            .is_err()
        {
            return;
        }
    }
    // A full-size final packet needs a zero-length one to end the transfer.
    if !bytes.is_empty() && bytes.len() % max == 0 {
        let _ = with_timeout(Duration::from_millis(20), tx.write_packet(&[])).await;
    }
}

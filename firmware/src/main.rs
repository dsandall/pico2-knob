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
mod accel;
mod media;
mod cube;
mod display;
mod gantry;
#[cfg(not(feature = "ble"))]
mod radio;
mod state;
mod ui;

use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, Ordering};

use embassy_futures::join::{join3, join5};
use embassy_nrf::config::HfclkSource;
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::pwm::{DutyCycle, SimpleConfig, SimplePwm};
use embassy_nrf::saadc::{self, ChannelConfig, Saadc, VddhDiv5Input};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::{bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Sender, State};
use embassy_usb::{Builder, Config};
use heapless::{String, Vec};

use crate::display::Display;

// CLOCK_POWER is one interrupt for two peripherals: USB's VBUS detection lives
// on the POWER half, MPSL's clock management on the CLOCK half. The `ble`
// build hands it to both. It has to: the bootloader leaves the USB events
// enabled and latched, and an interrupt nobody clears is a storm that starves
// everything below it the moment MPSL enables the line. See `ble::ClockGate`.
bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    #[cfg(not(feature = "ble"))]
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    #[cfg(feature = "ble")]
    CLOCK_POWER => usb::vbus_detect::InterruptHandler, ble::ClockGate;
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
    SAADC => saadc::InterruptHandler;
});

/// Poll interval for switches and the encoder. 1 kHz is far faster than a thumb.
const TICK: Duration = Duration::from_millis(1);
/// Consecutive stable samples (ms) needed to accept a switch edge.
const DEBOUNCE_TICKS: u8 = 4;
/// Two presses of the same button inside this many 1 kHz ticks are a double tap.
const DOUBLE_TAP_TICKS: u32 = 400;
/// Hold a button this long and the step wheel opens under your thumb.
const HOLD_TICKS: u32 = 1000;
/// Weight of the newest detent in the smoothed knob rate. Low enough that a
/// couple of fast clicks don't read as a sustained spin.
const RATE_SMOOTHING: f32 = 0.35;
/// A gap longer than this ends the gesture, and the rate starts from nothing.
const NEW_GESTURE_TICKS: u32 = 400;
/// Heartbeat: a dim 120 ms wink every 5 s. The blue LED is on P0.15 via PWM so
/// "dim" is real dimming, not a shorter blink. (The *red* LED is the LN2054
/// charger's status output - hardware, no GPIO, nothing firmware can do.)
const HEARTBEAT_PERIOD_TICKS: u32 = 5000;
const HEARTBEAT_ON_TICKS: u32 = 120;
/// Out of `SimpleConfig::default()`'s max_duty of 1000, at 1 kHz.
const HEARTBEAT_DUTY: u16 = 30;
const LED_FULL_DUTY: u16 = 1000;
/// Magic the Adafruit UF2 bootloader looks for in GPREGRET, from its `src/main.c`.
/// UF2 gives the mass-storage drive, serial gives CDC-only (what
/// `adafruit-nrfutil dfu serial` wants), and OTA gives BLE DFU - the one that
/// makes a wireless update possible at all. OTA is the "SoftDevice not yet
/// inited" variant, which is us: the SDC is a linked library, not the S140
/// binary in flash.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
const DFU_MAGIC_SERIAL_ONLY_RESET: u8 = 0x4e;
const DFU_MAGIC_OTA_RESET: u8 = 0xA8;
/// How long the orientation test pattern stays up at boot.
const SPLASH: Duration = Duration::from_millis(1500);

const SWITCH_NAMES: [&str; 4] = ["BTN1", "BTN2", "BTN3", "ENC_SW"];

const HELP: &str = concat!(
    "\r\ncommands: ? help | p pins | d cycle view (live/gantry/pattern/all-on)\r\n",
    "          i re-init display | f flip 180 | +/- contrast | e 12V rail\r\n",
    "          w wireless | m menu (knob moves, 1/2/3 select, knob press exits)\r\n",
    "          1/2/3 select axis (Z/X/Y, as the buttons do) | , . jog it\r\n",
    "          cube: hold 1/2/3 to spin that axis, knob alone zooms\r\n",
    "          gantry: 1/2/3 pick the axis, knob jogs it (needs the bridge)\r\n",
    "          l led (dark/dim/on) | v verbose | b bootloader (UF2)\r\n",
    "          #r uf2|serial|ota  reboot into a bootloader mode\r\n"
);

/// What a host gets for opening the port.
const GREETING: &str = "\r\npico2joy bring-up (nice!nano v2, embassy)\r\n";

/// Linux hands over every freshly enumerated ttyACM in cooked mode with echo
/// on (`stty -a` on a just-plugged puck says `echo`), and a tty with echo on
/// reflects everything we print straight back into the command parser. The
/// greeting alone spells `p`, `i`, `2` and then `b` - a reboot into the
/// bootloader, which on battery is where it stays until someone reflashes it.
///
/// Three things keep that from happening. Nothing is written unless DTR is
/// up, so no packet sits in the IN endpoint waiting to be delivered (and
/// echoed) during the host's `open()`, before whatever opened it has had the
/// chance to set raw mode. The greeting waits [`GREETING_DELAY`] after DTR
/// rises, which is longer than any tool takes between `open()` and
/// `tcsetattr()`. And for [`ECHO_WINDOW`] after the greeting goes out, keys
/// are held rather than dispatched while the parser watches for its own
/// greeting coming back: [`ECHO_SCORE`] received bytes that occur in this
/// line, in order, is an echo, not a person, and single keys are ignored
/// until the port is reopened. In order but not necessarily adjacent, and
/// never released on a mismatch, because the kernel's echo is lossy - one
/// byte per USB packet, and it drops most of a burst once the write URBs
/// are all in flight - so "pico2joy bring-" followed by fragments is what
/// actually comes back. A person typing in that first second gets their
/// keys when the window closes. `#` lines are never held, since nothing we
/// print can spell one that does harm.
const ECHO_PROBE: &[u8] = GREETING.trim_ascii().as_bytes();
const ECHO_WINDOW: Duration = Duration::from_millis(1000);
const ECHO_SCORE: usize = 4;
const GREETING_DELAY: Duration = Duration::from_millis(100);

type Line = String<192>;

/// Log lines waiting for the USB writer. Bounded and lossy on purpose.
static LOG: Channel<CriticalSectionRawMutex, Line, 8> = Channel::new();

/// Machine-channel lines waiting for the radio. Separate from [`LOG`] because
/// the two transports drain at their own pace and a host on one shouldn't stall
/// the other - and because only protocol lines are worth a notification.
#[cfg(feature = "ble")]
static WIRE: Channel<CriticalSectionRawMutex, Line, 8> = Channel::new();

static DETENTS: AtomicI32 = AtomicI32::new(0);
static PRESSED: AtomicU8 = AtomicU8::new(0);
static VPP_ON: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);
/// 0 = dark, 1 = dim heartbeat, 2 = full on (for finding the board).
static LED_MODE: AtomicU8 = AtomicU8::new(1);
/// Both radios self-start; the menu's `ble` row (or 'w') turns them off.
static RADIO_ON: AtomicBool = AtomicBool::new(true);
static SEQ: AtomicU8 = AtomicU8::new(0);
/// BAT+ / VDDH in millivolts, refreshed by the render loop.
static BATT_MV: AtomicU16 = AtomicU16::new(0);
/// 0 = off, 1 = starting, 2 = advertising, 3 = connected.
static LINK_STATE: AtomicU8 = AtomicU8::new(0);
static MENU_OPEN: AtomicBool = AtomicBool::new(false);
static MENU_SEL: AtomicU8 = AtomicU8::new(0);
/// 0 = live inputs, 1 = the cube, 2 = the real gantry, 3 = now playing,
/// 4 = orientation pattern, 5 = all pixels on.
static VIEW: AtomicU8 = AtomicU8::new(0);
const VIEW_CUBE: u8 = 1;
const VIEW_GANTRY: u8 = 2;
const VIEW_MUSIC: u8 = 3;
/// Gantry zoom, in detents off the resting size - see [`cube::zoom_scale`].
static ZOOM: AtomicI32 = AtomicI32::new(0);
/// Whether the gantry screen spells its positions out. The frame shows you
/// where the head is; the numbers say exactly, and get in the way of the view,
/// so they are a double tap away rather than always on.
static GANTRY_NUMBERS: AtomicBool = AtomicBool::new(false);
/// The step wheel, and what it is currently pointing at. Open only while a
/// button is held, so it can't be left on by accident.
static WHEEL_OPEN: AtomicBool = AtomicBool::new(false);
static WHEEL_SEL: AtomicU8 = AtomicU8::new(0);

/// The device's control model: three jog counters, one selected axis. The knob
/// drives the selected counter, BTN1/2/3 choose which. Everything on screen is
/// a view of this.
static AXIS_COUNTS: [AtomicI32; 3] =
    [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static AXIS: AtomicU8 = AtomicU8::new(0);

/// Which axis each button selects, in button order. The cube wanted Z, X, Y;
/// a gantry build that prefers X, Y, Z only has to change this line.
pub const BUTTON_AXIS: [usize; 3] = [2, 0, 1];
/// Why this boot happened, captured once and kept for the banner. The log
/// channel is lossy and boot is exactly when it overflows, so the one line that
/// explains a reboot loop is the one line that must not go through it.
static RESET_REASON: Mutex<CriticalSectionRawMutex, RefCell<&'static str>> =
    Mutex::new(RefCell::new("unknown"));

/// What the previous boot panicked with, waiting for someone to connect and
/// read it. Like [`RESET_REASON`], it stays out of the lossy log channel.
static PANIC_MESSAGE: Mutex<CriticalSectionRawMutex, RefCell<Option<Line>>> =
    Mutex::new(RefCell::new(None));

/// A bootloader mode the render loop should announce before we jump to it, or
/// 0 for "nothing pending". Going through the render loop is what buys the
/// screen a chance to say so - the reset itself is instant.
static REQ_REBOOT: AtomicU8 = AtomicU8::new(0);
/// The host is echoing our output back at us - see [`ECHO_PROBE`]. Set by the
/// command parser, cleared by the writer whenever DTR changes.
static ECHO_MUTED: AtomicBool = AtomicBool::new(false);
/// When the greeting last went out, in ms since boot (never 0), for the
/// [`ECHO_WINDOW`] that follows it.
static GREETED_MS: AtomicU32 = AtomicU32::new(0);

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
        VIEW_CUBE => "cube",
        VIEW_GANTRY => "gantry",
        VIEW_MUSIC => "music",
        4 => "pattern",
        5 => "all-on",
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

/// A line on the machine channel: no timestamp, because the far end is a parser
/// rather than a person. See [`gantry`] for the wire format.
fn proto_fmt(args: core::fmt::Arguments) {
    let mut line: Line = String::new();
    let _ = line.write_fmt(args);
    let _ = line.push_str("\r\n");
    #[cfg(feature = "ble")]
    let _ = WIRE.try_send(line.clone());
    let _ = LOG.try_send(line);
}

#[macro_export]
macro_rules! proto {
    ($($arg:tt)*) => { crate::proto_fmt(format_args!($($arg)*)) };
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
    //
    // MPSL wants to own the CLOCK peripheral and hand the HF clock out through
    // `request_hfclk()`, so this is a conflict waiting to be resolved in the
    // radio build - but leaving it out did not fix MPSL's init hang and does
    // cost USB the crystal it wants, so the crystal stays until the radio
    // actually comes up.
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
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
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
    let reason = reset_reason();
    RESET_REASON.lock(|cell| *cell.borrow_mut() = reason);
    logln!("boot: reset reason {reason}");
    if let Some((count, message)) = take_panic_message() {
        logln!("boot: last boot panicked ({count} in a row): {message}");
        PANIC_MESSAGE.lock(|cell| *cell.borrow_mut() = Some(message));
    }

    let commands = async {
        let mut buf = [0u8; 64];
        // The machine channel shares the port with the console: '#' opens a line
        // for [`gantry`] to parse, everything else stays a single keystroke.
        let mut proto: String<96> = String::new();
        let mut in_proto;
        // Keys held back during the echo window, see [`ECHO_PROBE`].
        let mut held: Vec<u8, 32> = Vec::new();
        // How far into the greeting the echo has got, and how many bytes have
        // agreed with it.
        let mut cursor = 0usize;
        let mut score = 0usize;
        // The greeting they belong to: a new one starts them over.
        let mut armed = 0u32;
        fn release(held: &mut Vec<u8, 32>) {
            for &byte in held.iter() {
                console_key(byte);
            }
            held.clear();
        }
        // Time left in the echo window, if it is open.
        fn echo_window() -> Option<Duration> {
            let greeted = GREETED_MS.load(Ordering::Relaxed);
            if greeted == 0 {
                return None;
            }
            let since = (Instant::now().as_millis() as u32).wrapping_sub(greeted);
            ECHO_WINDOW.as_millis().checked_sub(since as u64).map(Duration::from_millis)
        }
        loop {
            rx.wait_connection().await;
            in_proto = false;
            held.clear();
            loop {
                // Keys held for the window are released when it closes, even
                // if nothing else arrives.
                let read = match echo_window() {
                    Some(left) if !held.is_empty() => {
                        match with_timeout(left, rx.read_packet(&mut buf)).await {
                            Ok(read) => read,
                            Err(_) => {
                                release(&mut held);
                                continue;
                            }
                        }
                    }
                    _ => {
                        release(&mut held);
                        rx.read_packet(&mut buf).await
                    }
                };
                let Ok(n) = read else { break };
                for &byte in &buf[..n] {
                    if in_proto {
                        // '#' never appears inside a line, so seeing one means
                        // the last line was cut short: start the new one.
                        if byte == b'#' {
                            proto.clear();
                            continue;
                        }
                        if byte == b'\n' || byte == b'\r' {
                            in_proto = false;
                            machine_line(&proto);
                            proto.clear();
                        } else if proto.push(byte as char).is_err() {
                            // Longer than any line we define: drop it rather
                            // than parse half of one.
                            in_proto = false;
                            proto.clear();
                        }
                        continue;
                    }
                    // Opens the machine channel. Deliberately the *only* way
                    // to reach anything destructive: a single key that
                    // rebooted the puck would also fire on the tail of a
                    // protocol line whose '#' went missing, and "#s ..." ends
                    // up spelling console commands.
                    if byte == b'#' {
                        release(&mut held);
                        in_proto = true;
                        proto.clear();
                        continue;
                    }
                    if ECHO_MUTED.load(Ordering::Relaxed) {
                        continue;
                    }
                    if echo_window().is_none() {
                        release(&mut held);
                        console_key(byte);
                        continue;
                    }
                    // Inside the window: hold the key, and see whether it is
                    // the next thing an echo of the greeting would contain.
                    let greeted = GREETED_MS.load(Ordering::Relaxed);
                    if greeted != armed {
                        armed = greeted;
                        cursor = 0;
                        score = 0;
                    }
                    let _ = held.push(byte);
                    if let Some(at) = ECHO_PROBE[cursor..].iter().position(|&c| c == byte) {
                        cursor += at + 1;
                        score += 1;
                        if score >= ECHO_SCORE {
                            held.clear();
                            ECHO_MUTED.store(true, Ordering::Relaxed);
                            logln!(
                                "the host echoes what we send - console keys ignored until \
                                 the port is reopened (stty -F <port> raw -echo)"
                            );
                        }
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
                request_reboot(DFU_MAGIC_UF2_RESET);
            }
            if open != was_open {
                ECHO_MUTED.store(false, Ordering::Relaxed);
            }
            if open && !was_open {
                // Whoever opened the port is still putting it into raw mode;
                // say nothing they could echo back at us until they have.
                // See [`ECHO_PROBE`].
                Timer::after(GREETING_DELAY).await;
                if !tx.dtr() {
                    continue;
                }
                emit(&mut tx, GREETING).await;
                GREETED_MS.store((Instant::now().as_millis() as u32).max(1), Ordering::Relaxed);
                let mut line: Line = String::new();
                let reason = RESET_REASON.lock(|cell| *cell.borrow());
                let ms = Instant::now().as_millis();
                let _ = write!(line, "booted {}.{:03}s ago, reset reason: {reason}\r\n",
                               ms / 1000, ms % 1000);
                emit(&mut tx, &line).await;
                let panicked = PANIC_MESSAGE.lock(|cell| cell.borrow().clone());
                if let Some(message) = panicked {
                    emit(&mut tx, "LAST BOOT PANICKED: ").await;
                    emit(&mut tx, &message).await;
                    emit(&mut tx, "\r\n").await;
                }
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
        // Tick of the last detent, and the smoothed rate the curve reads.
        let mut last_detent: u32 = 0;
        let mut rate: f32 = 0.0;

        let mut pressed = [false; 4];
        let mut stable = [0u8; 4];
        // Tick of each button's last press, for spotting a double tap.
        let mut last_press = [0u32; 4];
        // How long each axis button has been held, in ticks.
        let mut held = [0u32; 3];
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

                            // How fast the knob is going, for [`accel`]. Ticks
                            // are milliseconds.
                            //
                            // Averaged over the last few detents, not taken from
                            // the gap that just closed: one quick pair of clicks
                            // is not a fast turn, and treating it as one is what
                            // makes acceleration feel like it is fighting you.
                            // A long gap means a new gesture, so the average
                            // starts again rather than carrying speed over from
                            // whatever happened a second ago.
                            let gap = ticks.wrapping_sub(last_detent).max(1);
                            last_detent = ticks;
                            let instant = 1000.0 / gap as f32;
                            rate = if gap > NEW_GESTURE_TICKS {
                                0.0
                            } else {
                                rate * (1.0 - RATE_SMOOTHING) + instant * RATE_SMOOTHING
                            };

                            let view = VIEW.load(Ordering::Relaxed);
                            // While the wheel is up the knob belongs to it.
                            // Not a `continue`: this loop's tail is where the
                            // ticker is awaited, and skipping that turns a 1 kHz
                            // poll into a busy spin that starves everything else.
                            if WHEEL_OPEN.load(Ordering::Relaxed) {
                                let count = gantry::STEPS_UM.len() as i32;
                                let sel = WHEEL_SEL.load(Ordering::Relaxed) as i32;
                                let next = (sel + direction).rem_euclid(count);
                                WHEEL_SEL.store(next as u8, Ordering::Relaxed);
                            } else if view == VIEW_MUSIC {
                                // The knob is the volume; the host decides how
                                // loud one detent is.
                                media::volume(direction);
                                logln!("music: vol {}", if direction > 0 { "up" } else { "down" });
                            } else {
                                // On the cube screen the buttons are momentary: the
                                // knob only spins an axis while it is held down, and
                                // with nothing held it zooms. Everywhere else
                                // BTN1/2/3 stay a latched select.
                                let held = held_axis(&pressed);
                                let axis = match (view, held) {
                                    (VIEW_CUBE, None) => None,
                                    (VIEW_CUBE, some) => some,
                                    _ => Some(AXIS.load(Ordering::Relaxed) as usize % 3),
                                };
                                match axis {
                                    // The real machine: the knob asks the printer to
                                    // move, and the screen only changes once it says
                                    // it did.
                                    Some(axis) if view == VIEW_GANTRY => {
                                        if gantry::can_jog(axis) {
                                            // One detent is worth more when the
                                            // knob is moving - see [`accel`].
                                            let steps = accel::steps_for(rate);
                                            gantry::note_gain(steps);
                                            let delta = gantry::jog(axis, direction * steps);
                                            logln!(
                                                "jog {} {} mm",
                                                ui::AXIS_NAMES[axis],
                                                ui::Millimetres(delta)
                                            );
                                        } else {
                                            logln!(
                                                "jog {} refused: {}",
                                                ui::AXIS_NAMES[axis],
                                                if !gantry::online() {
                                                    "no bridge"
                                                } else if !gantry::homed(axis) {
                                                    "not homed"
                                                } else {
                                                    "printing"
                                                }
                                            );
                                        }
                                    }
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
            }

            // Held-button gestures. Edges are handled below; this is the part
            // that needs to notice time passing rather than a change.
            for i in 0..3 {
                if pressed[i] {
                    held[i] = held[i].saturating_add(1);
                    // One second on any axis button opens the step wheel. Any
                    // button, because whichever one is under your thumb is the
                    // one you will hold.
                    if held[i] == HOLD_TICKS
                        && VIEW.load(Ordering::Relaxed) == VIEW_GANTRY
                        && !MENU_OPEN.load(Ordering::Relaxed)
                        && !WHEEL_OPEN.load(Ordering::Relaxed)
                    {
                        WHEEL_SEL.store(gantry::step_index() as u8, Ordering::Relaxed);
                        WHEEL_OPEN.store(true, Ordering::Relaxed);
                        logln!("wheel: open");
                    }
                } else {
                    held[i] = 0;
                }
            }
            // The wheel lives only as long as the hold: let go and whatever it
            // is pointing at is the new step.
            if WHEEL_OPEN.load(Ordering::Relaxed) && !pressed[..3].iter().any(|&d| d) {
                WHEEL_OPEN.store(false, Ordering::Relaxed);
                let step = gantry::set_step(WHEEL_SEL.load(Ordering::Relaxed) as usize);
                logln!("wheel: step {} mm", ui::Millimetres(step));
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
                        // On the now-playing screen the three buttons are
                        // transport, not axis select: BTN1 previous, BTN2
                        // play/pause, BTN3 next.
                        if down && i < 3
                            && VIEW.load(Ordering::Relaxed) == VIEW_MUSIC
                            && !MENU_OPEN.load(Ordering::Relaxed)
                        {
                            match i {
                                0 => { media::prev(); logln!("music: prev"); }
                                1 => { media::play_pause(); logln!("music: play/pause"); }
                                _ => { media::next(); logln!("music: next"); }
                            }
                        }

                        if down && i < 3 && !MENU_OPEN.load(Ordering::Relaxed) {
                            // One mapping everywhere, cube and gantry alike:
                            // muscle memory doesn't change screens.
                            let axis = BUTTON_AXIS[i];
                            AXIS.store(axis as u8, Ordering::Relaxed);
                            logln!("axis: {}", ui::AXIS_NAMES[axis]);

                            // Double-tap any of them to show or hide the numbers.
                            if VIEW.load(Ordering::Relaxed) == VIEW_GANTRY
                                && ticks.wrapping_sub(last_press[i]) < DOUBLE_TAP_TICKS
                            {
                                let on = !GANTRY_NUMBERS.fetch_xor(true, Ordering::Relaxed);
                                logln!("gantry: numbers {}", if on { "on" } else { "off" });
                            }
                            last_press[i] = ticks;
                        }

                        // The knob press is the menu key, and it means the
                        // same thing both ways round: in opens it, in again
                        // leaves. Inside, the three buttons are the select -
                        // whichever one falls under your thumb.
                        if down {
                            let open = MENU_OPEN.load(Ordering::Relaxed);
                            match (i, open) {
                                (3, false) => {
                                    MENU_OPEN.store(true, Ordering::Relaxed);
                                    logln!("menu: open");
                                }
                                (3, true) => {
                                    MENU_OPEN.store(false, Ordering::Relaxed);
                                    logln!("menu: closed");
                                }
                                (_, true) => activate_menu_item(),
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
                // The machine as the bridge last described it, or "offline".
                logln!(
                    "gantry {} | X{} Y{} Z{} | homed {}{}{} | step {} mm",
                    gantry::state_label(),
                    ui::Millimetres(gantry::position(0)),
                    ui::Millimetres(gantry::position(1)),
                    ui::Millimetres(gantry::position(2)),
                    if gantry::homed(0) { 'x' } else { '-' },
                    if gantry::homed(1) { 'y' } else { '-' },
                    if gantry::homed(2) { 'z' } else { '-' },
                    ui::Millimetres(gantry::step_um())
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

        const VIEWS: u8 = 6;
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
                // Tell the relay which app is on screen, so it streams only that
                // one - see the machine channel in tools/pico2joy.py.
                proto!("#view {}", view_label());
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
            // Fifteen seconds on our feet: whatever the last boot panicked
            // about, this boot is not in a loop over it.
            if ticks == 375 {
                clear_panic_box();
            }
            let pending = REQ_REBOOT.load(Ordering::Relaxed);
            if pending != 0 {
                logln!("rebooting into the bootloader ({})", bootloader_mode_label(pending));
                ui::draw_flashing(&mut screen, bootloader_mode_label(pending));
                if boost.on {
                    screen.flush().await;
                }
                // Long enough for the frame to be on the glass and the log line
                // to be on the wire.
                Timer::after_millis(250).await;
                reboot_to_bootloader(pending);
            }
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

            // Anything the knob asked the printer for goes out as one move per
            // frame, however fast it was turned.
            gantry::flush_jogs();

            // Every jog since the last frame is a kick of torque. Sample the
            // deltas whatever view is up, so switching to the gantry doesn't
            // dump a hoarded spin into it.
            for axis in 0..3 {
                let delta = counts[axis] - spun_counts[axis];
                if delta != 0 && view == VIEW_CUBE && !menu_open {
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
                // The gantry screen is a view of the host's state, so it has to
                // redraw when that changes and not when the puck's own does.
                (
                    gantry::position(0),
                    gantry::position(1),
                    gantry::position(2),
                    gantry::state_label(),
                    gantry::step_um(),
                    gantry::homed(0) as u8 | (gantry::homed(1) as u8) << 1 | (gantry::homed(2) as u8) << 2,
                    GANTRY_NUMBERS.load(Ordering::Relaxed),
                    WHEEL_OPEN.load(Ordering::Relaxed),
                    WHEEL_SEL.load(Ordering::Relaxed),
                    gantry::recent_gain(),
                    media::generation(),
                ),
            );
            // A coasting cube changes with nothing else changing, so it gets a
            // frame of its own; every other view still draws only on change.
            let coasting = spinning && view == VIEW_CUBE && !menu_open;
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
                            step_um: gantry::step_um(),
                            accel: accel::profile_label(),
                            millivolts: state.millivolts,
                        },
                    ),
                    (false, VIEW_CUBE) => ui::draw_cube(
                        &mut screen,
                        &cube,
                        counts,
                        held,
                        zoom,
                        state.millivolts,
                        state.vpp_on,
                    ),
                    (false, VIEW_GANTRY) if WHEEL_OPEN.load(Ordering::Relaxed) => ui::draw_wheel(
                        &mut screen,
                        WHEEL_SEL.load(Ordering::Relaxed) as usize,
                        state.millivolts,
                        state.vpp_on,
                    ),
                    (false, VIEW_GANTRY) => ui::draw_gantry(
                        &mut screen,
                        axis,
                        GANTRY_NUMBERS.load(Ordering::Relaxed),
                        state.millivolts,
                        state.vpp_on,
                    ),
                    (false, VIEW_MUSIC) => media::with_state(|status, vol, title, artist, art| {
                        ui::draw_nowplaying(
                            &mut screen,
                            status,
                            vol,
                            title,
                            artist,
                            art,
                            media::online(),
                        )
                    }),
                    (false, 4) => ui::test_pattern(&mut screen),
                    (false, 5) => ui::all_on(&mut screen),
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

    // A watchdog on the nRF52 survives a soft reset, so a build that starts one
    // leaves it running for everything flashed afterwards - and a build that
    // never heard of it just gets reset every 8 s, which cost us an afternoon.
    // Two rules make that safe: whoever boots into a running watchdog feeds it,
    // and only the radio build ever starts one, because only the radio can take
    // the executor down with it.
    let watchdog = async {
        use embassy_nrf::peripherals::WDT;
        use embassy_nrf::wdt::{Config as WdtConfig, Watchdog, WatchdogHandle};

        let running = WdtConfig::try_new(&p.WDT).is_some();
        let mut handle = if running {
            logln!("wdt: one was already running - feeding it");
            // SAFETY: reload register 0 is the one any build of ours enables.
            Some(unsafe { WatchdogHandle::steal::<WDT>(0) })
        } else if cfg!(feature = "ble") {
            let mut wdt_config = WdtConfig::default();
            // 32768 Hz ticks: eight seconds, far longer than any legitimate pause.
            wdt_config.timeout_ticks = 8 * 32768;
            match Watchdog::try_new::<_, 1>(p.WDT, wdt_config) {
                Ok((_wdt, [handle])) => {
                    logln!("wdt: 8 s, fed every second");
                    Some(handle)
                }
                Err(_) => None,
            }
        } else {
            None
        };

        loop {
            if let Some(handle) = handle.as_mut() {
                handle.pet();
            }
            Timer::after_millis(1000).await;
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

    join5(
        usb.run(),
        writer,
        inputs,
        render,
        join3(commands, wireless, watchdog),
    )
    .await;
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
        ui::MenuItem::Step => {
            let um = gantry::next_step();
            logln!("menu: jog step {} mm", ui::Millimetres(um));
        }
        ui::MenuItem::Accel => {
            let profile = accel::next_profile();
            logln!("menu: jog accel {profile}");
        }
        ui::MenuItem::Home => {
            gantry::request("home");
            logln!("menu: asked the bridge to home");
        }
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
///
/// It also writes the message down first. The reboot is what makes a panic
/// survivable; keeping the text is what makes it debuggable - without it every
/// panic looks identical from the outside, which is to say it looks like a
/// board that died for no reason. MPSL's assert handler panics with a file and
/// line, so this is the difference between "the radio hangs" and knowing
/// exactly which assertion it tripped.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // SAFETY: single core, and nothing else is running any more.
    let previous = unsafe { &*(&raw const PANIC_BOX) };
    let count = if previous.magic == PANIC_MAGIC {
        previous.count.saturating_add(1)
    } else {
        1
    };
    let mut box_ = PanicBox {
        magic: PANIC_MAGIC,
        count,
        len: 0,
        text: [0; PANIC_TEXT],
    };
    let mut sink = PanicWriter {
        text: &mut box_.text,
        len: 0,
    };
    let _ = write!(sink, "{info}");
    box_.len = sink.len as u32;
    // SAFETY: single core, interrupts are irrelevant now, and nothing else
    // touches this region - it is deliberately outside .bss so the reset does
    // not clear it.
    unsafe { (&raw mut PANIC_BOX).write(box_) };
    // A plain reset keeps RAM, and RAM is where the message is: going straight
    // to the bootloader hands it a machine whose memory it will scribble on.
    // Two panics in a row is a build that isn't going to come good on its own,
    // so that one goes to the bootloader and stays there.
    if count >= 2 {
        reboot_to_uf2()
    }
    cortex_m::peripheral::SCB::sys_reset()
}

const PANIC_MAGIC: u32 = 0x7069_636f;
const PANIC_TEXT: usize = 192;

#[repr(C)]
struct PanicBox {
    magic: u32,
    /// Consecutive panics. A build that panics once wants to tell you why; a
    /// build that panics every boot wants to be replaced.
    count: u32,
    len: u32,
    text: [u8; PANIC_TEXT],
}

/// `.uninit` is RAM that cortex-m-rt leaves alone at startup, which is what
/// lets a message cross a reset.
#[unsafe(link_section = ".uninit.PANIC")]
static mut PANIC_BOX: PanicBox = PanicBox {
    magic: 0,
    count: 0,
    len: 0,
    text: [0; PANIC_TEXT],
};

struct PanicWriter<'a> {
    text: &'a mut [u8; PANIC_TEXT],
    len: usize,
}

impl core::fmt::Write for PanicWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &byte in s.as_bytes() {
            if self.len == PANIC_TEXT {
                break;
            }
            self.text[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

/// The message the last panic left, if the last boot ended in one. Taken, not
/// read: a second report would be a lie about a boot that went fine.
fn take_panic_message() -> Option<(u32, Line)> {
    // SAFETY: as above, and this runs before any task could panic again.
    let box_ = unsafe { &*(&raw const PANIC_BOX) };
    if box_.magic != PANIC_MAGIC {
        return None;
    }
    // The magic stays: `clear_panic_box` drops it once this boot has proved it
    // can stay up, which is what makes `count` mean "in a row".
    let len = (box_.len as usize).min(PANIC_TEXT);
    let text = core::str::from_utf8(&box_.text[..len]).unwrap_or("<not utf8>");
    Line::try_from(text).ok().map(|line| (box_.count, line))
}

/// Forget the last panic, once this boot has been up long enough to call it a
/// good one.
fn clear_panic_box() {
    // SAFETY: as above.
    unsafe { (&raw mut PANIC_BOX).write(PanicBox { magic: 0, count: 0, len: 0, text: [0; PANIC_TEXT] }) };
}

/// Reset into the bootloader's UF2 mode instead of back into this app.
/// Why the chip came up, straight out of POWER.RESETREAS, and cleared so the
/// next boot's answer is its own. On a board with no debugger this is the only
/// thing that distinguishes "the watchdog got it", "the firmware asked for it"
/// and "someone pressed reset" - and each of those wants a different fix.
fn reset_reason() -> &'static str {
    let power = embassy_nrf::pac::POWER;
    let bits = power.resetreas().read().0;
    // Write-one-to-clear: leaving it set makes every later boot lie.
    power.resetreas().write_value(embassy_nrf::pac::power::regs::Resetreas(bits));
    match bits {
        0 => "power-on",
        b if b & (1 << 1) != 0 => "watchdog",
        b if b & (1 << 3) != 0 => "cpu lockup",
        b if b & (1 << 2) != 0 => "soft reset",
        b if b & (1 << 0) != 0 => "reset pin",
        _ => "other",
    }
}

fn reboot_to_uf2() -> ! {
    reboot_to_bootloader(DFU_MAGIC_UF2_RESET)
}

/// Ask for a reboot rather than taking one: the render loop puts "FLASHING" on
/// the screen, pushes the frame, and then jumps. A panic still resets straight
/// away - by then there may be no render loop left to ask.
fn request_reboot(magic: u8) {
    REQ_REBOOT.store(magic, Ordering::Relaxed);
}

fn bootloader_mode_label(magic: u8) -> &'static str {
    match magic {
        DFU_MAGIC_OTA_RESET => "over the air",
        DFU_MAGIC_SERIAL_ONLY_RESET => "serial DFU",
        _ => "USB drive",
    }
}

/// Leave a magic byte in GPREGRET - retained across a soft reset, cleared by a
/// power-on one - and reset into the bootloader mode it names.
fn reboot_to_bootloader(magic: u8) -> ! {
    embassy_nrf::pac::POWER
        .gpregret()
        .write(|w| w.set_gpregret(magic));
    cortex_m::peripheral::SCB::sys_reset()
}

/// One console keystroke. Everything here is a request flag or a counter, so
/// it can run from wherever the byte turned up.
fn console_key(byte: u8) {
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
        b'b' => request_reboot(DFU_MAGIC_UF2_RESET),
        _ => {}
    }
}

/// One line off the machine channel, whichever transport carried it. USB and
/// BLE both land here so the host tool speaks one protocol either way.
fn machine_line(line: &str) {
    let mut fields = line.split_ascii_whitespace();
    match fields.next() {
        // Identify: the relay asks on connect, and uses the view to pick which
        // app to stream. `#v 1` stays for older hosts that only want the ack.
        Some("?") | Some("#?") => {
            proto!("#v 1");
            proto!("#view {}", view_label());
        }
        // Reboot into a bootloader mode by name, so a host can start an update
        // without anyone touching the reset button.
        Some("r") | Some("#r") => {
            let magic = match fields.next() {
                Some("ota") => DFU_MAGIC_OTA_RESET,
                Some("serial") => DFU_MAGIC_SERIAL_ONLY_RESET,
                Some("uf2") | None => DFU_MAGIC_UF2_RESET,
                Some(other) => {
                    logln!("reset: unknown mode {other}");
                    return;
                }
            };
            proto!("#b {magic:#04x}");
            request_reboot(magic)
        }
        _ => {
            // Media lines first, then the gantry: neither answers for the
            // other's commands, so order only decides who sees a line first.
            if !media::handle_line(line) {
                gantry::handle_line(line);
            }
        }
    }
}

/// Write to the host, dropping the text if nobody is draining the port.
async fn emit<'d, D: embassy_usb::driver::Driver<'d>>(tx: &mut Sender<'d, D>, s: &str) {
    let max = tx.max_packet_size() as usize;
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(max) {
        // A packet written with nobody reading sits in the IN endpoint until
        // the next open(), and is handed over - and echoed, on a cooked tty -
        // before the opener can set raw mode. See [`ECHO_PROBE`].
        if !tx.dtr() {
            return;
        }
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

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

mod display;
mod radio;
mod ui;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use embassy_futures::join::{join, join5};
use embassy_nrf::config::HfclkSource;
use embassy_nrf::gpio::{Flex, Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::pwm::{DutyCycle, SimpleConfig, SimplePwm};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::{bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, Sender, State};
use embassy_usb::{Builder, Config};
use heapless::String;
use panic_halt as _;

use crate::display::Display;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    SPIM3 => spim::InterruptHandler<peripherals::SPI3>;
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
    "\r\ncommands: ? help | p pins | d cycle view (live/pattern/all-on)\r\n",
    "          i re-init display | f flip 180 | +/- contrast | e 12V rail\r\n",
    "          w wireless advertising | l led (dark/dim/on) | v verbose | b bootloader\r\n"
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
static RADIO_ON: AtomicBool = AtomicBool::new(true);
static REQ_HELP: AtomicBool = AtomicBool::new(false);
static REQ_PINS: AtomicBool = AtomicBool::new(false);
static REQ_BOOST: AtomicBool = AtomicBool::new(false);
static REQ_VIEW: AtomicBool = AtomicBool::new(false);
static REQ_REINIT: AtomicBool = AtomicBool::new(false);
static REQ_FLIP: AtomicBool = AtomicBool::new(false);
static REQ_BRIGHTER: AtomicBool = AtomicBool::new(false);
static REQ_DIMMER: AtomicBool = AtomicBool::new(false);

fn log_fmt(args: core::fmt::Arguments) {
    let mut line: Line = String::new();
    let ms = Instant::now().as_millis();
    let _ = write!(line, "[{:5}.{:03}] ", ms / 1000, ms % 1000);
    let _ = line.write_fmt(args);
    let _ = line.push_str("\r\n");
    // Drop the line rather than block if the host isn't draining the port.
    let _ = LOG.try_send(line);
}

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
    let p = embassy_nrf::init(config);

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
    let commands = async {
        let mut buf = [0u8; 64];
        loop {
            rx.wait_connection().await;
            while let Ok(n) = rx.read_packet(&mut buf).await {
                for &byte in &buf[..n] {
                    match byte {
                        b'?' | b'h' => REQ_HELP.store(true, Ordering::Relaxed),
                        b'p' => REQ_PINS.store(true, Ordering::Relaxed),
                        b'e' => REQ_BOOST.store(true, Ordering::Relaxed),
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

            // The 1200-baud touch: open at 1200 then close, the convention every
            // UF2 board honours for "reboot into the bootloader" (see flash.sh).
            // The DTR test is what makes it a *touch* - without it, merely opening
            // the port at 1200 reboots us, and a stale 1200 setting left on a
            // recycled ttyACM node then bounces the board into the bootloader the
            // moment anything opens it.
            if !open && tx.line_coding().data_rate() == 1200 {
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
                        detents += quarters.signum() as i32;
                        quarters = 0;
                        DETENTS.store(detents, Ordering::Relaxed);
                        logln!(
                            "ENC {} detents={detents}",
                            if step > 0 { "cw " } else { "ccw" }
                        );
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
                logln!(
                    "{line} | detents={} | 12V_EN {}",
                    DETENTS.load(Ordering::Relaxed),
                    if VPP_ON.load(Ordering::Relaxed) { "low" } else { "hi-Z" }
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

        // 0 = live inputs, 1 = orientation pattern, 2 = every pixel on.
        const VIEWS: u8 = 3;
        let mut ticker = Ticker::every(Duration::from_millis(40));
        let mut view: u8 = 0;
        let mut last = None;

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
                view = (view + 1) % VIEWS;
                logln!(
                    "view: {}",
                    match view {
                        1 => "orientation pattern",
                        2 => "all pixels on",
                        _ => "live inputs",
                    }
                );
                force = true;
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
            };

            let key = (state.detents, PRESSED.load(Ordering::Relaxed), state.vpp_on, view);
            if force || last != Some(key) {
                last = Some(key);
                match view {
                    1 => ui::test_pattern(&mut screen),
                    2 => ui::all_on(&mut screen),
                    _ => ui::draw(&mut screen, &state),
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
            if RADIO_ON.load(Ordering::Relaxed) {
                adv.update(&radio::Payload {
                    buttons: PRESSED.load(Ordering::Relaxed),
                    detents: DETENTS.load(Ordering::Relaxed) as i16,
                    uptime_s: Instant::now().as_secs() as u16,
                    flags: VPP_ON.load(Ordering::Relaxed) as u8,
                });
                adv.transmit().await;
            }
            ticker.next().await;
        }
    };

    join5(usb.run(), writer, inputs, render, join(commands, wireless)).await;
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

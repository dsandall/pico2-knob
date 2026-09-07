//! A connectable BLE link with no pairing and nothing persisted.
//!
//! [`trouble_host`] on the Nordic SoftDevice Controller ([`nrf_sdc`]), which is a
//! linked library rather than the S140 binary sitting in flash - so the app still
//! links at 0x26000 and the SoftDevice slot is left alone.
//!
//! One service, one notify characteristic, carrying the same
//! [`crate::state::Payload`] bytes the raw advertiser broadcasts. Deliberately
//! *no* Security Manager: `trouble-host`'s `security` feature is off and the
//! characteristic asks for no encryption, so a central connects and subscribes
//! with no pairing prompt and no key material on either side. This firmware has
//! no flash storage at all - power-cycle the puck and nothing records that it
//! ever spoke to anything.
//!
//! RADIO, RTC0, TIMER0, TEMP and a dozen PPI channels belong to MPSL/SDC here,
//! which is why this and [`crate::radio`] are mutually exclusive builds.

use embassy_futures::join::join3;
use embassy_futures::select::{Either3, select3};
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::interrupt::typelevel::{Binding, CLOCK_POWER, Handler};
use embassy_nrf::mode::Async;
use embassy_nrf::rng::{self, Rng};
use embassy_nrf::{Peri, bind_interrupts, peripherals};
use embassy_time::{Duration, Timer};
use nrf_sdc::mpsl::{self, MultiprotocolServiceLayer, Peripherals as MpslPeripherals};
use nrf_sdc::{self as sdc, Peripherals as SdcPeripherals};
use static_cell::StaticCell;
use trouble_host::prelude::*;

use crate::state::{ENCODED_LEN, Payload, device_address};

bind_interrupts!(struct Irqs {
    EGU0_SWI0 => mpsl::LowPrioInterruptHandler;
    RADIO => mpsl::HighPrioInterruptHandler;
    TIMER0 => mpsl::HighPrioInterruptHandler;
    RTC0 => mpsl::HighPrioInterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

/// MPSL's share of CLOCK_POWER, which USB's VBUS detection also needs (it is
/// one interrupt line for both peripherals). `main` binds the line to USB's
/// handler and to this, in that order, so the POWER events are cleared before
/// MPSL is asked about the CLOCK ones. Gated, because the line is live from the
/// moment USB comes up and MPSL's handler has nothing to be called on until
/// `mpsl_init` has run.
pub struct ClockGate;

static MPSL_UP: AtomicBool = AtomicBool::new(false);

impl Handler<CLOCK_POWER> for ClockGate {
    unsafe fn on_interrupt() {
        if MPSL_UP.load(Ordering::Relaxed) {
            unsafe { <mpsl::ClockInterruptHandler as Handler<CLOCK_POWER>>::on_interrupt() }
        }
    }
}

// SAFETY: `ClockGate` is what `main` binds to CLOCK_POWER, and it forwards to
// MPSL's handler once the gate opens - which `try_run` does before `mpsl_init`.
unsafe impl Binding<CLOCK_POWER, mpsl::ClockInterruptHandler> for Irqs {}

pub const LOCAL_NAME: &str = "pico2joy";

const CONNECTIONS_MAX: usize = 1;
/// ATT, plus headroom.
const L2CAP_CHANNELS_MAX: usize = 3;
/// How often a subscribed central hears about the knob.
const NOTIFY_INTERVAL: Duration = Duration::from_millis(100);
/// Controller working memory. nrf-sdc-sys exports the per-feature sizes but no
/// aggregate, so this is sized generously for one peripheral link with default
/// buffers; `build()` reports it if that is ever wrong.
const SDC_MEM_SIZE: usize = 4096;

/// One notification's worth of a line. The default ATT MTU is 23, so 20 bytes
/// is what fits without assuming the central negotiated anything larger; longer
/// lines simply take more notifications and the host reassembles on the newline.
const CHUNK: usize = 20;

/// 128-bit UUIDs, randomly chosen: a puck service carrying the state snapshot
/// and the machine channel. The macros want literals, so they live here rather
/// than in named constants.
#[gatt_service(uuid = "9f4a0000-1d2b-4c65-9c31-7f9a2b0d5e01")]
struct PuckService {
    /// The same eight bytes the advertiser carries: format, seq, buttons,
    /// detents, uptime, flags. Readable so a client can poll it, notify so it
    /// doesn't have to.
    #[characteristic(uuid = "9f4a0001-1d2b-4c65-9c31-7f9a2b0d5e01", read, notify)]
    state: [u8; ENCODED_LEN],

    /// Host to puck, the same `#`-lines the USB console takes: writes are
    /// concatenated until a newline, then dispatched exactly as if they had
    /// arrived over the wire. Write-without-response so a jog burst doesn't
    /// wait for an ack it has no use for.
    #[characteristic(uuid = "9f4a0002-1d2b-4c65-9c31-7f9a2b0d5e01", write, write_without_response)]
    rx: [u8; CHUNK],

    /// Puck to host: the same lines going the other way, in [`CHUNK`] pieces.
    #[characteristic(uuid = "9f4a0003-1d2b-4c65-9c31-7f9a2b0d5e01", notify)]
    tx: heapless::Vec<u8, CHUNK>,
}

#[gatt_server(connections_max = CONNECTIONS_MAX, attribute_table_size = 128)]
struct Server {
    puck: PuckService,
}

/// Reassemble host writes into lines. A line longer than this is not one of
/// ours, so it gets dropped rather than parsed in halves.
#[derive(Default)]
struct LineBuffer {
    line: heapless::String<96>,
}

impl LineBuffer {
    fn feed(&mut self, data: &[u8]) {
        for &byte in data {
            if byte == b'\n' || byte == b'\r' {
                if !self.line.is_empty() {
                    crate::machine_line(&self.line);
                    self.line.clear();
                }
            } else if self.line.push(byte as char).is_err() {
                self.line.clear();
            }
        }
    }
}

/// The peripherals MPSL and SDC claim, handed over in one move so `main` can't
/// keep a copy of any of them.
pub struct Claimed {
    pub rtc0: Peri<'static, peripherals::RTC0>,
    pub timer0: Peri<'static, peripherals::TIMER0>,
    pub temp: Peri<'static, peripherals::TEMP>,
    pub rng: Peri<'static, peripherals::RNG>,
    pub ppi_ch17: Peri<'static, peripherals::PPI_CH17>,
    pub ppi_ch18: Peri<'static, peripherals::PPI_CH18>,
    pub ppi_ch19: Peri<'static, peripherals::PPI_CH19>,
    pub ppi_ch20: Peri<'static, peripherals::PPI_CH20>,
    pub ppi_ch21: Peri<'static, peripherals::PPI_CH21>,
    pub ppi_ch22: Peri<'static, peripherals::PPI_CH22>,
    pub ppi_ch23: Peri<'static, peripherals::PPI_CH23>,
    pub ppi_ch24: Peri<'static, peripherals::PPI_CH24>,
    pub ppi_ch25: Peri<'static, peripherals::PPI_CH25>,
    pub ppi_ch26: Peri<'static, peripherals::PPI_CH26>,
    pub ppi_ch27: Peri<'static, peripherals::PPI_CH27>,
    pub ppi_ch28: Peri<'static, peripherals::PPI_CH28>,
    pub ppi_ch29: Peri<'static, peripherals::PPI_CH29>,
    pub ppi_ch30: Peri<'static, peripherals::PPI_CH30>,
    pub ppi_ch31: Peri<'static, peripherals::PPI_CH31>,
}

/// Bring up MPSL, the controller and the host, then advertise and serve for as
/// long as the board is powered. `snapshot` is called for each notification.
///
/// The LFCLK runs off the internal RC with periodic calibration rather than the
/// module's crystal: RC costs nothing at this duty cycle and keeps the firmware
/// working on a board that doesn't have one.
/// Log a step, then give the USB writer a moment to actually put it on the wire
/// before doing something that might not come back. Bringing up MPSL and the
/// controller is synchronous, so a hang in there stops the whole executor -
/// which makes "the last line you saw" the only debugging tool available.
async fn step(what: &str) {
    crate::logln!("ble: {}", what);
    Timer::after_millis(80).await;
}

pub async fn run<F: FnMut() -> Payload>(p: Claimed, snapshot: F) -> ! {
    // Let the console come up first, so a stack that wedges bringing the radio
    // up still leaves a board you can talk to; and the menu's `ble` row (or
    // 'w') can hold it off altogether.
    Timer::after_millis(1500).await;
    while !crate::RADIO_ON.load(core::sync::atomic::Ordering::Relaxed) {
        Timer::after_millis(100).await;
    }

    // Report a dead stack over the USB console rather than halting silently:
    // the rest of the firmware carries on either way.
    if let Err(e) = try_run(p, snapshot).await {
        crate::logln!("ble: stack failed: {:?}", e);
    }
    core::future::pending().await
}

async fn try_run<F: FnMut() -> Payload>(p: Claimed, mut snapshot: F) -> Result<(), sdc::Error> {
    // The RC source, matching what is actually running: embassy starts the LFCLK
    // for its RTC1 time driver long before we get here, and it starts it on RC.
    let lfclk = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_CTIV as u8,
        rc_temp_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_TEMP_CTIV as u8,
        // What the recommended calibration cadence above buys you.
        accuracy_ppm: 500,
        // Don't busy-wait inside `mpsl_init` for the LFCLK to report started:
        // embassy started it long before we got here, and a spin inside init
        // is exactly what turns a stuck interrupt into a dark board.
        skip_wait_lfclk_started: true,
    };

    crate::LINK_STATE.store(1, core::sync::atomic::Ordering::Relaxed);
    step("mpsl init").await;
    let mpsl_p = MpslPeripherals::new(p.rtc0, p.timer0, p.temp, p.ppi_ch19, p.ppi_ch30, p.ppi_ch31);
    static MPSL: StaticCell<MultiprotocolServiceLayer<'static>> = StaticCell::new();

    // What the clocks and the shared interrupt are doing on the way in. embassy
    // starts the LFCLK for its RTC1 time driver during `init`, so it is already
    // running on RC here (stat 0x00010000, src 0), and the POWER half of
    // CLOCK_POWER has USB's events armed. Worth a line every time: when
    // `mpsl_init` didn't come back on this board, this is what found it.
    {
        let clock = embassy_nrf::pac::CLOCK;
        let power = embassy_nrf::pac::POWER;
        crate::logln!(
            "ble: lfclk stat={:#010x} src={:#x}, hfclk stat={:#010x}",
            clock.lfclkstat().read().0,
            clock.lfclksrc().read().0,
            clock.hfclkstat().read().0,
        );
        // The POWER half of the shared interrupt: what is enabled, and what is
        // already latched. The bootloader's USB stack leaves USBDETECTED and
        // USBPWRRDY armed, which is exactly what used to storm here.
        crate::logln!(
            "ble: power inten={:#010x} usbdetected={} usbpwrrdy={} usbremoved={}",
            power.intenset().read().0,
            power.events_usbdetected().read(),
            power.events_usbpwrrdy().read(),
            power.events_usbremoved().read(),
        );
        Timer::after_millis(80).await;
    }

    MPSL_UP.store(true, Ordering::Relaxed);
    let mpsl = MPSL.init(MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk)?);
    step("mpsl up").await;

    // USB runs off the crystal, and MPSL now decides when the crystal runs: a
    // standing request keeps it on between radio events, where the controller
    // would otherwise let it stop.
    let _hfclk = mpsl.request_hfclk().await?;
    step("hfclk held for usb").await;

    step("controller init").await;
    let sdc_p = SdcPeripherals::new(
        p.ppi_ch17, p.ppi_ch18, p.ppi_ch20, p.ppi_ch21, p.ppi_ch22, p.ppi_ch23, p.ppi_ch24,
        p.ppi_ch25, p.ppi_ch26, p.ppi_ch27, p.ppi_ch28, p.ppi_ch29,
    );
    static RNG: StaticCell<Rng<'static, Async>> = StaticCell::new();
    let rng = RNG.init(Rng::new(p.rng, Irqs));
    static SDC_MEM: StaticCell<sdc::Mem<SDC_MEM_SIZE>> = StaticCell::new();
    let controller = sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .build(sdc_p, rng, mpsl, SDC_MEM.init(sdc::Mem::new()))?;

    step("host init").await;
    // Same random static address the raw advertiser used, so the puck keeps one
    // identity however it was built.
    let address = Address::random(device_address());
    static RESOURCES: StaticCell<HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX>> =
        StaticCell::new();
    let resources = RESOURCES.init(HostResources::new());
    let stack = trouble_host::new(controller, resources)
        .set_random_address(address)
        .build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let Ok(server) = Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: LOCAL_NAME,
        appearance: &appearance::human_interface_device::GENERIC_HUMAN_INTERFACE_DEVICE,
    })) else {
        crate::logln!("ble: attribute table too small");
        return Ok(());
    };

    let mut adv_data = [0u8; 31];
    let Ok(adv_len) = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(LOCAL_NAME.as_bytes()),
        ],
        &mut adv_data,
    ) else {
        crate::logln!("ble: advertising data too long");
        return Ok(());
    };

    step("advertising, connectable, no pairing").await;
    crate::LINK_STATE.store(2, core::sync::atomic::Ordering::Relaxed);
    let serve = async {
        loop {
            let acceptor = match peripheral
                .advertise(
                    &Default::default(),
                    Advertisement::ConnectableScannableUndirected {
                        adv_data: &adv_data[..adv_len],
                        scan_data: &[],
                    },
                )
                .await
            {
                Ok(acceptor) => acceptor,
                Err(_) => {
                    Timer::after_millis(500).await;
                    continue;
                }
            };

            let Ok(conn) = acceptor.accept().await else { continue };
            let Ok(conn) = conn.with_attribute_server(&server) else { continue };
            crate::LINK_STATE.store(3, core::sync::atomic::Ordering::Relaxed);
            crate::logln!("ble: connected");

            // The central picks the connection parameters, and this puck has no
            // say in them. The standard way for a peripheral to ask is the GAP
            // "Peripheral Preferred Connection Parameters" characteristic, which
            // is a TODO in trouble-host 0.8 (`gap.rs`, `PeripheralConfig`); the
            // other way, an HCI LE Connection Update, needs
            // `sdc_hci_cmd_le_conn_update`, which the controller only links in
            // for a build that supports the central role - flash we would be
            // spending to get one symbol.
            //
            // So a Linux host's defaults apply, and its supervision timeout is
            // short enough that one missed window tears the link down. That is
            // survivable rather than fatal: `BleLink` in tools/pico2joy.py puts
            // the link straight back and the relay resends everything, so a
            // dropout costs a few seconds. If it ever needs to cost nothing,
            // the fix is BlueZ's `[LE] ConnectionSupervisionTimeout` on the host
            // - see the README - or PPCP here once the stack grows it.

            let mut incoming = LineBuffer::default();
            loop {
                let next = select3(
                    conn.next(),
                    Timer::after(NOTIFY_INTERVAL),
                    crate::WIRE.receive(),
                );
                match next.await {
                    Either3::First(GattConnectionEvent::Disconnected { reason }) => {
                        crate::logln!("ble: disconnected, reason {:?}", reason);
                        crate::LINK_STATE.store(2, core::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    Either3::First(GattConnectionEvent::Gatt { event }) => {
                        // The machine channel is the only write we care about;
                        // nothing here is gated on permissions, so every request
                        // is served either way.
                        if let GattEvent::Write(write) = &event {
                            if write.handle() == server.puck.rx.handle {
                                write.with_data(|_, data| incoming.feed(data));
                            }
                        }
                        if let Ok(reply) = event.accept() {
                            reply.send().await;
                        }
                    }
                    Either3::First(_) => {}
                    Either3::Second(()) => {
                        let encoded = snapshot().encode();
                        if server
                            .puck
                            .state
                            .notify(&conn, &encoded, true)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // A protocol line the puck wants to send, in MTU-sized
                    // pieces. The host joins them up again on the newline.
                    Either3::Third(line) => {
                        let mut failed = false;
                        for chunk in line.as_bytes().chunks(CHUNK) {
                            let piece = heapless::Vec::<u8, CHUNK>::from_slice(chunk)
                                .unwrap_or_default();
                            if server.puck.tx.notify(&conn, &piece, true).await.is_err() {
                                failed = true;
                                break;
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                }
            }
        }
    };

    // The host runner's errors are its own business; log-and-retry lives inside
    // `serve`, and none of these three is expected to return.
    let host = async {
        let _ = runner.run().await;
    };
    join3(mpsl.run(), host, serve).await;
    Ok(())
}

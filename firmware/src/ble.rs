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
use embassy_futures::select::{Either, select};
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
    CLOCK_POWER => mpsl::ClockInterruptHandler;
    RADIO => mpsl::HighPrioInterruptHandler;
    TIMER0 => mpsl::HighPrioInterruptHandler;
    RTC0 => mpsl::HighPrioInterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

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

/// 128-bit UUIDs, randomly chosen: a puck service with one state characteristic.
/// The macros want literals, so they live here rather than in named constants.
#[gatt_service(uuid = "9f4a0000-1d2b-4c65-9c31-7f9a2b0d5e01")]
struct PuckService {
    /// The same eight bytes the advertiser carries: format, seq, buttons,
    /// detents, uptime, flags. Readable so a client can poll it, notify so it
    /// doesn't have to.
    #[characteristic(uuid = "9f4a0001-1d2b-4c65-9c31-7f9a2b0d5e01", read, notify)]
    state: [u8; ENCODED_LEN],
}

#[gatt_server(connections_max = CONNECTIONS_MAX, attribute_table_size = 64)]
struct Server {
    puck: PuckService,
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
    // Nothing happens until asked ('w' on the console). A board that wedges
    // bringing the radio up still boots with its console and screen intact,
    // which is the difference between a bad build and a dark board.
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
    let lfclk = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_RC as u8,
        rc_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_CTIV as u8,
        rc_temp_ctiv: mpsl::raw::MPSL_RECOMMENDED_RC_TEMP_CTIV as u8,
        // What the recommended calibration cadence above buys you.
        accuracy_ppm: 500,
        skip_wait_lfclk_started: false,
    };

    step("mpsl init").await;
    let mpsl_p = MpslPeripherals::new(p.rtc0, p.timer0, p.temp, p.ppi_ch19, p.ppi_ch30, p.ppi_ch31);
    static MPSL: StaticCell<MultiprotocolServiceLayer<'static>> = StaticCell::new();
    let mpsl = MPSL.init(MultiprotocolServiceLayer::new(mpsl_p, Irqs, lfclk)?);

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
            crate::logln!("ble: connected");

            loop {
                match select(conn.next(), Timer::after(NOTIFY_INTERVAL)).await {
                    Either::First(GattConnectionEvent::Disconnected { reason }) => {
                        crate::logln!("ble: disconnected, reason {:?}", reason);
                        break;
                    }
                    Either::First(GattConnectionEvent::Gatt { event }) => {
                        // Nothing here is gated on permissions, so every request
                        // is simply served.
                        if let Ok(reply) = event.accept() {
                            reply.send().await;
                        }
                    }
                    Either::First(_) => {}
                    Either::Second(()) => {
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

//! Wireless without a stack: raw BLE advertising straight off the RADIO peripheral.
//!
//! Non-connectable undirected advertising (`ADV_NONCONN_IND`) on channels
//! 37/38/39, carrying the live input state as manufacturer data. Nothing ever
//! connects, so there is no pairing prompt to click through, no bond, no keys,
//! and nothing cached on the host afterwards - the whole protocol is "shout the
//! state twice a second and let anyone who cares listen".
//!
//! No SoftDevice involved: the app stays at 0x26000 and the S140 sitting in
//! flash is untouched. RADIO does need HFXO, which `embassy_nrf::init` already
//! starts for USB.

use core::sync::atomic::{Ordering, compiler_fence};

use embassy_futures::yield_now;
use embassy_nrf::pac;
use embassy_nrf::pac::radio::vals;

use crate::state::{Payload, device_address};

/// The access address every BLE advertising packet uses, and the CRC the spec
/// fixes alongside it.
const ADV_ACCESS_ADDRESS: u32 = 0x8E89_BED6;
const CRC_POLY: u32 = 0x0000_065B;
const CRC_INIT: u32 = 0x0055_5555;

/// Advertising channel index paired with its MHz offset from 2400.
const ADV_CHANNELS: [(u8, u8); 3] = [(37, 2), (38, 26), (39, 80)];

const PDU_ADV_NONCONN_IND: u8 = 0x02;
const PDU_TX_ADD_RANDOM: u8 = 1 << 6;

const AD_FLAGS: u8 = 0x01;
const AD_COMPLETE_LOCAL_NAME: u8 = 0x09;
const AD_MANUFACTURER_DATA: u8 = 0xFF;
/// LE General Discoverable + BR/EDR not supported.
const FLAGS_LE_ONLY: u8 = 0x06;
/// 0xFFFF is the company ID reserved for testing, which is exactly what this is.
const COMPANY_ID: u16 = 0xFFFF;

pub const LOCAL_NAME: &[u8] = b"pico2joy";

/// The payload below the company ID is [`crate::state::Payload`], so the
/// advertisement and the connectable link speak the same bytes.
/// One advertising PDU, laid out the way RADIO's EasyDMA wants to read it:
/// S0 (the PDU header byte), then LENGTH, then that many payload bytes.
#[repr(align(4))]
pub struct Adv {
    buf: [u8; 39],
    address: [u8; 6],
}

impl Adv {
    pub fn new() -> Self {
        Self {
            buf: [0; 39],
            address: device_address(),
        }
    }

    pub fn address(&self) -> [u8; 6] {
        self.address
    }

    /// Rebuild the PDU around a fresh state snapshot.
    pub fn update(&mut self, state: &Payload) {
        let encoded = state.encode();
        self.buf[0] = PDU_ADV_NONCONN_IND | PDU_TX_ADD_RANDOM;
        self.buf[2..8].copy_from_slice(&self.address);
        let mut at = 8;

        let mut push = |bytes: &[u8]| {
            self.buf[at] = bytes.len() as u8;
            self.buf[at + 1..at + 1 + bytes.len()].copy_from_slice(bytes);
            at += 1 + bytes.len();
        };

        push(&[AD_FLAGS, FLAGS_LE_ONLY]);

        let mut name = [0u8; 1 + 16];
        name[0] = AD_COMPLETE_LOCAL_NAME;
        name[1..1 + LOCAL_NAME.len()].copy_from_slice(LOCAL_NAME);
        push(&name[..1 + LOCAL_NAME.len()]);

        let company = COMPANY_ID.to_le_bytes();
        let mut mfg = [0u8; 3 + crate::state::ENCODED_LEN];
        mfg[0] = AD_MANUFACTURER_DATA;
        mfg[1..3].copy_from_slice(&company);
        mfg[3..].copy_from_slice(&encoded);
        push(&mfg);

        // LENGTH covers AdvA plus the AD structures, and never the two bytes of
        // header that precede it.
        self.buf[1] = (at - 2) as u8;
    }

    /// Send the current PDU once on each advertising channel.
    pub async fn transmit(&self) {
        let r = pac::RADIO;
        for (channel, freq) in ADV_CHANNELS {
            r.frequency().write(|w| w.set_frequency(freq));
            // Bit 6 of DATAWHITEIV is hard-wired high, so the channel index is
            // the whole whitening seed.
            r.datawhiteiv().write(|w| w.set_datawhiteiv(channel));
            r.packetptr().write_value(self.buf.as_ptr() as u32);

            r.events_ready().write_value(0);
            r.events_end().write_value(0);
            r.events_disabled().write_value(0);
            compiler_fence(Ordering::SeqCst);

            r.tasks_txen().write_value(1);
            while r.events_ready().read() == 0 {}
            r.tasks_start().write_value(1);
            while r.events_end().read() == 0 {}
            r.tasks_disable().write_value(1);
            while r.events_disabled().read() == 0 {}
            compiler_fence(Ordering::SeqCst);

            // Ramp-up plus a 32-byte packet is ~0.6 ms of spinning; hand the
            // executor back between channels so the 1 kHz input poll keeps up.
            yield_now().await;
        }
    }
}

/// One-time RADIO setup for BLE 1 Mbps advertising packets.
pub fn init() {
    let r = pac::RADIO;
    r.mode().write(|w| w.set_mode(vals::Mode::Ble1mbit));
    r.txpower().write(|w| w.set_txpower(vals::Txpower::_0dBm));

    r.pcnf0().write(|w| {
        w.set_s0len(true); // the PDU header byte
        w.set_lflen(8); // 8-bit length field
        w.set_s1len(0);
        w.set_plen(vals::Plen::_8bit);
    });
    r.pcnf1().write(|w| {
        w.set_maxlen(37);
        w.set_statlen(0);
        w.set_balen(3); // 3 base bytes + 1 prefix byte = 4-byte access address
        w.set_endian(vals::Endian::Little);
        w.set_whiteen(true);
    });

    r.base0().write_value(ADV_ACCESS_ADDRESS << 8);
    r.prefix0()
        .write(|w| w.set_ap0((ADV_ACCESS_ADDRESS >> 24) as u8));
    r.txaddress().write(|w| w.set_txaddress(0));

    r.crccnf().write(|w| {
        w.set_len(vals::Len::Three);
        w.set_skipaddr(vals::Skipaddr::Skip);
    });
    r.crcpoly().write(|w| w.set_crcpoly(CRC_POLY));
    r.crcinit().write(|w| w.set_crcinit(CRC_INIT));
}

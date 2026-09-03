//! The state snapshot both radios shout, in one place so the wire format can't
//! drift between them (or from `tools/watch_pico2joy.py`).

/// Bumped whenever the byte layout below changes, so a host decoder can refuse
/// to guess instead of misreading it.
pub const FORMAT: u8 = 2;

/// Wire size of [`Payload::encode`].
pub const ENCODED_LEN: usize = 10;

pub struct Payload {
    /// Wraps; lets a listener spot dropped packets.
    pub seq: u8,
    /// bit0..2: BTN1..3. bit3: ENC_SW.
    pub buttons: u8,
    pub detents: i16,
    pub uptime_s: u16,
    /// bit0: VPP rail enabled.
    pub flags: u8,
    /// BAT+ / VDDH in millivolts. See [`percent_from_mv`] for the caveat.
    pub millivolts: u16,
}

impl Payload {
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let detents = self.detents.to_le_bytes();
        let uptime = self.uptime_s.to_le_bytes();
        let millivolts = self.millivolts.to_le_bytes();
        [
            FORMAT,
            self.seq,
            self.buttons,
            detents[0],
            detents[1],
            uptime[0],
            uptime[1],
            self.flags,
            millivolts[0],
            millivolts[1],
        ]
    }
}

/// A LiPo's discharge curve is flat in the middle and steep at both ends, so a
/// straight voltage-to-percent line is useless. This is the usual piecewise
/// approximation, interpolated between the breakpoints.
///
/// Caveat worth remembering when reading it: the nice!nano v2 senses the cell
/// through VDDH, which is the charger's output. On USB with no battery it reads
/// the charger holding ~4.2 V, so it will claim a full cell that isn't there.
pub fn percent_from_mv(mv: u16) -> u8 {
    const CURVE: [(u16, u8); 12] = [
        (4200, 100),
        (4100, 90),
        (4000, 80),
        (3950, 70),
        (3900, 60),
        (3850, 50),
        (3800, 40),
        (3750, 30),
        (3700, 20),
        (3600, 10),
        (3400, 5),
        (3000, 0),
    ];

    if mv >= CURVE[0].0 {
        return 100;
    }
    for window in CURVE.windows(2) {
        let (high_mv, high_pct) = window[0];
        let (low_mv, low_pct) = window[1];
        if mv >= low_mv {
            let span = (high_mv - low_mv) as u32;
            let above = (mv - low_mv) as u32;
            let range = (high_pct - low_pct) as u32;
            return (low_pct as u32 + above * range / span) as u8;
        }
    }
    0
}

/// The puck's BLE identity: a random static address derived from FICR.DEVICEADDR,
/// so it is stable for this board, unique between boards, and carries the two
/// high bits the spec requires. Shared so the raw advertiser and the connectable
/// link present the same address.
pub fn device_address() -> [u8; 6] {
    let lo = embassy_nrf::pac::FICR.deviceaddr(0).read();
    let hi = embassy_nrf::pac::FICR.deviceaddr(1).read();
    let mut address = [
        lo as u8,
        (lo >> 8) as u8,
        (lo >> 16) as u8,
        (lo >> 24) as u8,
        hi as u8,
        (hi >> 8) as u8,
    ];
    address[5] |= 0xC0;
    address
}

//! The state snapshot both radios shout, in one place so the wire format can't
//! drift between them (or from `tools/watch_pico2joy.py`).

/// Bumped whenever the byte layout below changes, so a host decoder can refuse
/// to guess instead of misreading it.
pub const FORMAT: u8 = 1;

/// Wire size of [`Payload::encode`].
pub const ENCODED_LEN: usize = 8;

pub struct Payload {
    /// Wraps; lets a listener spot dropped packets.
    pub seq: u8,
    /// bit0..2: BTN1..3. bit3: ENC_SW.
    pub buttons: u8,
    pub detents: i16,
    pub uptime_s: u16,
    /// bit0: VPP rail enabled.
    pub flags: u8,
}

impl Payload {
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let detents = self.detents.to_le_bytes();
        let uptime = self.uptime_s.to_le_bytes();
        [
            FORMAT,
            self.seq,
            self.buttons,
            detents[0],
            detents[1],
            uptime[0],
            uptime[1],
            self.flags,
        ]
    }
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

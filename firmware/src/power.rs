//! The USB power profile: what the puck does differently when it is powered
//! from somebody else's USB port rather than its own cell.
//!
//! On the X-Carve the puck hangs off the Raspberry Pi's USB, and the Pi's 5 V
//! sagged far enough at the router's inrush to brown out - every USB device
//! re-enumerated and the CNC controller reset with them. The puck is a small
//! load next to the Pi, so this cannot fix that on its own (see the README), but
//! it can make sure the puck draws as little as it reasonably can, draws it
//! *flat*, and gets out of the way entirely while the router spins up:
//!
//! - the LED stays dark;
//! - the OLED contrast is capped at [`CONTRAST_CAP`], and panel current is
//!   proportional to contrast;
//! - the 12 V panel rail is soft-started (see `Boost::soft_start` in
//!   `main.rs`) and dropped after [`IDLE_MS`] without input - any input wakes
//!   it, and that first input only wakes, it does not act;
//! - the radio stays quiet while a host has the USB port open, since the host
//!   is plainly not talking over BLE;
//! - `#quiet <ms>` on the machine channel drops the rail and the LED for that
//!   long, whatever else is going on, and then puts them back.
//!
//! The profile is chosen by [`Mode`]: `auto` (the default) means "on whenever
//! VBUS is present", `usb` forces it on, `full` forces it off. Why VBUS and not
//! "VBUS and no cell": the nice!nano senses the cell through VDDH, which reads
//! VBUS less a diode drop whenever USB is in, cell or no cell - so the cell is
//! invisible exactly when it matters. And a puck on USB *with* a cell is
//! charging it from that same port, which is more reason to keep the rest of
//! the draw down, not less.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

/// Drop the 12 V rail after this long without a knob turn or a button.
pub const IDLE_MS: u32 = 30_000;
/// Contrast ceiling under the profile. The vendor default is 0x4F, and the
/// SH1107's segment current is linear in contrast (ISEG = a/256 x IREF x 32),
/// so 0x2F draws ~60% of the default's panel current and still reads fine
/// indoors.
pub const CONTRAST_CAP: u8 = 0x2F;
/// Longest `#quiet` honoured, so a corrupted or malicious line can't keep the
/// panel dark for minutes.
pub const QUIET_MAX_MS: u32 = 10_000;

#[derive(Copy, Clone, PartialEq)]
pub enum Mode {
    Auto = 0,
    Usb = 1,
    Full = 2,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Auto as u8);
/// Whether a host has the CDC port open (DTR up), refreshed by the USB writer.
static USB_LINK: AtomicBool = AtomicBool::new(false);
/// Last knob turn, button edge or console key, ms since boot.
static LAST_INPUT_MS: AtomicU32 = AtomicU32::new(0);
/// `#quiet` holds until this time (ms since boot), or 0 for no hold.
static QUIET_UNTIL_MS: AtomicU32 = AtomicU32::new(0);
/// The rail is down because the puck went idle, and stays "dark" until the
/// render loop has it back up. Inputs that arrive while this is set wake the
/// panel and do nothing else.
static IDLE_DARK: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u32 {
    embassy_time::Instant::now().as_millis() as u32
}

pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Usb,
        2 => Mode::Full,
        _ => Mode::Auto,
    }
}

pub fn set_mode(mode: Mode) {
    MODE.store(mode as u8, Ordering::Relaxed);
    // A mode change is somebody at the puck or the console: count it as input,
    // so switching to `usb` doesn't drop the panel the same instant.
    touch();
}

pub fn next_mode() -> Mode {
    let next = match mode() {
        Mode::Auto => Mode::Usb,
        Mode::Usb => Mode::Full,
        Mode::Full => Mode::Auto,
    };
    set_mode(next);
    next
}

/// VBUS as the USB regulator sees it.
pub fn vbus() -> bool {
    embassy_nrf::pac::POWER.usbregstatus().read().vbusdetect()
}

/// Is the USB profile in force right now?
pub fn usb_profile() -> bool {
    match mode() {
        Mode::Usb => true,
        Mode::Full => false,
        Mode::Auto => vbus(),
    }
}

/// What the menu row and `p` say: the mode, and what `auto` resolved to.
pub fn profile_label() -> &'static str {
    match (mode(), usb_profile()) {
        (Mode::Auto, true) => "auto:usb",
        (Mode::Auto, false) => "auto:full",
        (Mode::Usb, _) => "usb",
        (Mode::Full, _) => "full",
    }
}

pub fn set_usb_link(open: bool) {
    USB_LINK.store(open, Ordering::Relaxed);
}

pub fn usb_link() -> bool {
    USB_LINK.load(Ordering::Relaxed)
}

/// Whether the radio may advertise: the user hasn't switched it off, and the
/// profile isn't holding it off because the USB link is live.
pub fn radio_allowed() -> bool {
    crate::RADIO_ON.load(Ordering::Relaxed) && !radio_held()
}

/// The profile, not the user, is what keeps the radio quiet.
pub fn radio_held() -> bool {
    usb_profile() && usb_link()
}

/// Note an input. Returns true if the panel was idle-dark, in which case the
/// input has woken it and should do nothing else.
pub fn touch() -> bool {
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    IDLE_DARK.load(Ordering::Relaxed)
}

/// No input for [`IDLE_MS`], under the profile.
pub fn idle() -> bool {
    usb_profile() && now_ms().wrapping_sub(LAST_INPUT_MS.load(Ordering::Relaxed)) >= IDLE_MS
}

/// Seconds until [`idle`], for `p`; `None` when the profile doesn't idle.
pub fn idle_in_s() -> Option<u32> {
    if !usb_profile() {
        return None;
    }
    let since = now_ms().wrapping_sub(LAST_INPUT_MS.load(Ordering::Relaxed));
    Some(IDLE_MS.saturating_sub(since) / 1000)
}

pub fn set_idle_dark(dark: bool) {
    IDLE_DARK.store(dark, Ordering::Relaxed);
}

pub fn idle_dark() -> bool {
    IDLE_DARK.load(Ordering::Relaxed)
}

/// `#quiet <ms>`: hold the rail and the LED down for that long, from now. A
/// second one extends or shortens the hold to its own end; `#quiet 0` ends it.
pub fn quiet(ms: u32) -> u32 {
    let ms = ms.min(QUIET_MAX_MS);
    let until = if ms == 0 { 0 } else { now_ms().wrapping_add(ms).max(1) };
    QUIET_UNTIL_MS.store(until, Ordering::Relaxed);
    ms
}

/// Is a `#quiet` hold running?
pub fn quiet_active() -> bool {
    let until = QUIET_UNTIL_MS.load(Ordering::Relaxed);
    if until == 0 {
        return false;
    }
    // Signed difference, so the comparison survives the u32 ms wrap (49 days).
    if (until.wrapping_sub(now_ms()) as i32) > 0 {
        true
    } else {
        QUIET_UNTIL_MS.store(0, Ordering::Relaxed);
        false
    }
}

/// The LED is dark under the profile and during a quiet hold, whatever its
/// own mode says.
pub fn led_forced_dark() -> bool {
    usb_profile() || quiet_active()
}

//! ER-OLED1.12-2 (SH1107, 128x128) on the J3 ribbon, 4-wire SPI.
//!
//! Geometry is SSD1306-shaped: 16 pages of 128 columns, one byte per page column
//! holding 8 vertically stacked pixels with D0 at the top. Bytes shift out MSB
//! first on the rising edge of SCLK (SPI mode 0) and DC (the SH1107's A0) is
//! sampled on every eighth clock, so it can be toggled with CS held low.
//!
//! The panel's own DC-DC stays off (`0xAD 0x8A`): VPP is external, from U2's 12 V
//! boost. Bring VPP up before enabling the display, and drop the display before
//! VPP on the way down.

use embassy_nrf::gpio::Output;
use embassy_nrf::spim::Spim;
use embassy_time::Timer;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;

pub const WIDTH: u32 = 128;
pub const HEIGHT: u32 = 128;
const COLS: usize = WIDTH as usize;
const PAGES: usize = HEIGHT as usize / 8;

/// Vendor init sequence (ER-OLED1.12-2 4-wire SPI sample), display left off.
const INIT: &[u8] = &[
    0xAE, // display off
    0xD5, 0x50, // clock divide / frame frequency ~104 Hz
    0x20, // page addressing mode
    0x81, 0x4F, // contrast
    0xAD, 0x8A, // DC-DC off: VPP supplied externally
    0xC0, // COM scan direction, normal
    0xA0, // segment remap, normal
    0xDC, 0x00, // display start line
    0xD3, 0x00, // display offset
    0xD9, 0x22, // pre-charge / discharge period
    0xDB, 0x35, // VCOMH deselect level
    0xA8, 0x7F, // multiplex ratio = 128
    0xA4, // follow RAM contents
    0xA6, // normal, not inverted
];

pub struct Display<'d> {
    spi: Spim<'d>,
    cs: Output<'d>,
    dc: Output<'d>,
    res: Output<'d>,
    fb: [u8; COLS * PAGES],
    contrast: u8,
    flipped: bool,
}

impl<'d> Display<'d> {
    pub fn new(spi: Spim<'d>, cs: Output<'d>, dc: Output<'d>, res: Output<'d>) -> Self {
        Self {
            spi,
            cs,
            dc,
            res,
            fb: [0; COLS * PAGES],
            contrast: 0x4F,
            flipped: false,
        }
    }

    /// Hardware reset, then the vendor init. Leaves the display off and the RAM
    /// cleared, ready for VPP to come up.
    pub async fn init(&mut self) {
        self.res.set_low();
        Timer::after_millis(10).await;
        self.res.set_high();
        Timer::after_millis(50).await;

        self.cmds(INIT).await;
        self.fb.fill(0);
        self.flush().await;
    }

    pub async fn on(&mut self) {
        self.cmds(&[0xAF]).await;
    }

    pub async fn off(&mut self) {
        self.cmds(&[0xAE]).await;
    }

    pub fn contrast(&self) -> u8 {
        self.contrast
    }

    pub async fn set_contrast(&mut self, contrast: u8) {
        self.contrast = contrast;
        self.cmds(&[0x81, contrast]).await;
    }

    pub fn flipped(&self) -> bool {
        self.flipped
    }

    /// Rotate the panel 180 degrees, for when the puck is assembled the other way
    /// up. Only affects what the controller does with RAM, not the framebuffer.
    pub async fn set_flipped(&mut self, flipped: bool) {
        self.flipped = flipped;
        if flipped {
            self.cmds(&[0xC8, 0xA1]).await;
        } else {
            self.cmds(&[0xC0, 0xA0]).await;
        }
    }

    pub fn clear(&mut self) {
        self.fb.fill(0);
    }

    /// Writes one logical pixel, rotating it into the panel's frame.
    ///
    /// This panel's RAM sits 90 degrees to the glass: an SH1107 page runs along
    /// what you see as the horizontal axis. The controller can't help - segment
    /// remap and COM direction only get you 180 - so the rotation happens here,
    /// on the way into the framebuffer, and everything above this line gets to
    /// think in ordinary screen coordinates.
    ///
    /// Logical (x, y) lands at panel column `y`, row `127 - x`. The `127 - x` is
    /// the part that matters: a bare transpose (column `y`, row `x`) rotates *and*
    /// mirrors, which reads as "nearly right, but backwards".
    pub fn set_pixel(&mut self, x: usize, y: usize, on: bool) {
        if x >= COLS || y >= HEIGHT as usize {
            return;
        }
        let (column, row) = (y, HEIGHT as usize - 1 - x);
        let byte = &mut self.fb[(row / 8) * COLS + column];
        let mask = 1 << (row % 8);
        if on {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }

    /// Push the whole framebuffer. ~4 ms at 4 MHz, with an await between pages so
    /// input polling still gets its 1 kHz slot.
    pub async fn flush(&mut self) {
        for page in 0..PAGES {
            self.cs.set_low();
            self.dc.set_low();
            // page address, then column 0 as low nibble + high nibble
            let _ = self.spi.write(&[0xB0 | page as u8, 0x00, 0x10]).await;
            self.dc.set_high();
            let _ = self
                .spi
                .write(&self.fb[page * COLS..(page + 1) * COLS])
                .await;
            self.cs.set_high();
        }
    }

    async fn cmds(&mut self, bytes: &[u8]) {
        self.cs.set_low();
        self.dc.set_low();
        let _ = self.spi.write(bytes).await;
        self.cs.set_high();
    }
}

impl OriginDimensions for Display<'_> {
    fn size(&self) -> Size {
        Size::new(WIDTH, HEIGHT)
    }
}

impl DrawTarget for Display<'_> {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, colour) in pixels {
            if point.x >= 0 && point.y >= 0 {
                self.set_pixel(point.x as usize, point.y as usize, colour.is_on());
            }
        }
        Ok(())
    }
}

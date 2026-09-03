use std::{env, fs, path::PathBuf};

// Flash map of a nice!nano v2 as it ships (Adafruit nRF52840 UF2 bootloader):
//
//   0x000000..0x001000   Nordic MBR
//   0x001000..0x026000   SoftDevice S140 slot (we don't use BLE; left untouched)
//   0x026000..0x0F4000   application       <- us
//   0x0F4000..0x100000   bootloader + MBR params + bootloader settings
//
// The bootloader starts the app at 0x026000 and asks the MBR to forward interrupts
// there, which is the same slot ZMK uses, so this is the layout that "just works"
// over USB with no debugger. Building with --features app-offset-1000 moves the app
// down over the SoftDevice slot, for bootloaders built without a SoftDevice.
fn main() {
    let origin: u32 = if env::var_os("CARGO_FEATURE_APP_OFFSET_1000").is_some() {
        0x0000_1000
    } else {
        0x0002_6000
    };
    const BOOTLOADER_START: u32 = 0x000F_4000;

    let memory_x = format!(
        "MEMORY\n\
         {{\n\
        \x20 FLASH : ORIGIN = 0x{origin:08X}, LENGTH = 0x{len:X}\n\
        \x20 RAM   : ORIGIN = 0x20000000, LENGTH = 256K\n\
         }}\n",
        len = BOOTLOADER_START - origin,
    );

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("memory.x"), memory_x).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=build.rs");
}

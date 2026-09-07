# pico2joy bring-up firmware

Bring-up firmware for the assembled puck: **no battery, no BLE**. It exists to
answer "did I solder it right?" — it blinks the nice!nano's LED, drives the
SH1107 ribbon OLED, and streams every encoder detent and button edge over USB
serial while mirroring the same state on the screen.

Rust + [Embassy] on the nice!nano v2 (nRF52840), flashed as a UF2 over the stock
bootloader — no debugger or SWD pads needed.

[Embassy]: https://embassy.dev

## Build and flash

```sh
cargo run --release              # build -> UF2 -> reboot the board -> flash it
MONITOR=1 cargo run --release    # ...and attach picocom when it comes back
```

`cargo run` goes through `flash.sh` (wired up as the cargo `runner`). It packages
the ELF as a UF2, asks the running firmware to reboot into UF2 mode — via its `b`
command, falling back to the 1200-baud touch — waits for the drive, copies, and
waits for the app's serial port to reappear. `udisksctl` is used to mount the
drive if your desktop doesn't automount it.

A board that has never been flashed can't reboot itself, so the first time
(or any time you see "board is in the bootloader's serial-only mode"):

1. Plug in USB-C.
2. **Double-tap** the reset button on the nice!nano. A drive named `NICENANO` (or
   `FTHR840BOOT`) appears — a single tap only gets you the bootloader's serial
   port, no drive.
3. `cargo run --release`, or drag `out/pico2joy-bringup.uf2` onto the drive.

The script waits 20 s for the drive, so you can start it and then double-tap;
`WAIT=40` if you want longer. Build without flashing:

```sh
./build.sh                       # -> out/pico2joy-bringup.uf2 (BLE, the default)
./build.sh --no-ble              # -> out/pico2joy-bringup-noble.uf2
./build.sh --offset-1000         # the alternate flash layout, see below
```

The first boot after flashing writes `UICR.NFCPINS` (to free P0.09/P0.10 from the
NFC block, where encoder A/B live) and resets itself once. If that reset lands in
the bootloader's double-tap window, the board comes up in the bootloader instead —
just tap reset once and it runs. This happens at most once per chip.

## Use

Open the app's serial port — a *second* `/dev/ttyACM*` distinct from the
bootloader's, `1209:0001 softek pico2joy bring-up`:

```sh
picocom -b 115200 /dev/ttyACM1     # baud is ignored, it's USB CDC
```

Opening the port prints a banner; then events stream as you use the puck.
Set the port raw first, as picocom and `pico2joy.py` do: Linux hands every
freshly enumerated ttyACM over cooked, with echo on, and a tty with echo on
sends everything the puck prints straight back into its command parser — the
banner alone spells `p`, `i`, `2` and then `b`, a reboot into the bootloader.
The firmware defends itself (`ECHO_PROBE` in `main.rs`): it writes nothing
until DTR is up and the opener has had 100 ms to go raw, and if its own
greeting comes back it ignores single keys until the port is reopened, `#`
lines excepted. So a bare `cat /dev/ttyACM0` gets a warning line rather than a
puck stuck on the `FLASHING` screen.

```
[   12.345] BTN1 down
[   12.501] BTN1 up
[   13.002] ENC cw  detents=1
[   13.410] ENC_SW down
```

Single-key commands:

| key   | does                                                                |
|-------|---------------------------------------------------------------------|
| `?`   | help                                                                |
| `p`   | inputs, detents, 12V_EN, battery, link state                        |
| `m`   | open/close the on-screen menu (handy without hands on the puck)     |
| `d`   | next view: cube / gantry / now playing / quota / orientation pattern |
| `i`   | re-init the display (reset + full init sequence)                    |
| `f`   | flip the panel 180 degrees (COM/segment remap)                      |
| `+`/`-` | contrast, in steps of 0x10 from the vendor default 0x4F           |
| `e`   | toggle the 12 V panel rail — panel off first on the way down        |
| `w`   | toggle the radio (advertising, or starting the BLE link)            |
| `v`   | verbose: log every encoder quadrature transition, not just detents  |
| `r`   | quota screen: ask the bridge to check the vendors now               |
| `l`   | LED: dark / dim heartbeat / on                                      |
| `b`   | reboot into the UF2 bootloader                                      |
| `#…`  | the machine channel — a line for the gantry bridge, not a keystroke |

## The control model

The device is a 3-axis jog controller, so that is the primitive everything else
is a view of: three jog counters plus a selected axis (`AXIS_COUNTS`, `AXIS` in
`main.rs`). **BTN1/2/3 select the axis, the knob jogs the selected one.**

Button-to-axis mapping lives in one line, `BUTTON_AXIS`, currently `Z, X, Y` in
button order; a build that wants `X, Y, Z` changes only that. The console can
drive the same model without hands on the puck: `1`/`2`/`3` select, `,`/`.` jog.

The **cube view** (second in the `d` cycle) renders those counters as a rotating
wireframe cube, with the three counts along the bottom. Hidden-line removal on a
convex solid is just backface culling: an edge is drawn exactly when at least one
of the two faces meeting at it faces the camera, so silhouette edges appear once
and the three edges meeting at the far corner never appear at all. It uses f32
and `libm` rather than fixed point, because the Cortex-M4F has a single-precision
FPU.

The cube's buttons are **momentary**, unlike everywhere else: hold BTN1/2/3 and
the knob jogs that axis and spins the cube about it, and with nothing held the
knob zooms. The knob applies **torque, not position** — a detent adds angular
momentum (about 0.52 rad/s), light drag bleeds it off with a ~5 s half-life, so a
flick sets it coasting and a counter-flick stops it.

The **gantry view** (third) is the real machine rather than a model of it — see
below.

## On the puck itself

- **Hold the knob in and turn it to change app.** One gesture reaches all seven
  screens from any of them, in either direction — which is more than `d` can do,
  and it costs no button, because a press was the only thing the knob did.
  The press is decided on release: turn in between and it was a switch, let go
  without turning and it was the menu.
- **Press the knob** to open the menu, **turn** to move, **BTN1/2/3** to select,
  **press the knob again** to leave — it's the same gesture in and out. Rows: `ble`, `12V rail`, `led`, `screen`, `jog step`,
  `home all`, `battery`, `exit` — each showing its current value on the right.
- The title bar says **which screen you are on**, the radio state, and the cell.
  It used to say `pico2joy` and carry a `12V` flag; neither earned its pixels —
  you can see it's the puck, and the rail flag was permanently lit (Q1 is flipped
  on this revision, so the rail can't be gated — and a board whose VPP *is* down
  has a dark panel nobody is reading a flag on). The rail keeps its menu row and
  its line in `p`, which is where a diagnostic belongs.
- Five screens, and every one of them is *about* something. The live input view
  and the all-pixels-on screen used to bracket them; both were bring-up
  instruments rather than things to look at. The pips told you a switch was
  wired, which `p` still reports and every other screen now proves by working,
  and lighting every pixel loaded VPP hard enough to find a weak boost — on a
  board whose rail can't be gated anyway. The orientation pattern stays, because
  a panel mounted the wrong way round is still worth one keystroke.
- **BTN1/2/3 mean whatever the screen is about**: the axis on the gantry, the
  transport on now-playing, the subscription on the quota screen. The knob is
  the same - jog, volume, or which account the gauge is showing.

## Driving a real printer

`tools/pico2joy.py` is the one host-side program: it finds the puck on USB or
BLE and speaks the same line protocol either way.

It serves several apps over that one link. `relay` owns the link and follows
the puck's screen - jog the gantry, turn the knob to the now-playing view and
the same knob is volume - because the machine channel is multiplexed by message
type and the puck announces its view with `#view`. `gantry`, `spotify` and
`quota` are that same relay pinned to one app, for a host that only has the one
job (the printer has no browser; a workstation may have no printer).

```
tools/pico2joy.py scan            # what can see the puck right now
tools/pico2joy.py monitor         # console passthrough
tools/pico2joy.py relay           # every app, following the puck's view
tools/pico2joy.py gantry          # just the gantry (relay pinned to one app)
tools/pico2joy.py quota           # just the Claude/Codex rate-limit windows
tools/pico2joy.py flash out/pico2joy-bringup.uf2   # reflash, no reset button
tools/pico2joy.py reset uf2|serial|ota             # into a bootloader mode
```

Standard library only for everything over USB — the printer's system Python has
no pyserial, and a bring-up tool that needs a venv is a tool that doesn't get
run. BLE needs `bleak`, declared inline, so `uv run tools/pico2joy.py …` fetches
it and nothing else has to be installed.

Nobody here is a server. The program is a *client* of everything: it opens the
puck's port (or connects to it as a BLE central) and makes HTTP requests to
Moonraker, which is the only real server in the picture. So it runs wherever the
puck is attached — the printer host, or a workstation — with no listening
socket and no discovery. The one hard rule is that a serial port and a BLE
connection each have exactly one owner, so one copy of it owns the puck at a
time.

Run it on the printer, where Moonraker is localhost and trusted:

```
scp tools/pico2joy.py sovol@spi-xi:~/pico2joy/     # plug the puck in there
ssh sovol@spi-xi 'python3 ~/pico2joy/pico2joy.py gantry'   # localhost Moonraker
```

Moonraker only trusts requests from its `trusted_clients`, which is why the
gantry bridge is happiest on the printer. To run it from a workstation instead, pass
`--api-key`, add that host to `trusted_clients`, or let the tool tunnel the port
so the request arrives from localhost:

```
ssh -N -L 17125:127.0.0.1:7125 sovol@spi-xi        # by hand, or:
tools/pico2joy.py gantry --tunnel sovol@spi-xi     # does the forwarding for you
```

The puck is a **view of the machine, never a second copy of it**. Positions and
limits come from the printer; a detent produces a jog *request*, and the number
on screen doesn't move until Klipper reports that the head did. A jog that gets
refused — unhomed axis, mid-print, outside the travel — simply doesn't happen,
instead of leaving the puck lying about where the nozzle is.

Both directions share the console port. `#` opens a line on the machine channel;
every other byte is still a single-key command. Values are integer micrometres,
so neither end rounds twice:

| direction | line | meaning |
|-----------|------|---------|
| host → puck | `#s <x> <y> <z> <homed-bits> <state>` | toolhead state, ~8 Hz |
| host → puck | `#l <xmin> <xmax> <ymin> <ymax> <zmin> <zmax>` | travel limits |
| host → puck | `#?` | identify |
| puck → host | `#j <axis> <delta-um>` | jog request |
| puck → host | `#c home` / `#c home_z` / `#c stop` | named command |
| puck → host | `#v 1` | identify reply |

No `#s` for 1.2 s and the screen says `offline` rather than showing a stale pose.
Jogs are coalesced one frame at a time, so spinning the knob fast becomes one
move rather than a queue of them, and they go out as a saved-state relative move:

```
SAVE_GCODE_STATE NAME=pico2joy_jog
G91
G1 X0.100 F6000
RESTORE_GCODE_STATE NAME=pico2joy_jog
```

On the gantry screen **BTN1/2/3 are X/Y/Z** (not the cube's `BUTTON_AXIS` order),
the knob jogs the selected axis by the step size from the menu (0.01 / 0.1 / 1 /
10 mm), and `home all` in the menu sends `#c home`. Each axis shows its position,
its travel as a bar with the head's place in it, and dashes if it isn't homed.

## Now playing (Spotify, or anything)

`tools/pico2joy.py spotify` turns the puck into a transport for whatever the
host is playing, and relays the album art back to the screen.

```
tools/pico2joy.py spotify              # over USB or BLE, whichever the puck is on
uv run tools/pico2joy.py spotify       # album art needs pillow, fetched by uv
```

It reads the **active MPRIS player** through `playerctl` (following `playerctld`,
so it tracks whatever you last touched) rather than Spotify's Web API - no OAuth,
no app registration, nothing to break when the API changes, and it works against
the Spotify *web player* in your browser exactly as it works against a desktop
app or, for that matter, a YouTube tab. Volume is the host's own output level via
`wpctl` (or `pactl`), because "louder" means the machine the music comes out of.

On the **now-playing screen** (fourth in the `d` cycle) the cover fills the frame
with a dark footer carrying title, artist and a volume bar. **BTN1/2/3 are
previous / play-pause / next**, and the **knob is volume**, five percent a detent.
With no bridge running the screen says so instead of showing a stale track.

The wire format is the same machine channel the gantry uses (`src/media.rs`):
`#ns`/`#nt`/`#na` carry play state, title and artist; the 128x128 one-bit cover
goes out as `#ab`, a run of `#a <seq> <hex>` rows, then `#ae`; and the puck sends
`#m p`/`#m n`/`#m b`/`#m v <steps>` back. One owner per link still holds, so the
puck drives the gantry or the player, not both at once.

## Subscription quotas

`tools/pico2joy.py quota` puts the state of your Claude and Codex plans on the
puck: how much of each short window is spent, how much of the week, how long
until the short one starts again, and how many sessions are running.

```
tools/pico2joy.py quota                       # ~/.claude and ~/.codex
tools/pico2joy.py quota --claude ~/.claude=me \
                        --claude work-box:~/.claude=work \
                        --codex ~/.codex
```

It reads the credentials each vendor's own CLI already keeps on disk and asks
the same usage endpoints those CLIs ask — `/api/oauth/usage` for Claude, the
ChatGPT backend's `wham/usage` for Codex. Nothing is counted here and nothing is
reconstructed from transcripts: a local token tally would be a second, wronger
copy of a number the vendor is authoritative for, which is the same reason the
gantry screen waits for Klipper rather than integrating its own jogs.

**Read-only, deliberately.** The access tokens in those directories are the
CLIs' to refresh; using the refresh token here could rotate it out from under a
running session and log you out of your own editor to draw a bar on a knob. So
an expired token shows `log in again` and waits for its CLI to renew it, which
happens the next time you use it.

The usage endpoints rate-limit, and a 429 is easy to earn and slow to clear — so
the default is a poll every five minutes, a 429 holds the last good reading
rather than blanking the screen, and the interval doubles on each failure until
it succeeds. Anthropic's `Retry-After` is honoured where it says anything useful
(it currently sends `0`). Five minutes of staleness costs nothing when holding a
button re-checks on demand, and `--refresh` moves it.

### More than one plan, and plans on other machines

The default is whatever is logged in on this machine, which for one Claude
subscription and one Codex is the whole configuration. **A second Claude account
needs a second config directory**, because Claude Code keeps one login per
directory and `/login` replaces it:

```sh
CLAUDE_CONFIG_DIR=~/.claude-work claude       # log the other account in, once
tools/pico2joy.py quota --claude ~/.claude=me --claude ~/.claude-work=work
```

If the other account lives on **another machine**, give it as `HOST:DIR` — scp's
spelling, and unambiguous because a local config directory has no colon:

```sh
tools/pico2joy.py quota --claude ~/.claude=me \
                        --claude design-desktop:~/.claude=work
```

That ships *this file* to the far end over ssh (`ssh host python3 - probe claude
~/.claude`), which reads the credentials, makes its own request, and prints one
JSON line back. Nothing is installed there and nothing is left behind — the same
bargain the rest of this tool makes. The token never crosses the network either:
`cat`ing the credentials home and calling the API from here would work, but there
is no reason to move a live OAuth token when the machine holding it can send back
six numbers instead. `tools/pico2joy.py probe claude ~/.claude` runs the same
reading locally, which is a good way to see exactly what the bridge sees.

`--claude`/`--codex` take `[HOST:]DIR` or `[HOST:]DIR=LABEL`; the label is what
shows on the row, and thirteen characters of it fit. Without one it takes the
account's display name or the local part of its email. Three accounts is the
limit — three buttons, three rows.

### On the screen

One block per account, all of them drawn all of the time. A name line carrying
the vendor, the sessions badge and the countdown to the short window's reset,
then a fat bar for that window and a thinner one for the week:

```
quota                     ble  88%
claude Dylan          [9]  1h47m
 5h ██████████████░░░░░░░░░  64%
 7d ███████░░░░░░░░░░░░░░░░  29%
codex engineering          1h12m
 5h ███████████████████████ 100%
 7d ██░░░░░░░░░░░░░░░░░░░░░   5%
```

Each bar carries its own label and number, drawn twice and clipped either side
of the fill boundary — lit over the empty part, dark over the filled part — so
the text stays legible at every value instead of vanishing into the bar around
half way. That is what buys the room for six bars on a 128-pixel screen without
a caption line above each one.

There is **nothing to select and nothing to page through**, which is the point:
the question this screen answers is "where do I stand", and an answer you have to
press a button to finish reading is a worse answer. **Holding a button** asks the
bridge to look again — the answer to "has it reset yet?" — and that is the only
control here. From the console that is `r`.

**Window names come from the bridge**, not from a constant in the firmware: `5h`
today, `6h` if a vendor moves, and **`7d/Fable`** when the weekly cap that
actually binds is one scoped to a model. Claude reports both an all-models weekly
figure and per-model ones, and the scoped one runs out first — a week that is 29%
gone overall can be 41% gone on the model you are using. The bar shows whichever
is higher and its label says which, because the useful number is the one you will
hit, not the flattering one.

**The badge is running sessions.** Neither vendor reports this, so it is counted
where the answer exists: the machine that account is logged in on, over the same
ssh hop as the reading. Top-level processes only — a session spawns helpers with
the same name, so counting every `claude` in `ps` says nine when three windows
are open — attributed by each process's `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`), so
two accounts on one machine are counted apart. It is drawn only when there are
any, so an idle plan's line stays clean and a busy one announces itself.

The countdown is the one number computed on the puck. The bridge sends the
seconds left at the moment it asked and the puck turns that into a deadline on
its own clock, so the minutes tick between polls instead of freezing; every poll
re-syncs it. The host stays the authority and the puck only interpolates — the
same bargain the gantry makes with the toolhead position.

The wire format is the same machine channel (`src/quota.rs`): `#qz <count>` is
the heartbeat, `#qa` names a slot and its two windows, `#qu` carries the two
windows as percent and seconds-to-reset plus the session count and state, and the
puck sends `#q r` back. The vendors are polled on a worker thread, because a
usage request is a round trip to the internet and the loop that does it is also
the loop that reads the knob.

## Staying connected over BLE

The relay is happiest on the cable, but it does not need one. `--link ble` puts
it on the radio and keeps it there:

```
uv run tools/pico2joy.py --link ble relay --claude ~/.claude=me --codex ~/.codex
```

**This is the default build**, so `./build.sh`, `cargo run` and
`tools/pico2joy.py flash` all put it on the puck. `./build.sh --no-ble` gives the
broadcast-only firmware instead: a quarter of the flash, no radio stack to go
wrong, and no way to accept a connection — a useful thing to fall back to when
you are bisecting something, and useless for anything in this section.

**The link puts itself back together.** `BleLink`'s thread is not there to
connect once; it scans, connects, serves until the link drops, and starts again,
for as long as the program runs. Nothing above it knows. A forced disconnect
costs about four seconds:

```
14:37:32 ble: link down
14:37:36 ble: connected to C5:96:2F:91:CE:B8
14:37:37 relay: link back, resending
```

That last line is the part that matters. A puck that reconnected is a puck that
was told the toolhead position, the album art and three subscriptions before the
gap and remembers none of it, so the link counts its connections and the relay
resends everything on every new one — not just the first. Lines queued while the
link is down are **dropped**, not buffered: a jog delivered thirty seconds late
is worse than a jog that never happened.

`--link auto` still prefers the cable, and falls back to the radio when the cable
goes away — so unplugging USB mid-session moves the relay onto BLE rather than
ending it.

### Making Linux hold the link

Two things on the host are worth checking, because neither is our code:

**USB autosuspend on the Bluetooth controller.** This is the big one. `btusb`
ships with `enable_autosuspend=Y`, and on the MediaTek combo cards (`0e8d:*`, the
MT7921/7922 family) a suspended controller drops LE links and is slow to come
back. Check what yours has done:

```sh
cat /sys/class/bluetooth/hci0/device/../power/control          # "auto" is the problem
cat /sys/class/bluetooth/hci0/device/../power/runtime_suspended_time   # in ms
```

Pin it on, for this boot and for every boot:

```sh
echo on | sudo tee /sys/class/bluetooth/hci0/device/../power/control

# /etc/udev/rules.d/81-bt-no-autosuspend.rules — use your own vendor:product
ACTION=="add", SUBSYSTEM=="usb", ATTR{idVendor}=="0e8d", ATTR{power/control}="on"
```

**BlueZ's supervision timeout.** The central picks the connection parameters and
the puck has no say in them (see `src/ble.rs` for why — the GAP preferred-
parameters characteristic is a TODO in trouble-host 0.8). BlueZ's default tears
the link down over one missed window, which is easy to earn from a device you
pick up and carry. Four seconds of tolerance in `/etc/bluetooth/main.conf`:

```ini
[LE]
MinConnectionInterval=24
MaxConnectionInterval=48
ConnectionLatency=0
ConnectionSupervisionTimeout=400
```

(Units are 1.25 ms for the intervals and 10 ms for the timeout, so that is
30–60 ms and 4 s.) Then `systemctl restart bluetooth`. This is an optimisation
rather than a fix — the link recovers either way — but it turns a four-second
gap into no gap.

One more, if a connection succeeds and then behaves as though the puck has no
services: BlueZ caches a GATT database per address, and the puck's bootloader
advertises from an address one digit from the application's. The bridge notices
an empty service list and drops the cached record itself before retrying, so
this should heal on its own; `bluetoothctl remove <address>` is the manual
version, and `bluez-utils` is worth having installed for exactly that kind of
poking.

## Battery

The nice!nano v2 senses the cell through **VDDH**, not a divider pin, so this
samples VDDH/5 on the SAADC against the internal 0.6 V reference at gain 1/6:

    millivolts = raw * 18000 / 4096

Percent comes from a piecewise LiPo curve (`state::percent_from_mv`), because a
straight voltage-to-percent line is useless on a cell that sits at 3.8 V for
most of its life.

Two things to expect. On USB with no cell it reads about **4.54 V** — VBUS less
the input diode drop — and therefore claims 100%. And while charging it reads
the charger's output rather than the cell, so it will read full before the cell
is. Neither is worth correcting until a battery is actually on J2.

## What this actually tests

- nice!nano seated in its sockets, USB and the on-module LED alive.
- All three buttons, the encoder push, and both encoder phases. A cold joint on
  one encoder phase shows up as `step=0` noise under `v` instead of clean detents.
- The switch pull-ups at boot: `p` flags anything already reading low (a held
  button, or a short to GND on that net).
- **The SH1107 panel** on the J3 ribbon: the boot splash is an orientation test
  pattern (border, both diagonals, a solid block and `TL` in the top-left), then
  it switches to the cube, which is a live view of the encoder and the buttons
  in its own right: hold a button and turn, and the cube spins.
- **U2, the 12 V boost**, which the panel needs as external VPP (11.5–12.5 V, the
  panel's own DC-DC stays off via `0xAD 0x8A`). Brought up after the controller
  is initialised and dropped before it, and `e` toggles it.

Untested: the LiPo path (J2, `VBAT`).

### If the screen looks wrong

- **Mirrored or upside down** — press `f`. That flips COM scan and segment remap
  (`0xC8 0xA1` vs `0xC0 0xA0`), which is the assembly-orientation case.
- **Rotated 90 degrees** — already handled. This panel's RAM sits 90 degrees to
  the glass (an SH1107 page runs along the horizontal), and the controller only
  offers 180, so `Display::set_pixel` rotates on the way into the framebuffer:
  logical `(x, y)` lands at panel column `y`, row `127 - x`. If a future panel
  wants the plain mapping, that one line is the only thing to change — and note
  a bare transpose (column `y`, row `x`) rotates *and* mirrors.
- **Nothing at all, or faint** — check TP4 for ~12 V (`e`), then try `+` a few
  times. VPP below ~11.5 V reads as a dim or blank panel rather than an error.
- **Garbled but alive** — drop `spim::Frequency::M4` in `main.rs` to `M2`.

## Pinout

| Signal    | nRF52840 | pad | Notes                                       |
|-----------|----------|-----|---------------------------------------------|
| LED       | P0.15    |  -  | on-module blue LED, active high             |
| VCC_EN    | P0.13    |  -  | nice!nano load switch: high = 3V3 pad live  |
| ENC_A     | P0.09    | 24  | NFC pin, freed via `nfc-pins-as-gpio`       |
| ENC_B     | P0.10    | 23  | likewise                                    |
| ENC_SW    | P1.11    | 22  |                                             |
| BTN1      | P0.31    | 17  |                                             |
| BTN2      | P0.02    | 19  |                                             |
| BTN3      | P1.15    | 20  |                                             |
| OLED_DC   | P0.06    |  1  | SH1107, 4-wire SPI over the J3 ribbon       |
| OLED_SCLK | P0.08    |  2  |                                             |
| OLED_SDI  | P0.17    |  5  |                                             |
| OLED_RES  | P0.22    |  7  | 10k pull-up R2: boots out of reset          |
| OLED_CS   | P0.24    |  8  | 10k pull-up R6: boots deselected            |
| 12V_EN    | P1.06    | 12  | Q1 gate, **active low** — see below         |

Taken from `board_pico2knob/pico2-knob.kicad_sch` (the checked-in
`bom_netlist.xml` is older than the ribbon-OLED rework — don't use it).

### 12V_EN is active low, and on this revision "off" isn't off

R8 pulls Q1's gate up to +BATT, so the rail is off with the gate high and on with
it pulled down. 3V3 is *not* a clean off — it leaves Vgs near -0.9 V, inside the
DMG3415U's threshold band — so the firmware drives the pin **low to enable** and
leaves it **disconnected (high-Z) to disable**, never high.

That said, Q1 looks flipped, so the rail can't actually be gated on this build:

- the schematic has Q1's **drain** on +BATT and its **source** on U2's Vin;
- the PCB footprint renames the SOT-23 pads `G`/`S`/`D` onto pads 1/2/3, which is
  exactly the DMG3415U's real pinout — so the physical drain is the +BATT one.

A P-channel high-side switch needs its *source* on the supply. Wired this way the
body diode (drain → source) conducts +BATT into U2's Vin whenever a cell or USB is
present, so U2 free-runs and the 12 V rail is always live, burning its quiescent
current on battery. `e` then only removes the diode drop rather than switching
anything.

To confirm on the bench: meter TP4 right after boot, before touching `e`. ~12 V
means the diode path is feeding U2. The rev-2 fix is swapping Q1's source and
drain (or dropping the FET and using U2's own shutdown).

## Flash layout / the two UF2s

The stock bootloader starts the app at **0x26000**, above the (unused) SoftDevice
slot, and asks the MBR to forward interrupts there — the same slot ZMK uses. That
is `out/pico2joy-bringup.uf2`, and it's the one to use. It carries the BLE stack;
`out/pico2joy-bringup-noble.uf2` (`./build.sh --no-ble`) is the same firmware
without it, and links at the same address.

If the bootloader on your board was built without a SoftDevice it will expect the
app at 0x1000 instead, and copying the default UF2 will appear to do nothing (the
drive just remounts). `out/pico2joy-bringup-0x1000.uf2` (`./build.sh
--offset-1000`) is linked for that case. `INFO_UF2.TXT` on the bootloader drive
names the bootloader build if you want to check first.

Neither variant touches the bootloader itself (0xF4000+), so double-tap reset
always gets you back.

## Next

- Battery: read VDDH via SAADC for a rough LiPo percentage (the nice!nano v2
  senses the cell through VDDH, not a divider pin).
- Sleep: the panel and the 12 V rail dominate the power budget, so idle wants
  display-off plus VPP down — which needs the Q1 fix above to actually work.
- BLE HID over `nrf-softdevice` or `trouble`, which is where the app at 0x26000
  starts mattering: a SoftDevice build has to move up to 0x27000.

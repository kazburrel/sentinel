# Project Status — full history for Codex context (rewritten, major update)

## Goal

Battery-powered motion-triggered camera on a Freenove ESP32-S3-WROOM CAM board:
AM312 PIR sensor detects motion → ESP32-S3 captures a JPEG from the onboard camera →
image is sent to a server (Ollama locally in dev, RunPod serverless in prod) for AI
vision processing → phone gets notified. Firmware is Rust (`esp-hal`, no_std, Embassy
async runtime via `esp-rtos`).

## Hardware

- Freenove ESP32-S3-WROOM CAM board, native USB-Serial/JTAG, no external UART bridge
- **The camera sensor is an OV3660, not an OV2640** (this was wrong for most of the
  project — see "Major discovery" below). Confirmed by physically reading the label
  printed on the camera module's ribbon cable.
- AM312 PIR sensor, wired: `VCC→3V3`, `OUT→GPIO21`, `GND→GND`
- Onboard WS2812 addressable RGB LED on GPIO48
- Camera pin map (confirmed correct for this board): XCLK/MCLK=15, PCLK=13, VSYNC=6,
  HREF=7, D0-D7=11,9,8,10,12,18,17,16, SCCB SDA=4, SCCB SCL=5
- Dev machine: MacBook Pro M1 Max, macOS Darwin 25.5.0, Apple Silicon

## Milestones completed and verified on real hardware

**Milestone 1 — first flash/serial loop.** Verified board detection, ESP32-S3 v0.2/8MB
flash identified via `espflash board-info`, repeating "alive" message over
`esp_println`. Committed in `c7937c4`.

**Milestone 2 — PIR motion detection.** GPIO21 digital input, edge-triggered
(`motion detected`/`motion stopped`). Verified against real arm movement. Committed in
`a08771a`.

**Milestone 3 — WS2812 LED feedback.** GPIO48 driven via raw `esp_hal::rmt`/`PulseCode`
(hand-rolled bit timing after both `esp-hal-smartled` and `esp-hal-smartled2` proved
unusable). LED off at rest, dim red on motion, off after. Code organized into
`firmware/src/pir.rs`, `firmware/src/ws2812.rs`, `firmware/src/lib.rs`. Committed in
`305d75f` — **still the last commit; nothing camera-related is committed yet.**

## Milestone 4 — camera capture: MAJOR DISCOVERY, currently mid-Rust-port

This milestone went through a long, mostly-mistaken debugging saga, then a real
breakthrough, and is now in a fresh, well-understood-but-unsolved state. Read in order:

### What actually happened (the short version)

1. Assumed the sensor was OV2640 (per original hardware ID, which turned out wrong).
2. Tried the experimental crate `esp32s3-cam-async` → total silence, no serial output.
3. Tried `esp-hal`'s own official OV2640 example → also total silence.
4. Spent a very long time debugging this as a suspected hang: `probe-rs` hardware
   debugger sessions, register/backtrace analysis via `addr2line`, stack-overflow
   theories, RAM-pressure theories, clean incremental bisection back to the last known
   working commit. All of this is preserved in `FOR_CODEX.md` (UPDATE 1-8) for
   reference, but **the conclusion at the end of that investigation was wrong** — see
   next point.
5. **Breakthrough**: switched to a completely different toolchain (Arduino + the
   mature, official `esp32-camera` C library) to check whether the camera hardware
   itself worked at all, independent of our Rust code. It also appeared silent at
   first — until a test was written that repeated its result **forever in a loop**
   instead of printing once. That immediately revealed: **the camera was working
   correctly the entire time.** There was never a hang, in Rust or Arduino. Every
   earlier test only printed its result once, and that single print consistently
   landed in the same narrow window our serial reader misses right after a board
   reset/replug (the reader takes a moment to attach after the physical USB
   reconnect). The "hang" was 100% an artifact of test methodology, not a real
   firmware or hardware fault.
6. While getting a clean capture in Arduino, noticed the camera module's ribbon-cable
   label actually reads **"OV3660"**, not OV2640. This had been assumed wrong since
   the very start of camera work. Every prior register-init sequence (both the
   experimental crate and the official esp-hal example) was silently doing OV2640
   register writes to a different chip the whole time — which may also have
   contributed to earlier confusion, though the "hang" itself is now known to be
   unrelated to this (see point 5 — even Arduino's *correct*, auto-detecting OV3660
   driver looked silent at first for the same observability reason).
7. Got a real, valid JPEG photo (640×480, then pushed to full-sensor UXGA 1600×1200)
   out of the board via Arduino, confirmed by opening the reconstructed `.jpg` file.
   Iterated on transfer speed (raw binary instead of hex text, 921600 baud instead of
   115200 — ~16x faster) and turned it into an on-demand "capture now" trigger
   instead of a fixed timer, landing at ~3 seconds from trigger to a saved,
   viewable UXGA photo. A reusable script, `capture.sh`, automates this end to end
   (finds the port, triggers capture, extracts the JPEG from the raw serial stream,
   saves timestamped to `~/Desktop`, opens it).
8. Started porting this proven-working setup back to Rust (the Arduino work was
   always meant as a diagnostic detour, not a replacement — see next section).
   **This port is where we are now, and it has hit a new, real, not-yet-understood
   bug** (see "Current blocker" below) — genuinely different from the earlier "hang,"
   which is fully resolved/understood.

### The Rust OV3660 port (in progress)

New file `firmware/src/ov3660.rs`: a from-scratch OV3660 SCCB driver, ported from
Espressif's real `esp32-camera` library (`sensors/ov3660.c`,
`sensors/private_include/ov3660_settings.h`/`ov3660_regs.h`) — **not hand-transcribed**,
extracted programmatically with a Python script to avoid copy-transcription errors
(a real bug was caught and fixed this way: the first extraction pass incorrectly
included 16 commented-out register entries from the source file; a second pass that
strips `//` comments before parsing fixed it — final table is 199 register-writes,
verified against the source).

Contents:
- `OV3660_DEFAULT_REGS` (199 entries) + `OV3660_FMT_JPEG` (6 entries) — `(u16, u16)`
  register/value tables (OV3660 uses 16-bit SCCB register addresses, unlike OV2640's
  8-bit ones)
- `Ov3660::check_id()` — reads the real 16-bit product ID (registers `0x300A`/`0x300B`)
  and confirms it equals `0x3660`, rather than assuming the sensor type
- `Ov3660::init_jpeg(quality)` — soft reset, writes the default-regs table, writes the
  JPEG-format table, sets the quality register (`0x4407` = `COMPRESSION_CTRL07`)
- Uses I2C address `0x3C` (7-bit) — **note:** this was originally `0x30` (copied
  from the OV2640 assumption without re-verifying) and caused the first blocker below;
  now fixed and confirmed against Espressif's real source
  (`OV3660_SCCB_ADDR = 0x3C`, vs `OV2640_SCCB_ADDR = 0x30`, in
  `esp32-camera/driver/include/sensor.h`)
- `firmware/src/bin/camera_test.rs` was rewritten to use this new driver, keeping the
  exact same `esp_hal::lcd_cam::cam::Camera`/DMA setup as the earlier OV2640 attempt
  (that part was proven correct and was never the actual problem)
- Applied the methodology lesson directly: this firmware prints its status
  **repeatedly, forever**, not once — so if something actually goes wrong, it cannot
  silently look like a hang the way earlier one-shot-print tests did.

### Blocker #1 (SOLVED): wrong I2C address

`Ov3660::check_id()` failed every attempt with
`I2c(AcknowledgeCheckFailed(Address))` — a real, immediate error (not the old hang
observability issue), meaning the sensor wasn't acknowledging its address at all. Spent
a while ruling out pull-ups and bus frequency with source-level evidence (both
confirmed correct against `esp-hal-1.1.1/src/i2c/master/mod.rs`) and tried a
clock-before-I2C reordering theory — none of it was the cause.

**Root cause:** `OV3660_ADDRESS` was set to `0x30`, copied from the original OV2640
assumption without re-verifying for the actual sensor. Espressif's real
`esp32-camera/driver/include/sensor.h` defines `OV2640_SCCB_ADDR = 0x30` but
`OV3660_SCCB_ADDR = 0x3C` — different sensors, different default SCCB address. Fixed
by changing the constant to `0x3C`. **This resolved the NACK completely** — `check_id()`
now succeeds, chip ID confirms as a real OV3660, and `init_jpeg()` (the full 199+6
register write sequence) completes without any I2C error.

### Blocker #2a (SOLVED): DMA started too early

Original code called `camera.receive(dma_rx_buf)` *before* the I2C/SCCB setup, on the
theory that the physical XCLK signal might not start until `.receive()` began (this
theory turned out wrong). The real problem: a small 20KB DMA descriptor chain, started
that early, can exhaust itself during the ~1s+ of I2C setup (500ms wait + chip-ID
retries + 200+ register writes) *before we ever get around to reading from it* —
producing `transfer.is_done()` with no real EOF ever having been consumed. Fixed by
reordering: full I2C/SCCB setup (`check_id()` + `init_jpeg()`) now happens first,
`camera.receive()` is called last, immediately followed by reading. This matches the
official esp-hal example's ordering. **After this fix, warmup-frame skipping succeeds**
— no more "DMA warmup never completed."

### Blocker #2b (SOLVED): per-byte `print!()` was too slow to keep up with the DMA

Confirmed Codex's diagnosis exactly: the real-frame loop was doing `print!("{b:02X}")`
for every single byte *before* consuming it, while the warmup loop just consumed
immediately with no printing. That per-byte formatting/serial-write was far slower than
the incoming DMA data rate, so the 20KB ring buffer filled and finished before EOF ever
appeared. Removed the per-byte print entirely (just accumulate `frame_len` and consume
immediately) — **capture now completes successfully end to end**: the firmware's
forever-repeating status line reports `still alive: last capture was 5594570 bytes`.
The full pipeline (SCCB config → DMA receive → warmup discard → real frame → EOF) now
works in Rust.

**New concern surfaced by this success**: 5,594,570 bytes (~5.6MB) is suspiciously huge
for one JPEG frame — Arduino's UXGA captures were only ~100-300KB. This is more evidence
for the still-open `set_framesize()` gap: without it, the sensor may be outputting at
some much larger/less-compressed default rather than a sane resolution. Not yet fixed.

### Blocker #2c (SOLVED): frame size — needed `set_framesize()`'s compression-enable bit

Confirmed exactly right: `OV3660_FMT_JPEG` only selected YUV formatting, never actually
set register `0x3821`'s compression-enable bit (`0x20`), so the sensor was emitting
~5.6MB of uncompressed data. Ported a minimal, hardcoded VGA (640x480) version of
`set_framesize()`/`set_image_options()` (crop window, binning/scaling, JPEG-compression
enable, 20MHz-XCLK PLL settings — new const table `OV3660_FRAMESIZE_VGA` in
`ov3660.rs`, written right after `OV3660_FMT_JPEG` in `init_jpeg()`). **Frame size
dropped from 5,594,570 bytes to 12,288 bytes** — the right order of magnitude for a
real compressed VGA JPEG (Arduino's VGA captures were 14-32KB, so this is in-range,
maybe a touch small/aggressive but clearly real compression is happening now).

### DONE: verified the actual bytes are a valid, viewable JPEG — MILESTONE 4 COMPLETE

Implemented the buffered approach: a 64KB static buffer (`StaticCell`) that the fast
count-only DMA loop copies chunks into (no printing during the live loop), then dumps
as hex — repeated every 10s forever, not once, same reliability lesson as everywhere
else in this project. Captured, reconstructed with `xxd -r -p`, and verified:

```
JPEG image data, JFIF standard 1.01, ..., 640x480, components 3
```

**A real, valid, correct-resolution photo, captured entirely by the Rust firmware —
no Arduino/experimental-crate dependency needed.** This closes out the entire camera
capture investigation. The full chain of fixes, in order: wrong sensor assumed
(OV2640→OV3660) → wrong I2C address (`0x30`→`0x3C`) → DMA started before I2C setup
(reordered) → per-byte `print!()` too slow during capture (removed, buffered instead)
→ missing JPEG-compression-enable bit in `set_framesize()` (ported a minimal hardcoded
VGA version) → buffered capture + dump to get real bytes off-device. Every one of
these was found and fixed with actual evidence (source code, real error messages,
concrete byte counts), not guessing.

## Milestone 5 — reusable CameraHandle + PIR integration + local capture tooling

Everything listed as "next steps" in the previous update is now done and verified on
real hardware:

- **`firmware/src/camera.rs` refactored from a one-shot function into a stateful
  `CameraHandle` struct.** The old `capture_jpeg()` function took ownership of all
  camera peripherals *by value* and consumed them entirely within the call — meaning
  it could only ever be called once per program lifetime, which is incompatible with
  triggering a capture on every PIR motion event. Fixed by confirming (via
  `esp-hal-1.1.1/src/lcd_cam/cam.rs`) that `CameraTransfer::stop()` returns
  `(Camera<'d>, BUF::Final)` — i.e. hands the `Camera` back for reuse. `CameraHandle::new(...)`
  now does the one-time peripheral wiring + full OV3660 SCCB init (soft reset, ~200
  default registers, JPEG format, VGA framesize/compression-enable) exactly once;
  `CameraHandle::capture_jpeg(&mut self, buf)` can be called repeatedly afterwards,
  reusing the already-configured sensor and hardware each time (`camera: Option<Camera<'d>>`
  internally, `.take()`'d out and put back after each `.receive()`/`.stop()` cycle).
  This also means repeat captures are much faster than the first — no need to redo the
  ~200-register SCCB init on every motion event.
- **`firmware/src/bin/camera_test.rs`** updated to use `CameraHandle` (thin hardware
  test, unchanged in spirit).
- **`firmware/src/bin/main.rs` now does real PIR-triggered camera capture**: builds a
  `CameraHandle` once at boot alongside the existing PIR/LED setup, and on
  `MotionEdge::Detected` calls `camera.capture_jpeg(jpeg_buf)`, logging success (byte
  count) or failure. `main.rs` contains zero camera protocol details — it only
  orchestrates via the `CameraHandle` API, matching the existing `pir.rs`/`ws2812.rs`
  separation-of-concerns pattern. GPIO usage doesn't conflict: PIR/LED use
  GPIO21/GPIO48+RMT, camera uses LCD_CAM/DMA_CH0/I2C0 + GPIO4/5/6/7/8/9/10/11/12/13/15/16/17/18.
- **Verified on real hardware repeatedly on a single boot** (no reflash between
  triggers): 3 separate motion events each produced a fresh, valid photo (23171,
  22407, 23524, 22463, 21349 bytes seen across test runs) — confirms the
  `CameraHandle` reuse design actually works, not just compiles.
- **New `firmware/src/hexdump.rs` module**: `print_hex_dump(&[u8])`, a small
  serial-transport helper (hex-encode + `JPEG BEGIN`/`JPEG END` framing markers) shared
  by both `main.rs` and `camera_test.rs` — kept separate from `camera.rs` since framing
  bytes for serial debug output is a transport concern, not a camera concern.
- **New `scripts/capture_photo.sh` + `scripts/decode_capture.py`**: a local dev/test
  tool (not part of firmware) that listens to the board's serial port, waits for you to
  trigger the PIR sensor, polls for a completed `JPEG END` marker (rather than always
  sleeping a fixed window), decodes the hex dump into a real `.jpg`, and opens it
  automatically. Typical round-trip is ~5-10s (PIR debounce + 2 skipped warm-up frames
  + serial transfer at 115200 baud), not the naive 20s fixed wait it started as.

## Next steps (not yet started)

- **WiFi upload to the server.** This is the actual next project milestone per the
  stated goal above — right now a captured JPEG only ever sits in RAM and gets
  hex-dumped to serial for local debugging via `scripts/capture_photo.sh`. The real
  pipeline needs: connect to WiFi (`esp-radio`/`embassy-net`, both already in
  `Cargo.toml`), POST the captured JPEG bytes to the `server` crate (already scaffolded
  in the workspace, not yet implemented), get a response back, decide what to do with
  it (eventually: phone notification).
- `FOR_CODEX.md`'s original content (UPDATE 1-8) can likely be deleted entirely now —
  it documents a hang that was never real, and is superseded by this file.
- This update (Milestone 5) is not yet committed to git — will be committed together
  with this doc update.

## Current exact codebase state

**Committed to git** (through `499bb41`, Milestone 4): PIR + WS2812 (milestones 1-3),
OV3660 camera capture proven working standalone via `camera_test.rs` (Milestone 4),
workspace structure, `shared`/`server` scaffolding.

**Uncommitted, current** (Milestone 5, described above — about to be committed):
- `firmware/src/camera.rs` — refactored to the reusable `CameraHandle` struct
- `firmware/src/bin/camera_test.rs` — updated to use `CameraHandle`
- `firmware/src/bin/main.rs` — real PIR-triggered camera capture wired in
- `firmware/src/lib.rs` — now also declares `pub mod hexdump;`
- `firmware/src/hexdump.rs` — new shared serial hex-dump helper
- `scripts/capture_photo.sh`, `scripts/decode_capture.py` — new local dev tooling for
  triggering a capture and viewing the resulting photo
- `NEXT_STEP.md` — untracked by design (user preference, never `git add` this file)

## Hardware/tooling quirks discovered this project (useful context, not bugs to fix)

- macOS's "Allow accessory to connect?" security prompt can silently block the serial
  port until approved.
- This board's native USB-Serial/JTAG can get stuck in bootloader/download mode after
  `espflash`'s software reset sequence — a full physical unplug/replug reliably fixes
  it; a bare RESET press sometimes doesn't.
- `/dev/cu.usbmodemXXXX`'s numeric suffix changes across replugs (seen `1101` and
  `101` in the same session) — always re-`ls` before flashing/reading.
- **The single biggest lesson of this entire milestone**: a firmware test that prints
  its result exactly once is not trustworthy for hang/crash diagnosis on this board,
  because the host-side serial reader takes a moment to attach after every
  reset/replug, and that window reliably eats the first print(s). Any diagnostic
  firmware from here on should report status repeatedly/forever, never as a one-shot.
- `probe-rs` (installed this session, works via the ESP32-S3's built-in USB-JTAG) is
  a real, working hardware debugger for this board — useful for register-level
  inspection when actually needed, but not a substitute for first ruling out the
  observability issue above.
- Xtensa's windowed-register ABI encodes call-size bits in the top 2 bits of `a0`
  (return address register) — real address is `(a0 & 0x3FFFFFFF) | 0x40000000`.
  Only relevant if doing manual register/backtrace analysis again.
- `arduino-cli` + ESP32 core work fine as a side-by-side toolchain to the Rust/esp-hal
  one on this same Mac; useful for hardware-vs-software isolation testing. FQBN used:
  `esp32:esp32:esp32s3:CDCOnBoot=cdc,PSRAM=opi`.

## What's needed from Codex right now

Nothing blocking — **Milestone 5 is complete** (reusable `CameraHandle`, real
PIR-triggered capture in `main.rs`, verified repeatedly on hardware). The next
milestone is WiFi + upload to the server (see "Next steps" above) — open to input on
approach (e.g. `esp-radio`/`embassy-net` setup specifics, server request format) but
nothing is currently blocked.

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
- **Built-in microSD/TF-card slot, verified working.** Uses the ESP32-S3 SDMMC
  peripheral in 1-bit mode: CMD=GPIO38, CLK=GPIO39, DATA0=GPIO40. The user's 1GB
  FAT32 card is inserted and has been read/written repeatedly on real hardware
  (Milestones 17-18) via an unreleased upstream `esp-hal` fork -- see "Future
  local fallback storage" below for the full history. Insert/remove only while
  the board is powered off.
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

## Milestone 6 — continuous capture, MJPEG video assembly, UXGA resolution, 3x FPS fix

Still within the camera phase (video, not yet WiFi/upload). Everything below is
verified on real hardware. Committed in `cafe716`.

- **Continuous-capture benchmark** (`firmware/src/bin/video_test.rs`): reuses
  `CameraHandle` in a tight loop for a fixed window, no hex-dumping during capture
  (measured, not guessed: printing during a live capture starves the DMA — see
  Milestone 4 Blocker #2b), reports frame count/size/failures/achieved FPS. First run:
  58 frames in 10.42s (5.57 FPS), 1 failure, no resets — proved the reusable
  `CameraHandle` design holds up under sustained repeated use, not just occasional PIR
  triggers.
- **MJPEG video assembly pipeline**: `firmware/src/bin/mjpeg_test.rs` captures N frames
  back-to-back, hex-dumping each one immediately *after* it's captured (safe — no DMA
  transfer is in-flight while dumping, unlike printing *during* one). Host-side,
  `scripts/capture_video.sh` waits for the board's `MJPEG CAPTURE DONE` marker,
  extracts frames via `scripts/decode_capture.py` (now always numbers frames
  `_0.jpg`/`_1.jpg`/... , even a single frame, for a consistent ffmpeg input pattern),
  then assembles them into a real video via `ffmpeg`.
  - **First attempt produced an MJPEG-codec `.avi`** — the frames and assembled video
    both had valid, non-black pixel data (confirmed by extracting raw pixels and
    checking average brightness directly), but **macOS QuickTime Player has broken
    support for MJPEG-in-AVI and silently renders it as a black frame.** Not a capture
    bug — a playback-compatibility bug. Fixed by re-encoding to H.264 in an `.mp4`
    container instead (`-c:v libx264 -pix_fmt yuv420p`), which plays natively
    everywhere on macOS. The ESP32 still only ever produces raw JPEG frames; only the
    final local-viewing container/codec changed.
- **UXGA (1600×1200) resolution support**: added a `Framesize` enum to `ov3660.rs`
  (`Vga` | `Uxga`) and a second hardcoded crop/scale/PLL register table,
  `OV3660_FRAMESIZE_UXGA`, computed (not guessed) from the real driver's
  `set_framesize()` algorithm and its `ratio_table[ASPECT_RATIO_4X3]` crop window —
  same 4:3 aspect/crop window as VGA, but UXGA is more than half the sensor's native
  2048×1536 so it runs *without* 2×2 binning (different `0x3820`/`0x3821`/`0x4514`/
  `0x4520`/increment registers than VGA's binned path). Confirmed the PLL config is
  unchanged from VGA's (only `FRAMESIZE_QXGA` or a 16MHz XCLK need a different PLL, and
  neither applies at UXGA with our 20MHz XCLK) — so this reuses an already-verified PLL
  setting rather than needing new, unverified clock math. `CameraHandle::new()` now
  takes a `Framesize` argument; all existing call sites (`main.rs`, `camera_test.rs`,
  `video_test.rs`, `mjpeg_test.rs`) pass `Framesize::Vga` to keep existing behavior
  identical. New `firmware/src/bin/uxga_test.rs` (single-shot, 256KB buffer — a UXGA
  JPEG is ~5x VGA's bytes) and `uxga_video_test.rs` (continuous burst, 8 frames)
  verify it. First UXGA capture: 65,456 bytes, confirmed 1600×1200 valid JPEG.
  **Not yet attempted: QXGA (2048×1536, the sensor's true native max)** — the real
  driver uses a *different* PLL multiplier/divider specifically for `FRAMESIZE_QXGA`
  (24/1/3/.../8 instead of VGA/UXGA's 30/1/3/.../10), which hasn't been ported or
  tested yet. Deliberately stopped at UXGA as the lower-risk step since it reuses a
  proven PLL config; QXGA would need its own verification pass.
- **3x FPS fix for continuous capture**: `CameraHandle::capture_jpeg()` was
  unconditionally skipping 2 "warm-up" frames on *every* call — correct for a cold,
  infrequent PIR-triggered shot (sensor's auto-exposure/gain need to resettle after
  sitting idle), but wasteful in a continuous back-to-back burst, where it was
  discarding 2 out of every 3 frames captured for no reason. Fixed by splitting into
  `capture_jpeg` (unchanged, still skips 2 — used by `main.rs`'s PIR path,
  `camera_test.rs`, `uxga_test.rs`) and a new `capture_jpeg_continuous` (skips 0 — used
  by `video_test.rs`, `mjpeg_test.rs`, `uxga_video_test.rs`), both delegating to a
  shared private `capture_jpeg_with_warmup(buf, warmup_frames)`. **Measured effect:**
  VGA continuous capture went from 6.56 FPS to 20.06 FPS; UXGA went from 3.50 FPS to
  8.99 FPS — roughly the predicted 3x in both cases. `main.rs`'s PIR photo behavior is
  completely unchanged (still uses the original `capture_jpeg`, still skips warm-up
  every time, since a real motion event could be minutes after the last one).
- User's own verdict on the resulting VGA video (20 FPS): noticeably smoother, no
  longer feels like slideshow/fast-forward.

## Image quality investigation (research done, no code changes yet)

User asked to investigate every possible way to improve image sharpness/clarity before
considering a different camera module, and explicitly asked to distinguish grounded
claims from guesses. Investigated by reading the *entire* real `ov3660.c` (not just the
framesize-relevant parts read earlier) — every ISP tuning function it exposes
(`set_sharpness`, `set_denoise`, `set_contrast`, `set_saturation`, `set_brightness`,
`set_ae_level`, `set_wb_mode`, `set_lenc`, `set_raw_gma`, `set_bpc`/`set_wpc`,
`set_gainceiling`, `set_agc_gain`, `set_aec_value`) — cross-checked against what our own
`OV3660_DEFAULT_REGS` table already enables by default. Findings, ranked:

1. **Mechanical lens focus (highest potential impact, not a register)**: the real
   driver explicitly has no autofocus support (`sensor->af_is_supported = NULL;` and
   every other `af_*` hook `NULL`, comment `// No autofocus support`). This is
   fixed-focus hardware — if the sample image is uniformly soft/defocused (not yet
   provided/inspected), no register can fix that, only physically rotating the M12
   lens barrel can.
2. **Sharpness register** (`0x5300`-`0x530C`, `set_sharpness()`, level -3..+3): real
   ISP edge-enhancement, genuinely untouched by us or by the stock defaults (sensor
   runs its power-on "auto" sharpen mode). Cheap, safe, register-only, not yet tried.
3. **JPEG quality**: `COMPRESSION_CTRL07` (`0x4407`) is 0-63, lower = higher quality.
   We pass `10` — decent but not maxed; lowering to ~4-6 would reduce compression
   artifacts further, more noticeably at higher resolutions.
4. **Resolution** (UXGA/QXGA): more real pixels, but bounded by the lens's actual
   resolving power — already built (UXGA) as of Milestone 6 above.
5. Lens correction, gamma, bad-pixel-correction, white-pixel-correction (`0x5000`):
   **already enabled by default** in our own port and in stock Espressif defaults —
   no headroom left here, and this undercuts the idea that Freenove's default init
   specifically trades quality for speed (Freenove doesn't maintain its own OV3660
   driver; it uses this same reference driver).
6. Denoise, manual exposure/gain, contrast/saturation/white-balance presets: real but
   lower-impact/situational levers (denoise trades off against sharpness; manual
   AE/AGC only helps in fixed known lighting, risky for a PIR camera across changing
   conditions; contrast/saturation are cosmetic, not real detail).
7. **Community/undocumented register tricks** (GitHub issues, forums, Reddit,
   Discord): explicitly flagged as **not grounded** — no live access to those
   discussions in this session, declined to fabricate specifics. This is the one piece
   of the investigation suited to Codex rather than direct implementation.
8. **OV5640 verdict**: likely still a noticeable upgrade even after fully optimizing
   the OV3660 software-side, because the ceiling being optimized against is optical
   (small fixed-focus lens), not the ISP — OV5640 modules commonly have real autofocus
   and higher native resolution, addressing the root cause rather than working around
   it. Try the free software levers first; they cost nothing.

**Still waiting on**: an actual sample image from the user to inspect (not yet
provided) before prioritizing which of the above to implement first.

## Milestone 7 — PSRAM-backed recording (record-then-export, not yet PIR-triggered)

Codex research conclusion: PSRAM buffering is the right architecture for genuine
motion-triggered recording (vs. the Milestone 6 approach of hex-dumping each frame to
serial as it's captured, which only works for short test bursts, not a real "record for
N seconds" product). Verified Codex's specific technical claims against the real
`esp-hal 1.1.1`/`esp-alloc 0.10.0` source before writing any code:
- `esp_hal::psram::{Psram, PsramConfig}` and `Psram::raw_parts() -> (*mut u8, usize)`
  exist exactly as described.
- `esp_alloc::psram_allocator!` macro exists, and its doc comment carries the *exact
  same* atomic-safety warning Codex cited (`esp32-camera`/ESP32-S3: atomic instructions
  don't work correctly on PSRAM-resident memory).
- `esp_println::Printer::write_bytes(&[u8])` (raw binary write, not text formatting)
  exists, confirming raw-binary export is possible over the same USB-Serial/JTAG path
  already used for everything else.

Design decision made from this: rather than routing the frame arena through
`esp_alloc`'s global heap (which would mix internal/external memory in one allocator
and require careful capability-tagging to avoid ever putting an atomic in PSRAM), get
PSRAM's raw pointer+size directly via `raw_parts()` and manage a simple hand-rolled
arena over it -- no `Vec`/`Box`/atomics involved at all, so the atomics-in-PSRAM hazard
is avoided by construction, not by discipline.

- **`firmware/src/bin/psram_test.rs`** (new): standalone PSRAM smoke test, no camera
  code at all. Confirms `Psram::new()` reports ~8MB and that a byte pattern written
  across the whole region reads back correctly. **Verified on hardware**: exactly
  8,388,608 bytes (8.00MB) detected, 3/3 full-region pattern-check rounds passed, no
  crashes -- run twice (reflash + rerun) with identical results both times.
- **`firmware/src/recorder.rs`** (new): `PsramRecorder` -- appends length-prefixed
  frames (`[frame_len: u32 LE][timestamp_ms: u32 LE][JPEG bytes]`) into a raw
  `&'static mut [u8]` region (e.g. from `Psram::raw_parts()`). Deliberately holds no
  atomics, just plain bytes, for the reason above. `reset()`/`frame_count()`/
  `bytes_used()`/`capacity()`/`recorded_bytes()` round out the API.
- **`firmware/src/bin/psram_record_test.rs`** (new): records a fixed 5-second burst of
  VGA frames straight into PSRAM via `capture_jpeg_continuous` (Milestone 6's no-warmup
  fast path) + `PsramRecorder::record_frame`, with **zero serial output at all during
  recording** -- only after the window ends does it export the whole clip as one raw
  binary dump (`Printer::write_bytes`, not hex) framed by `RAW EXPORT BEGIN <n>`/
  `RAW EXPORT END` text markers. The DMA-adjacent scratch buffer stays in internal RAM
  (a 64KB `StaticCell`, same as earlier tests) -- only the already-complete JPEG gets
  copied into PSRAM, avoiding any PSRAM-DMA-alignment question entirely since it's a
  plain CPU `copy_from_slice`, not a DMA transfer into PSRAM.
- **`scripts/decode_raw_capture.py`** (new): parses the raw binary export (distinct
  from `decode_capture.py`'s hex-text parser) and computes **FPS from real per-frame
  timestamps** recorded on-device, not an estimate.
- **`scripts/capture_psram_video.sh`** (new): waits for `RAW EXPORT END`, decodes,
  assembles via `ffmpeg` into `.mp4`. Per explicit user request, individual frame files
  now go into a `mktemp -d` temp directory that's deleted on exit (`trap cleanup EXIT`)
  -- **only the final video is saved to the Desktop**, no more burst-photo clutter.
  Applied the same fix to `scripts/capture_video.sh` (Milestone 6's pipeline) for
  consistency.
- **Verified on hardware, run twice**: 68 frames/4.9s (13.88 FPS measured), then 59
  frames/4.95s (11.92 FPS measured) on a second run -- valid 640x480 H.264 `.mp4` both
  times, Desktop left with only the two video files (confirmed via `ls`), no leftover
  frame files anywhere (confirmed the temp dir gets cleaned up).
- **Known tradeoff, not yet addressed**: this pipeline's measured FPS (~12-14) is
  *lower* than Milestone 6's hex-dump-per-frame pipeline (20.06 FPS at VGA) -- the extra
  per-frame `copy_from_slice` into PSRAM costs real time that the old approach (dump
  straight from the scratch buffer) didn't pay. Not yet investigated whether this is
  worth optimizing (e.g. capturing directly into PSRAM instead of scratch-then-copy)
  or simply an acceptable cost for the architecture that actually supports real
  multi-second recording without a tethered Mac mid-capture.
## Milestone 8 — motion-triggered PSRAM recording (standalone test binary, verified)

Committed in Milestone 7's deliberately-deferred last step: wired the PSRAM recorder
to real PIR motion instead of boot/reset. Per plan, built as a separate test binary
first, not yet folded into `main.rs`.

- **`firmware/src/bin/motion_record_test.rs`** (new): combines `main.rs`'s PIR/LED
  pattern with Milestone 7's `PsramRecorder`/`CameraHandle`. On `MotionEdge::Detected`:
  LED red, `recorder.reset()`, record a fixed 5s VGA burst via
  `capture_jpeg_continuous` (same no-warmup fast path as the continuous-capture test
  binaries), export the clip as a raw binary dump (`RAW EXPORT BEGIN`/`END` markers,
  same format as Milestone 7), LED off, print `waiting for next motion event...`, then
  re-sync the debounced `MotionSensor` state (`MotionSensor::new(pir.is_high())`) so a
  body still present right after recording doesn't immediately register as a fresh
  edge. Also added a 10-second idle heartbeat print while waiting for motion --
  without it, the first test run produced a completely empty log for 80+ seconds of
  waiting with no way to tell "no motion yet" from "board is stuck," the exact
  observability trap this project's own hard-learned lesson (Milestone 4) warns about.
- **Verified on hardware, multiple independent runs**: two recordings in one boot
  (63 frames/0 failures, then 55 frames/0 failures); a later run, 61 frames/0 failures
  on its own boot. Every run: LED response correct, recorder correctly reset between
  events, zero crashes, zero failed frames across all runs.
- **Verified the full export-to-video path still works** with this trigger source, not
  just the boot-triggered one: decoded and assembled two separate motion-triggered
  recordings into playable `.mp4`s (640x480, ~5.05s each, 12-12.5 FPS measured from real
  timestamps) via the existing `decode_raw_capture.py`/`ffmpeg` pipeline.
- Per explicit request, `scripts/capture_psram_video.sh` (already built in Milestone 7)
  works unchanged for this trigger source too -- flash `motion_record_test`, run the
  script, wave, and it waits for the `RAW EXPORT END` marker and produces a video on
  the Desktop automatically, no manual log-capture/decode steps needed.
- **Not yet done**: still a standalone test binary, not integrated into `main.rs`. Per
  the plan ("Once hardware-verified, integrate it into the main firmware"), this is
  now hardware-verified and ready for that integration.

## Milestone 9 — `motion_record_test.rs` moved into `main.rs` unchanged

User's assessment: the current code already has motion-triggered 5-second recording,
but is missing *product-style event behavior* -- a real state machine (continue
recording while PIR stays active, a post-motion tail, a safe max clip length,
merging renewed motion into the current event instead of duplicate clips, waiting for
PIR to go low before rearming, thumbnail-frame selection, an actual export-timing
decision instead of immediate USB dump, and eventually circular pre-roll). None of
that is built yet -- deliberately. The identified smallest next step was purely
relocating the already-verified fixed-5-second recorder from `motion_record_test.rs`
into the real `main.rs`, with **no logic changes**, and re-verifying on hardware before
touching any of the state-machine behavior.

- **`firmware/src/bin/main.rs`** replaced with `motion_record_test.rs`'s content
  verbatim (same PIR/LED/PSRAM-record/raw-export logic, same 10s idle heartbeat, same
  debounced-motion re-sync after each recording), keeping `main.rs`'s original
  `#![deny(clippy::mem_forget)]`/`#![deny(clippy::large_stack_frames)]` lint attributes
  and its app-descriptor/doc-comment conventions. `firmware/src/bin/motion_record_test.rs`
  itself is left in place unchanged, as the standalone reference/regression test binary
  -- same precedent as `camera_test.rs` staying alongside `main.rs` since Milestone 5.
- **Verified on hardware**: reflashed the real `firmware` binary (not the test one) and
  triggered two motion events in one boot -- recording #1: 64 frames, 0 failures,
  5.02s, 743,193 bytes; recording #2: 69 frames, 0 failures, 5.06s, 816,274 bytes.
  Identical behavior to the standalone test binary's verified runs: LED red during
  recording, correct reset between events, correct heartbeat while idle, zero crashes.
- **Still exactly as limited as Milestone 8** -- this was a pure relocation, not new
  capability. Every item in the "product-style event behavior" list above (state
  machine, post-motion tail, max clip length, dedup, rearm-on-low, thumbnail selection,
  export-timing policy, pre-roll) is still unimplemented. This milestone only confirms
  the verified recorder behaves identically once it's the thing that actually ships.

## Milestone 10 — doorbell-style event state machine in `main.rs`

Replaced Milestone 9's fixed-5-second clip with a real motion-event state machine, per
explicit spec. Reviewed `PROJECT_STATUS.md` and the actual code first (per
instruction), and found a real pre-existing gap while doing so: `camera.rs`'s
`trim_to_jpeg()` used `.unwrap_or(0)`/`.unwrap_or(captured_len)` for a missing JPEG
SOI/EOI marker -- a corrupted frame would silently "succeed" with garbage bounds
instead of erroring. Fixed as part of this milestone since it was explicitly requested
("Add strict JPEG validation... must return an error, not silently succeed").

- **`firmware/src/camera.rs`**: `trim_to_jpeg()` now returns `Result<usize, CameraError>`
  instead of always succeeding; new `CameraError::InvalidJpeg` variant. Checked (via
  `grep`) that nothing does exhaustive matching on `CameraError` elsewhere in the
  codebase, so this is a non-breaking addition -- confirmed by building every other
  binary (`camera_test`, `video_test`, `mjpeg_test`, `uxga_test`, `uxga_video_test`,
  `psram_test`, `psram_record_test`, `motion_record_test`) after the change, all clean.
- **`firmware/src/bin/main.rs`** event logic: on motion, first frame uses
  `capture_jpeg()` (settling/warmup capture, correct after sitting idle) and is copied
  into a new internal-RAM thumbnail buffer as "the future event thumbnail" -- no
  separate photo taken, just an extra copy of the frame already in hand. Every
  subsequent frame in the same event uses `capture_jpeg_continuous()`. Per-frame,
  after capturing: check `pir.is_high()` -- HIGH clears any pending tail deadline
  outright (this single rule is what makes renewed motion during the tail restart the
  countdown, with no special-case code needed); LOW starts a 2-second tail deadline if
  one isn't already running. Event stops once that tail expires **and** at least 5
  seconds have elapsed since the event started (so a very brief motion blip still
  produces a full-length minimum clip, not a truncated one). Rearming reuses the
  existing `motion = MotionSensor::new(pir.is_high())` resync from Milestone 8/9 --
  already exactly "wait for PIR to go LOW before rearming" with no new mechanism
  needed, since a `MotionSensor` constructed while PIR is still HIGH won't fire a new
  `Detected` edge until it actually cycles LOW then HIGH again.
- **Verified on hardware**: 3 independent events in one boot, each landing at ~5.4-5.7s
  (consistent with a brief wave -- PIR drops low almost immediately, so the event runs
  until the 5s minimum, not the full tail-driven length), thumbnails preserved
  (12-21KB), low failure counts (1, 0, 1 frames -- consistent with the occasional DMA
  hiccups seen in earlier milestones, not new rejections from the stricter JPEG
  validation). Decoded event #1 and confirmed a valid, playable 640x480 `.mp4`
  (56 frames, matching the reported count exactly).
- **A 15-second absolute max was initially added, then explicitly removed** per
  direct instruction -- there is now no fixed maximum; an event only ends when PIR has
  genuinely gone LOW past the 2s tail, or PSRAM fills up. At the measured frame rate
  and JPEG sizes, the 8MB PSRAM is expected to hold roughly 35-50 seconds rather than
  several minutes. Verified
  again on hardware after the removal: a real walk-around test produced a 33.14-second,
  409-frame event that decoded and assembled into a valid clip.

### Behavior review: event can stop too early relative to real presence

During the 33-second walk-around test, the user's impression was that recording ended
*before* they physically finished moving (specifically: before finishing setting an
object down). The code path was checked and the tail logic is behaving exactly as
designed: any HIGH reading cancels a pending tail, and only a sustained LOW ends the
event. Research confirmed this is most likely a sensor-behavior issue rather than a
state-machine bug:

1. **PIR sensors are inherently worse at slow/small motion** than fast lateral
   movement -- carefully lowering an object is exactly the kind of motion a PIR can
   miss, potentially reporting LOW a couple seconds before the person is actually done.
2. **The AM312 has approximately a 2-second trigger/hold period and a 2-second
   blocking period.** The firmware's current 2-second tail therefore provides almost
   no margin for a sensor LOW gap or missed slow movement.

**Decision, now implemented:** `TAIL_DURATION` raised from 2 seconds to 5 seconds in
`firmware/src/bin/main.rs`, no other event behavior changed. Built clean (`cargo build`
+ `cargo clippy`, both with zero warnings), flashed, and tested on hardware. The user
confirmed the camera behavior, including the revised motion tail, is tested and the
camera phase is complete.

### Dev convenience: `RECORDING_ENABLED` compile-time toggle

User asked for a way to stop the board recording every time they move near it while
still developing. Added `const RECORDING_ENABLED: bool = true;` near the top of
`firmware/src/bin/main.rs` -- when `false`, a detected motion edge just prints
`motion detected -- recording disabled, ignoring` and skips the whole
capture/record/export block entirely; camera/PSRAM/LED still initialize normally at
boot either way, so this only silences the recording action itself, not hardware
setup. Flip and reflash to toggle; no new hardware required. Built and clippy-checked
clean. The user then flipped it to `false` directly in the editor for their current
dev session (not yet reflashed as of this doc update).

Discussed the natural evolution of this: once the WiFi/server milestone exists, the
right design is for this to become a remotely-settable flag (app/web UI sets
enabled/disabled on the server, firmware checks it instead of a hardcoded constant)
rather than a compile-time constant requiring a reflash to change. Not building that
now -- it's blocked on WiFi existing at all, and the compile-time flag is the correct
stopgap until then.

### Known deferred limitations

- **USB-to-Mac delivery already works.** After recording, firmware exports the clip
  over native USB serial; the existing Mac script receives it, reconstructs it, and
  produces a playable video. The upcoming transport milestone is not the first ability
  to send or view recordings on the Mac -- it replaces this proven wired/debug path
  with WiFi and a persistent Mac server.
- The first valid JPEG is preserved in RAM as a thumbnail, but it is not separately
  exported or saved yet. Its transport/storage policy belongs with WiFi/server work.
- Raw USB clip export is synchronous and blocks motion detection. Motion occurring
  during an export is therefore missed; non-blocking/queued transfer is deferred to
  the transport milestone.
- There is no circular pre-roll, so a clip starts only after the PIR triggers. Pre-roll
  remains explicitly deferred.
- With 8MB PSRAM, measured frame sizes, and the current frame rate, practical clip
  capacity is roughly 35-50 seconds, not several minutes. PSRAM-full remains the hard
  stop because the explicitly removed fixed-duration maximum has not been restored.

### Future media payload design — optional audio

The current board/setup has no microphone, so recordings are video-only and no audio
capture work belongs in the current camera milestone. However, the upcoming shared
protocol/server design must not hardcode an event as "video only."

- Define an event envelope in the `shared` crate with event metadata and independently
  described media parts/streams (for example: thumbnail JPEG, MJPEG video, and future
  audio). Each part should identify its media kind, encoding, byte length, and timing
  information needed for later audio/video synchronization.
- Audio is **optional**. Current firmware sends no audio part at all; it must not
  allocate or transmit an empty audio buffer merely to reserve the feature.
- The server must accept and store events containing whichever supported media parts
  are present. Initially that means thumbnail/video only.
- When an external microphone is added later, firmware-side microphone capture can
  attach an audio part through the same protocol instead of redesigning the payload.
- This is a future-proofing requirement for the WiFi/server-transfer milestone, not a
  claim that audio recording is currently implemented.

### Future local fallback storage — onboard microSD

The board does have an onboard microSD/TF-card slot, so no external SD module or
jumper wiring is required. The available 1GB card could later provide persistent local
storage when WiFi or the Mac/mini-computer server is unavailable.

- Intended future flow: record temporarily into PSRAM, save/queue the completed event
  on microSD when needed, upload over WiFi, then delete it from the card only after a
  successful server acknowledgement.
- SD support is not implemented or hardware-tested yet. It will require Rust SDMMC
  support, a FAT filesystem, event filenames/indexing, capacity handling, and a safe
  retention/deletion policy.
- GPIO38, GPIO39, and GPIO40 must be reserved for the onboard 1-bit SDMMC connection
  and not assigned to unrelated peripherals if this feature is added.
- This remains a separate milestone after basic HTTP upload to the Mac server works;
  it does not replace or delay the immediate WiFi transport work.

**Codex research result (2026-07-05): an onboard-slot path now exists, but only as an
unmerged upstream driver.** The original conclusion above was accurate for released
`esp-hal`, but very recent upstream work changes the practical answer:

- Released `esp-hal` 1.1.1 still has no public `sdmmc` module. The official latest
  documentation lists the supported peripheral modules and SDMMC is absent. A fresh
  clone/search of current `esp-rs/esp-hal` main likewise found only the generated
  `SDHOST` peripheral singleton/signals, not a usable host/card driver.
- Open, non-draft upstream PR
  [esp-rs/esp-hal#5760](https://github.com/esp-rs/esp-hal/pull/5760), titled
  **"SDMMC/SDIO host"**, was opened 2026-06-21 and remained open/unmerged at research
  time. Its changelog explicitly adds an initial host driver for ESP32, ESP32-S3 and
  ESP32-P4. Researched exact revision:
  `cef6d86604d91abcf62afc9804724a637eb7af3a` from contributor branch
  `bugadani/esp-hal:sdmmc`.
- Crucially, that PR contains an async ESP32-S3 SD-card/FAT smoke test using the exact
  Freenove onboard-slot wiring already documented here: SDMMC slot 1, CLK=GPIO39,
  CMD=GPIO38, DATA0=GPIO40, one-bit mode. It constructs
  `SdHostController::new(peripherals.SDHOST, Config::default())`, selects
  `controller.slot::<1>(...)`, attaches those three pins, calls `.into_async()`, then
  initializes the card through `sdio::sd::Card`. Its FAT test uses
  `embedded-fatfs`, `embedded-partitions`, and `block-device-adapters` to mount the
  first FAT partition/superfloppy and perform a non-destructive create, read, update,
  and delete cycle. Upstream source:
  [sdmmc_sd_async.rs](https://github.com/bugadani/esp-hal/blob/cef6d86604d91abcf62afc9804724a637eb7af3a/qa-test/src/bin/sdmmc_sd_async.rs).
- This means the inserted 1GB FAT32 card and built-in slot are viable candidates; an
  external SPI breakout should **not** be purchased yet. The lack of DAT3/CS still
  prevents using the onboard slot through SPI-oriented crates such as
  `embedded-sdmmc`, but it is irrelevant to the new native one-bit SDMMC path.
- Codex performed a dependency compatibility experiment in an isolated `/tmp` copy,
  leaving this working tree and lockfile untouched. Replacing only `esp-hal` with the
  PR revision did **not** work: Cargo produced duplicate native `links` conflicts
  (`esp_rom_sys`, then `xtensa-lx-rt`) and a registry `esp-rtos` macro mismatch. After
  pinning/patching the matching `esp-hal` ecosystem crates from the same exact
  revision (including the HAL/runtime/support crates needed to keep one coherent
  source set), enabling `esp-hal/__sdmmc`, and retaining the project's compatible
  `esp-radio 0.18.0`, the **current complete `firmware` binary passed
  `cargo check --release`**. So the driver can coexist at compile time with this
  camera/PIR/LED/WiFi firmware, but adopting it is a broader temporary ecosystem pin,
  not a safe one-line dependency addition.
- ESP-IDF does have official native SDMMC and SDSPI host/protocol drivers, but they are
  ESP-IDF driver components, not ROM calls or a small "IDF-lite" library that can be
  dropped into the current bare-metal `esp-hal`/`esp-rtos` ownership model. Using them
  directly would require migrating the firmware to the ESP-IDF Rust stack or manually
  porting/binding a substantial driver, neither justified while the upstream
  bare-metal PR exists. `esp-bootloader-esp-idf` in this project supplies bootloader
  compatibility; it does not provide the ESP-IDF runtime or storage drivers.

**Decision / next safe step (as written by Codex):** commit Milestone 16 first, then
create an isolated, standalone `sdmmc_test` milestone pinned to the exact PR revision
and matching ecosystem sources. Port only the upstream create/read/update/delete FAT
smoke test; do not integrate SD into `main.rs` yet. Flash it against the inserted 1GB
FAT32 card and verify the real slot. If it fails or the dependency override
destabilizes existing subsystems, revert the isolated experiment and use an external
3.3V SPI microSD breakout later. If it passes, run full camera/PIR/LED/WiFi regression
builds and hardware checks before accepting the unreleased dependency set or building
the queue.

### Result: the onboard slot works -- verified twice on real hardware (Claude)

Milestone 16 was committed first (`0a2feb3`), then a new, fully isolated `sdmmc_test/`
crate was created (own `Cargo.toml`/`Cargo.lock`/`[workspace]`, not part of `firmware/`'s
dependency graph or the root workspace's `["server", "shared"]` members -- deliberately
so this unreleased/experimental dependency pin cannot destabilize the camera/WiFi
firmware even if it turned out broken):

- Pinned exactly as Codex's research specified: `esp-hal`, `esp-rtos`,
  `esp-bootloader-esp-idf`, `esp-println`, `esp-backtrace` all as `git` dependencies at
  `https://github.com/bugadani/esp-hal`, `rev = "cef6d86604d91abcf62afc9804724a637eb7af3a"`;
  `embedded-fatfs`/`embedded-partitions` at
  `https://github.com/MabezDev/embedded-fatfs`, `rev = "518528cc111fcf65c48abbdeb80735a38eada112"`;
  a `[patch.crates-io]` redirecting `block-device-driver`/`block-device-adapters` to that
  same fork -- reconstructed from the upstream PR's own `qa-test/Cargo.toml` (fetched
  directly) rather than guessed.
- Ported `qa-test/src/bin/sdmmc_sd_async.rs` from that exact revision (fetched directly,
  not paraphrased), trimmed to this board's one chip/pin set (ESP32-S3, 1-bit mode,
  CLK=GPIO39/CMD=GPIO38/DATA0=GPIO40, SDMMC slot 1) instead of the original's multi-chip
  `cfg_select!`. Kept the upstream design intact: non-destructive by default, gated
  behind a BOOT-button (GPIO0) press, operating only on one test-owned file
  (`ESPQA.TXT`) that's created, read back, updated, read back again, then deleted --
  never touching anything else on the card. Added this project's own repeating
  "still alive" heartbeat print around the button-wait loop, matching this project's
  established lesson about one-shot prints being missed by the serial reader.
- **First flash attempt failed** (`espflash` error: "appdesc segment not found").
  Root-caused by comparing ELF symbol addresses and program headers against the known-
  working `firmware` binary: `esp_app_desc` landed at an IRAM address (`0x00403844`)
  instead of the expected flash-mapped region (`0x3c000020`), and the binary had
  collapsed to 3 tiny LOAD segments instead of the proper 5-6 flash/IRAM/DRAM regions.
  Traced to a genuinely easy-to-miss cause: **the new crate was simply missing the
  `build.rs` that every esp-hal application crate needs**, which emits
  `cargo:rustc-link-arg=-Tlinkall.x` (the linker script that actually places sections in
  the right memory regions) -- this isn't provided automatically by any dependency's own
  build script (confirmed by diffing verbose `cargo build -v` link commands and every
  relevant dependency's build-script `stdout` between `firmware` and `sdmmc_test`; the
  `-Tlinkall.x` argument only appeared in `firmware`'s own `build.rs`, never in any
  crate's). Copied `firmware/build.rs` verbatim into `sdmmc_test/`; the very next build
  placed `esp_app_desc` correctly and produced a properly laid-out binary that flashed
  and ran.
- **Verified twice on real hardware with the user's inserted 1GB FAT32 card**: after the
  fix, the board booted, initialized the SD card over the real SDMMC hardware interface,
  and correctly waited for a BOOT-button press before doing anything. Two separate
  button presses (the user pressing BOOT live, Claude watching the serial log) both
  produced `async FAT CRUD: PASS` -- a genuine create, read-back, update, read-back,
  delete cycle against the physical card, not a mock or a dry run.
- **Conclusion: the onboard microSD slot is viable.** An external SPI breakout is not
  needed. The remaining risk Codex flagged -- that this requires a broad, coherent
  pin of an entire unreleased `esp-hal` ecosystem, not a safe one-line addition -- was
  accurate but has since been paid down: Milestone 17 migrated `firmware/`'s real
  dependencies to this same pinned fork/revision and Milestone 18 wired the SD queue
  directly into `main.rs`, both verified on real hardware.
- `sdmmc_test/` was committed in `9292db7` (Milestone 17) as the isolated hardware
  proof; it stays deliberately separate from `firmware/` rather than being deleted,
  since it's the smallest possible reproduction of "does the onboard slot work at all"
  independent of everything else in the real firmware.

## Milestone 11 — WiFi connectivity (scan + connect, verified on hardware)

First real step of the WiFi/transport phase: prove the radio and network stack work
at all before building any HTTP/server logic on top, matching this project's
established pattern (a dedicated standalone test binary per new subsystem, e.g. the
PSRAM smoke test in Milestone 7). Grounded every API in the real `esp-radio 0.18.0`/
`embassy-net 0.9.1` source before writing code (same discipline as the PSRAM work):
confirmed `esp_radio::wifi::new(peripherals.WIFI, Default::default())` returns
`(WifiController, Interfaces)`, that `Interfaces::station` (an `Interface<'d>`)
implements the `embassy_net_driver::Driver` trait required by `embassy_net::new()`,
and the exact `StationConfig`/`Config::Station`/`set_config`/`connect_async` sequence
from the crate's own doc examples rather than guessing.

- **`firmware/src/bin/wifi_scan_test.rs`** (new): no credentials needed -- calls
  `controller.scan_async(&ScanConfig::default().with_max(20))`, sorts by signal
  strength, prints SSID/channel/RSSI/auth method every 15s. Used to find the user's
  real network before writing any connection code. **Verified on hardware**: found
  4-5 real nearby networks correctly (including two `CommunityFibre10Gb_*` SSIDs, an
  extender, and others), RSSI values sensible (-45 to -71 dBm).
- **`firmware/src/bin/wifi_test.rs`** (new): connects to a real network via
  `StationConfig`, brings up an `embassy_net::Stack` (a background `net_task` spawned
  via `Spawner` drives the `Runner`), waits for `stack.wait_config_up()`, reports the
  DHCP-assigned IP repeatedly. **Verified on hardware, first real attempt**: connected
  to the user's actual WiFi network, DHCP assigned `192.168.1.204` (gateway
  `192.168.1.1`), connection held stable across multiple heartbeat checks (didn't
  drop).
- **Credentials handling**: `firmware/src/wifi_credentials.rs` added to `.gitignore`
  (holds the real SSID/password, pulled in via `include!("../wifi_credentials.rs")`
  from both WiFi test binaries) with `firmware/src/wifi_credentials.rs.example`
  committed as the template. Confirmed via `git check-ignore -v` and `git status`
  that the real-credentials file never appears as trackable.
- Small API-fitting fixes needed during implementation (all found by the compiler,
  not guessed in advance): `set_config()` already starts the controller (no separate
  `.start()` call), `is_connected()` returns a plain `bool` not a `Result`, spawning
  the `net_task` requires unwrapping the `Result<SpawnToken, SpawnError>` the task
  function itself returns before passing it to `spawner.spawn()` (which takes a bare
  token and returns `()`, not a `Result`), and the device type for `Runner`/`Stack` is
  `esp_radio::wifi::Interface`, not a `WifiDevice` type (which doesn't exist in this
  crate version).

## Milestone 12 — HTTP POST proven end-to-end (ESP32 -> WiFi -> Mac server)

Grounded the TCP API the same way as everything since Milestone 7: read
`embassy-net 0.9.1`'s real `tcp.rs` source before writing code, confirming
`TcpSocket::new(stack, rx_buffer, tx_buffer)`, that it implements
`embedded_io_async::{Read, Write}` (so `write_all`/`read` are available, no extra HTTP
client crate needed), and that `connect()` accepts anything `Into<IpEndpoint>`
(a `(IpAddress, u16)` tuple, per smoltcp's `wire::ip::Endpoint` `From` impl).

- **`server/src/main.rs`** replaced with a minimal HTTP receiver (plain `std`, no
  dependencies added): `TcpListener` accept loop, reads until the header-ending blank
  line, extracts `Content-Length`, reads exactly that many body bytes, prints both,
  replies `200 OK`. Deliberately not a real router/parser -- this proves the pipe,
  not the protocol.
- **`firmware/src/bin/http_post_test.rs`** (new): same WiFi connection logic as
  `wifi_test.rs`, then opens a `TcpSocket` to `SERVER_IP:SERVER_PORT` (new consts in
  the gitignored `wifi_credentials.rs`/`.example`) and sends a hand-written HTTP/1.1
  POST request with a small text body every 10s, printing the server's response.
- **Verified on hardware, full round trip**: board connected to the Mac's server,
  server correctly parsed and printed each request's body
  (`hello from esp32-s3, post #N`), board received and printed a real `200 OK`
  response -- repeated successfully across multiple posts, not just once.
- **One real bug caught and fixed**: the `Host` header was being built with
  `{SERVER_IP:?}` (debug-formatting the raw `[u8; 4]` as `[192, 168, 1, 21]`) instead
  of a proper dotted-decimal string. Doesn't break the receiving end (the minimal
  server ignores `Host` entirely), but fixed for correctness and reverified on
  hardware (confirmed `Host: 192.168.1.21:8080` in the next capture).

### Security threat model — camera and personal devices share the LAN

The WiFi proof places the ESP32 camera on the same home subnet as the Mac, phones,
PS5, and other personal devices. Sharing a subnet does not automatically give the
camera control of those devices, but it creates a possible lateral-movement path: if
any one LAN device is compromised, an attacker can probe other reachable devices.
Conversely, an untrusted LAN client can currently connect to the development receiver
on the Mac while it is running.

The Milestone 12 receiver is intentionally a transport experiment, **not a secure
server**. Review of the actual code confirms that it currently binds to `0.0.0.0:8080`,
uses plaintext HTTP, accepts requests without authentication, grows its request buffer
without a size limit, has no read timeout, handles clients serially, and returns
`200 OK` even if a body ends early. It is acceptable only as a temporary trusted-LAN
test and must not be exposed through router port forwarding, UPnP, or the public
internet. Real camera images/video must not be wired into this receiver until the
minimum protections below are added.

Security requirements for the real local upload path:

- Add per-device authentication to every upload. Keep the secret out of git, compare
  it on the server, and reject missing/invalid credentials before storing a body.
- Enforce a small header limit, an explicit maximum payload size, read timeouts,
  exact-body validation, allowed routes/methods/content types, and safe server-chosen
  filenames under one dedicated storage directory. Never accept a client-provided
  filesystem path.
- Return success only after the complete validated event is safely stored; use clear
  error responses so firmware can retry without treating a partial upload as complete.
- Keep the macOS firewall enabled and allow only the required server process/port.
  Keep router WPA2/WPA3, router firmware, admin credentials, firewall, and WiFi
  password secure; disable remote administration, WPS, UPnP, and port forwarding when
  they are not explicitly required.
- Add transport encryption before uploads cross an untrusted network. Plain HTTP on
  the present trusted development LAN is only an intermediate bring-up step; its
  authentication token and camera footage are otherwise visible to a device capable
  of observing that traffic.
- Long term, place the camera on a dedicated IoT VLAN/SSID and allow only the minimum
  route `camera -> Mac/mini-computer upload port`; block camera access to phones, PS5,
  general Mac services, and other LAN clients. A normal guest network with client
  isolation may also block the required camera-to-server connection, so isolation
  rules must deliberately permit that one path.
- The ESP32 should remain an outbound upload client, with no general-purpose inbound
  web/admin service unless a later feature genuinely requires one.

## Milestone 13 — receiver hardened per the security threat model

Implemented the full checklist from Milestone 12's security review before any real
camera footage gets wired in. Nothing here is a new architecture -- same minimal
`POST /upload` receiver, now actually safe to point a real (if still trusted-LAN-only)
upload at.

- **Auth token**: a shared secret (`X-Upload-Token` header) generated with
  `openssl rand -hex 16`, stored in gitignored config on both ends --
  `firmware/src/wifi_credentials.rs` (`UPLOAD_TOKEN`, alongside the existing WiFi/
  server consts) and the new `server/src/config.rs` -- with `.example` templates for
  both committed. **Verified on hardware**: a request without/with the wrong token
  gets rejected with `401 Unauthorized`; the correct token succeeds.
- **Bounded parsing**: `MAX_HEADER_BYTES` (8KB) and `MAX_BODY_BYTES` (12MB --
  comfortably above the ~8MB/35-50s PSRAM clip ceiling measured in Milestone 10,
  without being unbounded) both enforced before any data past those limits is
  buffered; a 10-second read timeout so a stalled connection can't hang the server
  forever.
- **Exact-body validation**: `read_exact_body()` now returns a real error
  (`BodyTruncated`) instead of the old behavior of silently returning `200 OK` on a
  connection that closed before `Content-Length` bytes arrived.
- **Method/route validation**: only the literal request line `POST /upload HTTP/1.1`
  is accepted; anything else is rejected before any body is even read.
- **Safe storage**: server-chosen filenames only (`upload_<unix_millis>.bin`, never
  derived from client input) under one dedicated `server/uploads/` directory.
- **Correct acknowledgements**: a `RequestError` enum maps each failure to the right
  HTTP status (`401`, `411`, `413`, `431`, `400`) instead of always returning `200`.
- **One real bug caught during hardware verification, not before**: `UPLOADS_DIR` was
  a plain relative `"uploads"` path, which resolves relative to whatever directory the
  binary happens to be launched from -- `cargo run -p server` from the workspace root
  put files at the repo root (`/uploads/`), outside `server/` entirely and outside the
  `.gitignore` pattern (`server/uploads/`) meant to cover them. Fixed with
  `concat!(env!("CARGO_MANIFEST_DIR"), "/uploads")`, an absolute path baked in at
  compile time, independent of the launch directory. Reverified on hardware after the
  fix: uploads land at `server/uploads/` and are correctly gitignored there (confirmed
  via `git check-ignore -v`); the stray root-level files from before the fix were this
  session's own test artifacts and were deleted.
- Still deliberately deferred, per the threat model: TLS and IoT network segmentation.
  This remains a trusted-LAN development tool, not something to expose beyond that.

### Post-Milestone 13 security review — two changes required

Codex reviewed the actual hardened receiver rather than relying only on the milestone
description. The server builds cleanly, and the authentication, body-size limit,
timeout, exact-body validation, safe storage path, and successful-write acknowledgement
are present. Two follow-up changes were identified/decided before real camera footage
is uploaded:

1. **Fix the header-limit boundary check.** `read_headers()` currently searches for
   `\r\n\r\n` before checking whether the buffer has grown beyond
   `MAX_HEADER_BYTES`. Because reads happen in 4KB chunks, a terminator arriving in
   the chunk that pushes the buffer beyond 8KB can be accepted. Enforce the limit on
   the terminator position/bytes consumed immediately after each read, and add tests
   for a header exactly at the limit and one just over it. Until fixed, the claim that
   the 8KB header limit is fully enforced is not exact.
2. **Use one generic `404 Not Found` response for concealment.** Per user preference,
   unknown routes, missing upload tokens, and incorrect upload tokens should all return
   the same minimal `404 Not Found` response. Server-side logs may record a generic
   rejection reason but must never print the supplied or expected token. This avoids
   confirming that `/upload` is a real authenticated endpoint. It is an additional
   concealment layer, not a replacement for authentication, rate limiting, or future
   TLS. The current implemented behavior is still `401 Unauthorized` for failed auth
   until this small response-mapping change is made and retested.

### Both post-review gaps closed and reverified

- **Header-limit boundary, fixed**: `read_headers()` in `server/src/main.rs` now checks
  `buf.len() > MAX_HEADER_BYTES` *before* searching for the `\r\n\r\n` terminator each
  iteration, not after -- so a terminator arriving in the same read that pushes the
  buffer past the limit no longer bypasses the rejection. **Verified**: a request with a
  ~20KB custom header now correctly triggers `HeadersTooLarge` (confirmed in the
  server's own log, `request failed: HeadersTooLarge`).
- **Generic 404, implemented**: `RequestError::status_line()` now maps both
  `BadRequestLine` (unknown route) and `Unauthorized` (missing/wrong token) to the
  identical `"HTTP/1.1 404 Not Found"` response. Logging already never printed the
  token itself (only "missing/invalid X-Upload-Token" or the submitted request line),
  so no logging change was needed.
- **Verified on the running server with `curl`, all four cases**:
  - Valid token -> `200 OK`
  - Missing token -> `404 Not Found`
  - Wrong token -> `404 Not Found`
  - Unknown route (`POST /nonexistent`) -> `404 Not Found`
  - All three failure responses confirmed byte-for-byte identical, and distinct from
    the `200 OK` success case.
- Real board traffic (`http_post_test.rs`, still running throughout this test session)
  kept succeeding the whole time, confirming the fixes didn't regress the legitimate
  path.
- Test upload artifacts from this verification pass were this session's own throwaway
  data and were deleted afterward; nothing real was stored.

### Second Codex review — remaining pre-media cleanup

The two first-review fixes are genuinely present and `cargo check -p server` remains
clean. The receiver is now a **minimally hardened trusted-LAN prototype**, not a fully
hardened production server. A second review of the actual code found these remaining
items:

1. **Header accounting is safe but overly broad.** The new check closes the original
   bypass, but it compares `buf.len()` with `MAX_HEADER_BYTES`. That buffer can already
   contain body bytes read in the same TCP chunk as the header terminator, so a valid
   header close to 8KB could be rejected merely because some body bytes arrived with
   it. Locate `\r\n\r\n`, compare its end position (the true header length) with the
   limit, and only use total buffer length to reject a still-unterminated header.
2. **The requested boundary tests do not exist in the codebase yet.** Hardware/curl
   testing proved that a ~20KB header is rejected, but there are no automated tests for
   a header exactly at the allowed boundary and one byte over it. Add those tests,
   including a valid near-limit header with body bytes coalesced into the same read.
3. **Timestamp-only storage names are not collision-proof.**
   `upload_<unix_millis>.bin` can theoretically overwrite an earlier upload created in
   the same millisecond. Structured event storage should use a unique event ID and/or
   collision-safe file creation; it must never silently replace an accepted event.

Still deliberately later rather than blockers for the next trusted-LAN experiment:
TLS, rate limiting, atomic temporary-file/rename storage, firmware retry behavior, and
IoT VLAN isolation. These are why project language should say "minimally hardened for
a trusted development LAN," not "fully hardened."

### All three second-review findings fixed and verified

- **Header-length accounting, fixed**: `read_headers()` in `server/src/main.rs` is now
  generic over `Read` (was hardcoded to `TcpStream`, blocking unit testing) and checks
  the limit against the *header's own length* (the `\r\n\r\n` terminator's position),
  not the total accumulated buffer -- a small, valid header no longer gets wrongly
  rejected just because body bytes happened to arrive coalesced into the same TCP
  read. The "no terminator yet" branch still checks total buffer length, which is
  correct there since every byte read so far genuinely is header content until a
  terminator actually appears.
- **Automated boundary tests, added**: 4 new `#[cfg(test)]` unit tests in
  `server/src/main.rs` using `std::io::Cursor` as an in-memory `Read` mock (no real
  socket needed) -- header exactly at the limit (accepted), one byte over (rejected),
  a header under the limit with body bytes coalesced in (accepted, the actual bug this
  guards against), and an unterminated blob past the limit (rejected). All 4 pass
  (`cargo test -p server`). One test's first draft had a wrong assertion (expected
  `read_headers` to slurp every byte provided to the mock reader before returning, when
  it correctly returns as soon as the terminator is found in whatever a single read
  pass produced) -- caught by the test failing, not assumed; fixed the assertion, not
  the implementation.
- **Third-review catch, fixed**: the coalesced-body test's first version used a
  100-byte header, which `read_headers()`'s 4096-byte read chunk finds and returns on
  the very first read -- buf.len() never got anywhere near the 8192-byte limit, so the
  test didn't actually exercise the boundary its name claimed to guard. Root cause: the
  read loop pulls a fixed 4096-byte chunk per call, and the limit (8192) is an exact
  multiple of that chunk size, so buf.len() at terminator-discovery can only ever reach
  *up to* the limit, never past it, while header_len stays under it -- reaching past it
  would require the terminator itself to sit beyond the limit, which is a reject case,
  not an accept case. Fixed by using an 8000-byte header (forces two full reads before
  the terminator is found) plus a trailing coalesced body large enough that the second
  read is a full 4096-byte chunk, landing buf.len() exactly on `MAX_HEADER_BYTES`
  (8192) while header_len (8000) stays comfortably under it -- the strongest gap
  reachable given the chunk/limit relationship. Verified the test actually catches a
  regression: temporarily reintroduced a `buf.len() >= MAX_HEADER_BYTES` bug in the
  found-terminator branch, confirmed both this test and the exact-limit test failed,
  then restored the correct `header_len > MAX_HEADER_BYTES` check and re-ran clean.
- **Collision-safe storage, fixed**: new `store_upload()` uses
  `OpenOptions::new().create_new(true)`, which atomically fails if the target filename
  already exists, retrying with an incrementing `_N` suffix on collision instead of
  ever silently overwriting a previously-stored upload. **Verified with a real
  concurrent 5-request burst** that genuinely collided on the same millisecond
  timestamp -- all 5 preserved as `upload_<ts>.bin`, `_1.bin`, `_2.bin`, `_3.bin`,
  `_4.bin`, each with its distinct correct body intact, none overwritten.
- **Full regression re-verified live** against the doubly-fixed server: valid token ->
  `200`, missing token -> `404`, unknown route -> `404`, ~20KB oversized header ->
  rejected (server log confirms `HeadersTooLarge`) -- real board traffic
  (`http_post_test.rs`) kept succeeding throughout. Test artifacts from this
  verification pass were deleted afterward.

Milestones 11-13 are now **committed** (`377f260`), along with `DEPLOYMENT.md`; secrets
(`firmware/src/wifi_credentials.rs`, `server/src/config.rs`) and `NEXT_STEP.md` stayed
untracked as intended.

## Milestone 14 — real event data: shared envelope, thumbnail, then video (verified on hardware)

Replaced the placeholder text body with real captured media, using a small hand-rolled
wire format instead of a serde/postcard dependency (firmware is `no_std` with only a
tiny on-chip heap; the format only ever needs to describe "how many bytes, what kind"
around data that already lives in a buffer -- no owned allocation needed on either
side).

- **`shared` crate now defines the event envelope** (`shared/src/lib.rs`, replacing the
  untouched `add()` stub): a 6-byte header (`CAM1` magic + version + part count)
  followed by one or more parts, each a 5-byte header (kind byte + little-endian u32
  length) plus that many raw payload bytes. `PartKind` is `Thumbnail`/`Video`/`Audio`
  (audio reserved, unused). Encode side returns fixed-size arrays (no allocation, so
  firmware can write header bytes then stream the payload straight from its existing
  capture/PSRAM buffer without ever copying the whole event into one contiguous
  buffer). Decode side (`decode_envelope_header` + `PartsIter`) borrows from the
  caller's byte slice throughout -- also no allocation, works the same whether the
  caller then copies each part into a file (the `server` crate) or just inspects it.
  7 `#[cfg(test)]` unit tests cover round-tripping one part, round-tripping two parts,
  and every error case (bad magic, unsupported version, truncated header, unknown part
  kind, part length exceeding the buffer). All pass (`cargo test -p shared`).
- **`firmware/src/bin/event_upload_test.rs`, new**: WiFi connect (same as
  `http_post_test.rs`) + real `CameraHandle::capture_jpeg` (same pin wiring as
  `camera_test.rs`), wraps the captured JPEG as a single `Thumbnail` part, and POSTs it
  to the hardened `server` receiver. **Verified on real hardware**: captured real
  640x480 JPEGs (~10.6KB each), got `200 OK` responses, and the server stored each as
  its own `.jpg` file that `file`/`sips` confirm is a valid, correctly-decoded JPEG (one
  frame opened and visually confirmed -- a real photo of a lamp fixture, not corrupt
  data).
- **`server/src/main.rs` storage rebuilt around the envelope**: `store_upload()`
  (single raw-bytes-to-one-file) replaced by `store_event()` + `create_event_part_file()`
  -- parses the envelope, then writes each part to its own
  `event_<timestamp>_<label>.<ext>` file (`.jpg` for thumbnails; `.bin` for
  video/audio until a container format is chosen), sharing one timestamp per event so
  parts sort together, with the same collision-safe `create_new(true)`-plus-retry-suffix
  scheme Milestone 13 established. Malformed envelopes map to `RequestError::InvalidEnvelope`
  -> `400 Bad Request`. 3 new `#[cfg(test)]` unit tests (single-part storage incl.
  content-equality, two-part storage with distinct filenames/extensions, malformed-body
  rejection) run against a scratch temp directory, not the real `UPLOADS_DIR`
  (`store_event`/`create_event_part_file` now take `dir` as a parameter specifically so
  tests don't touch real uploads). All pass (`cargo test -p server`, 7 total including
  Milestone 13's).
- **Verified end-to-end with a real JPEG before touching hardware**: built an envelope
  body from a real JPEG in Python, POSTed it via `curl` to a locally running server,
  confirmed the stored file was byte-identical to the source and opened correctly --
  caught any integration mistakes before involving the board at all.
- **`firmware/src/bin/event_upload_video_test.rs`, new**: adds a recorded video clip
  alongside the thumbnail. Captures one still JPEG, then records a fixed 5-second burst
  straight into PSRAM via the existing `PsramRecorder` (same approach as
  `psram_record_test.rs`), and uploads both as a two-part envelope (`Thumbnail` +
  `Video`) in one HTTP POST. The video part's payload is
  `PsramRecorder::recorded_bytes()` verbatim -- the same `[frame_len: u32 LE]
  [timestamp_ms: u32 LE][JPEG bytes]` per-frame format `scripts/decode_raw_capture.py`
  already parses, just delivered over WiFi instead of serial, so no new decode logic
  was needed. **Verified on real hardware**: 3 consecutive events, each 70 frames
  (~747KB) recorded at the expected ~13.9 FPS (matching Milestone 7's measured PSRAM
  copy overhead) plus a ~10.6KB thumbnail, all `200 OK`, both parts stored under a
  shared timestamp. Decoded the video part with `decode_raw_capture.py` (wrapped in the
  `RAW EXPORT BEGIN <n>` marker it expects, since that script was written for a serial
  log, not a bare file -- no script changes needed) -- all 70 frames extracted cleanly,
  no truncation warnings, assembled into a real H.264 `.mp4` via `ffmpeg` (confirmed via
  `ffprobe`: 640x480, 70 frames, ~5.04s) and one frame opened and visually confirmed as
  a real photo, not corrupt data. Test uploads and decoded artifacts were deleted
  afterward.
- `firmware/Cargo.toml` — `[[bin]]` entries added for both new binaries.

Both new firmware binaries are standalone test binaries (matching this project's
established pattern of proving each new subsystem in isolation before wiring it into
`main.rs`) -- `main.rs` itself is untouched; replacing its wired USB delivery path with
WiFi is still a separate, later step (see Next steps).

### Post-review hardening before `main.rs` integration (envelope bumped to v2)

A review of Milestone 14 (before it touched `main.rs`) found four real gaps -- fixed,
tested, and **re-verified on real hardware** (both `event_upload_test.rs` and
`event_upload_video_test.rs` re-flashed and re-run end-to-end, since the wire format
itself changed):

- **Strict parsing, fixed**: a zero-part envelope is now rejected
  (`EnvelopeError::EmptyEvent`) instead of silently accepted, and undeclared trailing
  bytes after the last declared part are now rejected too (`EnvelopeError::TrailingBytes`)
  instead of silently ignored -- `PartsIter` reports this the moment all declared parts
  are consumed but bytes remain. Both are covered by new `shared` unit tests.
- **Transactional storage, fixed**: `store_event()` now parses (and thus fully
  validates) every part *before* writing anything -- a malformed part N can no longer
  leave parts 1..N-1 on disk as an orphaned partial event, since nothing is written
  until the whole envelope has already parsed successfully. Storage itself
  (`commit_part_file()`, replacing `create_event_part_file()`) now writes to a private
  temp file first, then commits to the collision-safe final name via `hard_link` rather
  than `rename` -- `rename` silently replaces an existing destination on POSIX, so it
  can't actually detect a collision on the final name once an earlier event's temp file
  has been cleaned up; `hard_link` fails instead, which is what the retry-on-collision
  logic actually needs. If a part still fails to store after parsing succeeded (real
  disk/permission failure), every part already committed for that event is deleted
  before the error is returned, so no partial event is ever left visible under real
  filenames. Verified with a new test that makes the uploads directory read-only
  mid-test and confirms zero files remain afterward.
- **Richer part metadata, envelope bumped to v2**: added a firmware-assigned `event_id`
  (`u64`, a per-boot-session monotonic counter for now -- no RTC on this board) to the
  envelope header, and `encoding`/`timestamp_ms`/`duration_ms` to each part header
  (`PART_HEADER_LEN` 5 -> 14 bytes). `Encoding` is a closed enum like `PartKind`
  (`Jpeg`, `RecorderFrames`, `Pcm16Mono` reserved for the future audio part) --
  `timestamp_ms`/`duration_ms` let the video part declare its own start-offset and
  length relative to the event without a downstream consumer having to parse the whole
  frame blob just to find out. This was deliberately done as a version bump (`VERSION`
  1 -> 2, replacing v1 outright) rather than added compatibly, since v1 never shipped
  anywhere outside this session's own test binaries -- no deployed consumer to stay
  compatible with.
- **Correct status codes, fixed**: added `RequestError::StorageFailure(std::io::Error)`,
  mapped to `500 Internal Server Error`, distinct from the pre-existing `Io` variant
  (still `400`, for TCP/connection-level failures). Previously, `store_event`'s
  filesystem calls used the blanket `From<io::Error>` conversion into `Io`, so a real
  disk/permission failure on a perfectly valid request was indistinguishable from a
  malformed request -- both returned `400`. Now a valid event that fails to persist
  returns `500`, telling firmware the request was fine and retrying makes sense.
- `shared` now has 10 unit tests (up from 7), `server` has 10 (up from 7); all pass
  (`cargo test -p shared -p server`).
- **Re-verified end-to-end on hardware** after all of the above: `event_upload_test.rs`
  re-flashed, captured real JPEGs, server logged each part's
  kind/encoding/timestamp_ms/duration_ms and stored a valid `.jpg` (confirmed via
  `file`). `event_upload_video_test.rs` re-flashed, produced 3 events (70 frames each,
  ~2MB clips this time -- a more detailed scene than the first pass, not a regression),
  video part's logged `timestamp_ms`/`duration_ms` matched the real measured
  capture-to-record-start gap (~134-146ms) and recording length (~5043-5044ms); decoded
  one clip with `decode_raw_capture.py` (all 70 frames, no truncation warnings) and
  visually confirmed a real frame. No `.tmp` files left behind after successful
  commits. Test uploads deleted afterward.

### Second review: the "no partial event" guarantee wasn't actually true yet

A follow-up review of the hardening above found the transactional-storage claim didn't
fully hold up -- three real gaps in `commit_part_file`/`store_event`, plus two
lower-priority protocol gaps, all fixed and tested:

- **Temp file leaked on write/sync failure, fixed**: if `write_all` or `sync_all` on
  the temp file failed, `commit_part_file` returned early via `?` without removing the
  temp file it had just created -- a real (if rare) leftover. Now both are attempted
  together and any failure explicitly removes the temp file before returning the error.
- **A committed part could escape rollback, fixed**: after `hard_link` successfully
  exposed a part under its final name, a *subsequent* failure removing the
  now-redundant temp file was propagated as `commit_part_file`'s own error --
  incorrectly, since the event's data was already fully durable and visible under the
  final name at that point. That mistaken error meant `store_event` never added the
  filename to its rollback-tracking list, so a later part's real failure would roll back
  *other* parts but never this one -- an orphaned file outside the rollback mechanism's
  visibility entirely. Fixed: `commit_part_file` now always returns `Ok(final_path)`
  once `hard_link` succeeds; a failure removing the temp file is only logged as a
  warning, never turned into a reported failure.
- **`flush()` doesn't mean durable, fixed**: `std::fs::File::flush()` is a no-op (no
  internal buffering to flush) -- it does not force the OS to write data to disk.
  Replaced with `sync_all()` (an actual `fsync`), called before the payload is ever
  exposed under a final name, so a part is genuinely durable against a crash or power
  loss by the time it's visible, not just "written" in a sense that doesn't survive a
  crash.
- **Real fault-injection tests for first/second-part failure, added**: the previous
  rollback test could only force a failure *before* the first part was ever written
  (a real directory-wide permission failure), so it never actually exercised "an
  earlier part already committed, a later one fails, the earlier one gets rolled back."
  Fixed by extracting `store_event_with_committer` (the real `store_event` delegates to
  it, passing `commit_part_file`) so tests can inject a fake committer -- one new test
  makes the *first* part's commit fail (asserts nothing gets written), another makes the
  *second* part's commit fail after letting the first genuinely commit (asserts the
  first part's real file gets rolled back). The original real-permission test is kept
  alongside these, since it exercises genuine OS-level I/O failure rather than a
  simulated one.
- **Kind/encoding mismatch, fixed (lower priority, done anyway)**: `Thumbnail` +
  `Pcm16Mono` (individually valid values, nonsensical pairing) was previously accepted
  and would have been stored and trusted downstream. Added
  `EnvelopeError::InvalidEncodingForKind`, checked against an explicit whitelist of
  currently-valid `(kind, encoding)` pairs in `shared`'s `PartsIter` -- a whitelist
  rather than a strict 1:1 mapping, so a kind can gain a second valid encoding later
  (e.g. a real video container) without another wire version bump.
- **`event_id` reuse across reboots (lower priority, not fixed)**: still a per-boot
  monotonic counter, so it can repeat after a reboot and isn't used for deduplication.
  Now at least logged per event (`event_id=N` in the `store_event` part log) for
  traceability. Real dedup/idempotency is deferred to the retry-semantics work in Next
  steps, where it's actually needed.
- `shared` now has 11 unit tests (up from 10), `server` has 12 (up from 10); all pass
  (`cargo test -p shared -p server`).
- **Live-verified against the real running server binary** (not just `cargo test`): a
  valid v2 envelope -> `200 OK`, stored and readable; a `Thumbnail`+`Pcm16Mono`
  mismatch -> `400 Bad Request`, logged as `InvalidEncodingForKind`. This round's fixes
  don't change the wire byte layout (only add server-side validation of already-valid
  field values), and real firmware never sends an invalid pairing, so a full hardware
  reflash wasn't required this time -- unlike the v1->v2 bump above, which changed every
  message's byte layout and did need it. `firmware` was still rebuilt (`cargo build
  --release`) to confirm it compiles clean against the updated `shared` crate. Test
  artifacts deleted afterward.

**`main.rs` integration is still the next step, not yet started** -- both hardening
passes above were explicitly prerequisites for it, not the integration itself.

## Milestone 15 — `main.rs` uploads real motion events over WiFi (verified on hardware)

Milestone 14 (both hardening passes) was committed first as a safe checkpoint
(`65940e9`), then `main.rs` itself was wired up: the real PIR-triggered doorbell event
state machine from Milestones 9-10 now uploads its thumbnail + video over WiFi using
the same envelope and request shape `event_upload_video_test.rs` proved, instead of
that logic living only in a standalone test binary.

- **WiFi connect/DHCP added to `main.rs`** at boot, right after camera init (same
  block-until-connected pattern as every WiFi test binary) -- blocks startup until WiFi
  is up, since there is no offline/degraded-start mode yet (deliberately deferred, see
  Next steps).
- **New `upload_event()` async function** builds the two-part envelope (`Thumbnail` +
  `Video`, event ID = the existing per-boot `recording_count`) and sends it over one TCP
  connection, returning an `UploadOutcome` enum (`Success` / `Rejected(status_code)` /
  `ConnectFailed` / `IoError`) instead of just logging a raw response string -- this is
  the actual "distinguish success from failure" logic, not just a human squinting at
  printed bytes. Required adding a targeted, justified `#[allow(clippy::large_stack_frames)]`
  (`main.rs` denies this lint crate-wide) since the request-header string plus the
  512-byte response buffer push the function over clippy's default threshold; the same
  shape was already proven fine on hardware in `event_upload_video_test.rs`.
- **Called right after each event finishes recording**, before the existing USB raw
  export -- logs one clear `WiFi upload SUCCEEDED` / `FAILED -- <reason>` line, then USB
  export always runs regardless of that outcome. No retry logic yet -- a single attempt,
  exactly as scoped; retries/offline handling are next.
- **`RECORDING_ENABLED` flipped to `true`** (was `false`, the dev-convenience toggle from
  Milestone 10) -- needed to actually trigger real events for this test; left `true`
  since the project has moved from "developing near the board" into real reliability
  testing. Flip back to `false` and reflash if that convenience is needed again.
- **Verified on real hardware with a real failed-server scenario**, not just the happy
  path -- 4 consecutive real motion events (hand-waved in front of the PIR sensor):
  - Event #1 (91-110 frames, ~7.3-8.2s each depending on how long motion lasted, matching
    the doorbell state machine's variable duration, not a fixed test-binary duration) and
    #2: server running -> **WiFi upload SUCCEEDED**, both parts stored server-side and
    confirmed valid (thumbnail opens as a real JPEG -- visually confirmed, a real photo
    of a ceiling; video decodes cleanly via `decode_raw_capture.py`, 55/91/110 frames
    depending on event, no truncation warnings).
  - Server killed, event #3 triggered: **WiFi upload FAILED -- could not connect to
    server**, logged cleanly, no crash/hang -- USB export still ran (confirmed
    `RAW EXPORT BEGIN`/`END` present in the serial log right after the failure line) --
    and the state machine still rearmed normally afterward.
  - Server restarted, event #4 triggered: **WiFi upload SUCCEEDED** again, with zero
    intervention beyond restarting the server -- confirms the single-attempt path
    recovers on its own once connectivity returns, exactly as expected before any retry
    logic exists.
  - One stray upload from the *previous* firmware (`event_upload_video_test.rs`, still
    mid-retry-loop during the reflash window) landed on the fresh server with a
    telltale fixed ~5s duration -- correctly identified as test-session noise, not a
    `main.rs` bug, and discarded.
  - All test uploads and artifacts deleted afterward; real production uploads from this
    session were not kept.
- `cargo build`/`cargo clippy --bins` clean across the whole firmware workspace
  (`--all-targets` still fails for unrelated, pre-existing reasons -- `no_std` binaries
  can't build a `test` harness -- not something this change touched).

**Not yet done, explicitly next**: upload retries / offline handling (only after this
single-attempt path is proven reliable, which the above hardware test was the first
real pass at) -- see Next steps. Battery work follows that.

## Milestone 16 — reliability hardening: timeouts, degraded boot, reconnect, dedup, retries (verified on hardware)

Milestone 15 was committed first (`d6cd733`), then a review found seven real reliability
gaps in it -- all fixed, tested, and verified on real hardware (not just re-built):

- **Bounded upload timeouts, fixed**: `upload_event()` had no timeout at all -- a
  stalled server (accepting the connection, then never reading or never responding)
  could leave it (and the whole motion event loop behind it) waiting forever. Added
  `CONNECT_TIMEOUT` (5s) around the TCP connect and `TRANSFER_TIMEOUT` (20s) around the
  whole send-then-read-response step, via `embassy_time::with_timeout`. A new
  `UploadOutcome::TimedOut` variant reports this distinctly from a clean connection
  refusal or a mid-transfer I/O error.
- **Degraded startup, fixed**: boot used to block on `connect_async()` in a loop and
  then on `stack.wait_config_up()` -- during a WiFi outage, the board would never reach
  the PIR motion loop at all, meaning it couldn't even record locally. Replaced with a
  new `wifi_maintain_task` background task (connects, waits for a disconnect, reconnects,
  forever) spawned once and never awaited by `main()`; boot proceeds straight to camera/
  PIR setup regardless of whether WiFi has connected yet.
- **WiFi reconnection after a later disconnect, fixed**: the same `wifi_maintain_task`
  handles this uniformly -- `WifiController::wait_for_disconnect_async()` blocks until a
  real disconnect event, then the loop falls through to `connect_async()` again, exactly
  like the initial connect. One task covers both "never connected yet" and "was
  connected, then dropped."
- **Empty-event upload, fixed**: if every single capture attempt in an event failed (PIR
  fired but the camera never produced a frame), `main.rs` used to upload a zero-length
  thumbnail and video anyway and get back a real `200`. Fixed at two layers: `main.rs`
  now checks `recorder.frame_count() == 0` and skips the upload entirely (USB export
  still runs, for completeness, showing 0 bytes); `shared` independently now rejects any
  zero-length part (`EnvelopeError::EmptyPart`) regardless of which client sends one, so
  the server doesn't have to trust firmware to check this itself.
- **Safe retries + reboot-safe event identity + server-side dedup, added**: `main.rs`
  now generates a per-boot random `boot_nonce` (`esp_hal::rng::Rng`) and combines it with
  the recording counter for `event_id` (`(boot_nonce << 32) | recording_count`), so IDs
  don't collide with a previous boot's counter values. A new
  `upload_event_with_retries()` wraps `upload_event()` in a bounded loop
  (`MAX_UPLOAD_ATTEMPTS = 3`, 3s fixed backoff), retrying `ConnectFailed`/`TimedOut`/
  `IoError`/server `5xx` (transient) but not `4xx` (retrying an identical malformed
  request won't help). The server gained a matching `EventDedup` (a bounded/FIFO ring of
  recently seen `event_id`s) checked in `store_event_with_committer()` right after
  parsing the envelope header: a duplicate `event_id` is treated as an already-successful
  retry and returns `200` without writing anything a second time, which is what actually
  makes retries safe against the "server stored it, but firmware's read timed out before
  seeing the response" race. 2 new `server` unit tests cover the dedup logic directly
  (detects repeats, evicts oldest past capacity) and end-to-end (`store_event` called
  twice with the same `event_id` stores once).
- **Real warm-up offset preserved, fixed**: the video/thumbnail parts' `timestamp_ms` was
  hardcoded to `0` in `main.rs`, even though capturing the first frame (shared by both
  parts) includes the camera's warm-up delay -- so the true offset from `event_start` is
  never actually zero. Now measured (`part_timestamp_ms`, captured the moment the first
  frame succeeds) and passed to both parts' headers; `video_duration_ms` is now
  `event_duration_ms - part_timestamp_ms` (the video's own content span, not including
  the warm-up gap before it started). This is what a future audio part would need to
  line up against the video accurately.
- **Stale doc comment, fixed**: `main.rs`'s module doc said "a further 2s tail" -- the
  code has used `TAIL_DURATION = 5s` since Milestone 10; the doc comment just never got
  updated then. Fixed, and the doc comment expanded to describe the background WiFi
  supervisor and retry behavior added in this milestone.
- `shared` now has 12 unit tests (up from 11), `server` has 14 (up from 12); all pass
  (`cargo test -p shared -p server`, 26 total).
- **Verified on real hardware**, including scenarios specifically exercising the new
  behavior, not just re-confirming what Milestone 15 already proved:
  - **Reconnect-after-disconnect**: temporarily added a one-shot test scaffold to
    `wifi_maintain_task` (forces a real `disconnect_async()` 15s after first connecting,
    reverted before this was committed) -- confirmed the board reconnected on its own and
    a subsequent real motion event uploaded successfully afterward (`event_id` and a
    nonzero `part_timestamp_ms=139` visible in the server log), proving the reconnect
    path is exercised by a genuine live disconnect, not just at boot.
  - **Degraded boot**: temporarily set a wrong WiFi password (in the gitignored
    `wifi_credentials.rs`, reverted after), reflashed, and confirmed a real motion event
    still recorded fully (49 frames) and ran USB export even though WiFi never connected
    -- the serial log shows the retry loop's real behavior end-to-end: attempt 1
    (`NoRoute`) -> retryable -> wait 3s -> attempt 2 (`NoRoute`) -> wait 3s -> attempt 3
    (`NoRoute`) -> attempts exhausted -> `WiFi upload FAILED -- could not connect to
    server`, while `wifi_maintain_task` kept independently retrying the real connection
    in the background (visible: a genuine `FourWayHandshakeTimeout` from the wrong
    password) without blocking any of it. The board rearmed and kept running normally
    afterward. Credentials restored and the clean (non-scaffolded) firmware reflashed
    before the rest of this testing.
  - **Normal operation post-fix**: multiple real motion events with the clean firmware
    uploaded successfully, with `timestamp_ms` now consistently nonzero and matching the
    real measured warm-up gap (as low as ~116ms, as high as ~7.6s on one event where
    several early capture attempts failed first -- an honest reflection of real
    variability, not a bug).
  - **Mid-retry-cycle recovery**: attempted to kill and restart the server precisely
    within a single event's retry window to catch attempt 2 or 3 succeeding after
    attempt 1 failed; live human-triggered motion timing wasn't precise enough to land
    exactly in that window across several tries (the server was either still down for
    all 3 attempts, matching the degraded-boot result above, or already back up before
    attempt 1). This exact sub-scenario is therefore verified by combining the two
    results that were each independently confirmed on hardware -- the retry loop's
    mechanics (degraded-boot test) and a fresh upload succeeding once connectivity is
    back (every other test here) -- rather than by one single test pinning down that
    precise timing window.
  - All test uploads and artifacts deleted afterward.
- `cargo build`/`cargo clippy --bins` clean across the firmware workspace.

### Critical fix: dedup was recording event_id before storage actually succeeded

A review of Milestone 16 found the retry-safety mechanism it just added had a real
data-loss bug of its own: `EventDedup` recorded `event_id` as seen immediately after
parsing the envelope header -- *before* the parts were validated or anything was
written to disk. Sequence that broke: (1) an attempt reaches the server, (2) envelope
validation or storage fails (`400`/`500`), (3) but `event_id` is already marked seen,
(4) firmware retries the same `event_id` per Milestone 16's design, (5) the server now
treats it as an already-successful duplicate and returns `200` without storing
anything -- the exact opposite of what retries were added for. A failed attempt was
silently and permanently unretryable.

- **Fixed**: split `EventDedup::check_and_record` into `is_duplicate` (read-only,
  called early to skip redundant work for a genuine duplicate) and `record` (mutating,
  now called only once `store_event_with_committer` has committed every part
  successfully -- the last line before returning `Ok`). A request that fails
  envelope/part validation or fails to store no longer burns its `event_id`.
- **3 new tests** (`server` now has 17, up from 14): `store_event_allows_retry_after_a_
  storage_failure` (fake committer fails every part, confirms `event_id` isn't marked
  seen, then a real retry with the same ID stores correctly), `store_event_allows_retry_
  after_invalid_payload_with_same_event_id` (a truncated part fails validation, confirms
  `event_id` isn't marked seen, then a corrected body with the same ID stores
  correctly), and `event_dedup_does_not_record_until_told_to` (checking `is_duplicate`
  repeatedly never itself records anything).
- **Verified the tests actually catch the bug**: temporarily reintroduced the old
  behavior (recording immediately after the duplicate check, before validation/storage),
  confirmed both new `store_event_allows_retry_after_*` tests fail against it, then
  restored the fix and confirmed all 17 tests pass again.
- **Known limitation, intentionally not fixed here**: `EventDedup` is in-memory only --
  a server restart forgets every recorded `event_id`, so retry-dedup is not restart-safe
  (a retry arriving right after a server restart would be stored a second time under a
  new receipt timestamp, same as before Milestone 16 existed). This is explicitly
  deferred to accompany the future persistent offline queue work (see Next steps), which
  will need durable event identity on both sides anyway -- adding a partial persistence
  layer just for dedup now would be thrown away/reworked once that lands.

## Milestone 17 — firmware migrated to the SDMMC-capable esp-hal fork; combined camera+PIR+LED+WiFi+SD hardware regression passes

Followed the user's explicit corrected order after Milestone 16/`sdmmc_test/`: commit
the isolated smoke test, migrate `firmware/`'s real dependencies to the same pinned
revision, build every firmware binary, then hardware-regression-test camera+PIR+LED+WiFi
with SD initialized simultaneously *before* touching the actual queue logic.

- **`sdmmc_test/` committed** (`9292db7`) as the isolated hardware proof, unchanged from
  the "Result: the onboard slot works" section above.
- **`firmware/Cargo.toml` migrated** to the same pinned fork/revision
  (`bugadani/esp-hal` @ `cef6d86604d91abcf62afc9804724a637eb7af3a`) for `esp-hal`,
  `esp-println`, `esp-rtos`, `esp-bootloader-esp-idf`; added the SD/FAT dependencies
  (`sdio`, `embedded-fatfs`/`embedded-partitions` from the `MabezDev/embedded-fatfs` fork,
  `block-device-adapters`) and a `[patch.crates-io]` section, matching `sdmmc_test/`
  exactly. This forced three successive dependency-graph fixes, each root-caused against
  real error output before patching (not guessed):
  1. **`links` conflicts** (`xtensa-lx-rt`, `esp-rom-sys`): `esp-radio 0.18.0` pulls the
     released crates-io copies of these, which collide with the git-pinned esp-hal's own
     copies of the same native-linked crates. Fixed by patching both to the same fork
     revision.
  2. **Two different esp-hal versions resolving simultaneously** (1.1.1 crates-io +
     1.1.0 git, the crates-io copy then panicking "unstable feature required but not
     enabled"): root-caused to `esp-radio 0.18.0`'s manifest requiring `esp-hal
     ~1.1.0-rc.0`, a pre-release constraint that `[patch.crates-io]`'s plain `1.1.0`
     can't satisfy, forcing Cargo to resolve a second, unpatched copy just for that one
     dependent.
  3. Fixing #2 required bumping `esp-radio` itself to the fork's own version
     (`1.0.0-beta.0`, a real, if narrow, API break from the released `0.18.0`) — paused
     here and explicitly asked the user before proceeding, since this was no longer a
     transparent dependency-graph fix. Investigated the diff first (approved via
     "Investigate the API diff first"), then migrated on "just go on."
- **`esp_radio::wifi` API migration across 6 files** (`main.rs`, `wifi_test.rs`,
  `wifi_scan_test.rs`, `http_post_test.rs`, `event_upload_test.rs`,
  `event_upload_video_test.rs`): the free function `wifi::new(device, config) ->
  (WifiController, Interfaces)` was replaced by `WifiController::new(device, config) ->
  Self` + `Interface::station()`; `Interface` lost its `'static` lifetime parameter
  (`WifiController<'d>` kept its own). Mechanical, per-file, each verified compiling
  before moving to the next.
- **Severe LLVM backend crash found and fixed, affecting the real production binary, not
  just test binaries**: every camera/DMA_CH0-using binary (including `firmware` itself)
  failed to build under the patched fork with `Cannot select: XtensaISD::PCREL_WRAPPER`
  against `DMA_CH0::info::INFO`/the interrupt-handler symbols, but only under
  `lto = 'fat'`. Fixed by changing `[profile.release]` to `lto = 'thin'` in
  `firmware/Cargo.toml` — verified across all 14 binaries (`cargo build --release
  --bins` and `cargo clippy --release --bins`, both clean).
- **New `firmware/src/bin/sd_regression_test.rs`**: the actual gate this milestone exists
  to pass — camera, PIR/LED, WiFi, and the onboard SD slot all initialized and running
  simultaneously (undebounced PIR → capture-and-log on one path via `embassy_futures::
  select`, BOOT button → the same FAT CRUD cycle as `sdmmc_test.rs` on the other), plus a
  10s heartbeat.
- **Real bug found and fixed during this test: `capture_jpeg` could only ever succeed
  once per boot under the forked esp-hal.** `camera.rs`'s `capture_jpeg_with_warmup`
  called `esp_hal::dma_rx_stream_buffer!(20 * 1000, 1000)` fresh on every capture. That
  macro expands to a `static DESCRIPTORS: ConstStaticCell<...>` declared at the call
  site, consumed via `.take()` — a pattern meant for one static allocation for the
  program's entire lifetime, not a fresh call every capture. First capture's `.take()`
  succeeds; every capture after that panics `` `ConstStaticCell` is already taken, it
  can't be taken twice `` at the exact same source line, since it's the same static
  instance. Root-caused by reading the macro's real expansion in the fork's
  `esp-hal/src/dma/mod.rs` (`dma_buffers_impl!`/`dma_descriptors_impl!`), not guessed.
  This was latent in *every* binary that calls `capture_jpeg` more than once per boot
  (including the real `main.rs`) — it only surfaced now because `sd_regression_test.rs`
  was the first binary actually re-run twice on hardware since the fork migration.
  **Fixed**: `CameraHandle` now allocates the `DmaRxStreamBuf` exactly once in `new()`
  and stores it as a field (`dma_rx_buf: Option<DmaRxStreamBuf>`), reusing the same
  `Option::take()`/put-back pattern already used for `camera: Option<Camera<'d>>` —
  taken out at the start of `capture_jpeg_with_warmup`, put back on every exit path
  (`Camera::receive` failure, `DmaFinishedBeforeEof`, and the normal end of a successful
  capture), since `CameraTransfer::stop()` returns the same `DmaRxStreamBuf` back
  (`BUF::Final = DmaRxStreamBuf` in the fork's `DmaRxBuffer` impl).
- **Verified on real hardware, all subsystems simultaneously active**: after reflashing
  the fixed binary, three consecutive PIR-triggered captures all succeeded (15771,
  17865 bytes, plus the first at 15187 bytes) with camera+PIR/LED+WiFi+SD all running —
  no panic on the second or third capture, confirming the fix and not a fluke. The BOOT
  button's SD path also passed: `async FAT CRUD: PASS`, the same real create/read/
  update/read/delete cycle against the physical card, now proven while WiFi and the
  camera are both live too, not in isolation.
- **Step 5 of the user's corrected order (camera+PIR+LED+WiFi with SD initialized
  simultaneously) is now genuinely passed.** Nothing in `main.rs` has been touched yet —
  this milestone only proved the dependency migration and the SD slot are safe to build
  on top of; the actual offline-queue logic (steps 6-7) has not started.
- Board-history note: mid-testing, a stuck-bootloader/download-mode episode (this
  project's known USB-Serial/JTAG quirk) was resolved by reflashing after confirming via
  `espflash board-info` that the chip itself was reachable; separately, the board was
  fully unplugged overnight and reconnected the next day between test sessions, which
  explains an otherwise-confusing port-number change (`/dev/cu.usbmodem101` ->
  `/dev/cu.usbmodem2101`) — not a new hardware fault.

## Milestone 18 — SD-backed offline event queue + persistent server-side dedup

Step 6-7 of the user's corrected order, built directly on Milestone 17's proven
migration and hardware-verified SD slot. Goal: an event that exhausts
`upload_event_with_retries`' in-memory attempts is no longer simply lost once its
PSRAM buffers get reused by the next motion event.

- **New `firmware/src/queue.rs`** (added to the `firmware` lib crate's public
  modules): `save_event` writes an event's exact wire-format envelope bytes (the
  same header+parts `upload_event` would have sent) to a new SD file, and
  `drain_queue` uploads every currently-queued file to the server, deleting each
  one only once a `200` confirms delivery. Both dispatch over
  `embedded_partitions::mbr::Scheme::open` the same way `sd_regression_test.rs`'s
  FAT CRUD test already does, generic over `IO: ReadWriteSeek`, so either an
  MBR-partitioned card or a bare superfloppy FAT volume (this board's own 1GB
  card) works without hardcoding one layout.
  - **Save**: written to a fixed temp name (`EVT_TMP.BIN`) first, flushed, then
    `Dir::rename`d to its final `EVT_<event_id as 16 hex digits>.BIN` name --
    exactly the write-then-atomically-commit pattern the server's own
    `commit_part_file` already uses, so a power loss mid-write can never leave a
    half-written file visible to the scan/retry pass (it's still under the temp
    name, or doesn't exist yet). Long filenames needed enabling
    `embedded-fatfs`'s `alloc`/`lfn` features (previously `default-features =
    false` with neither on) -- confirmed via the crate's own source that LFN
    directory entries are written, not just read, so this isn't a read-only
    convenience.
  - **Drain**: lists queued files first (name+length), *then* processes them --
    never mutates the directory while an iterator over it is still live. Each
    file's bytes are streamed straight from SD into the existing 64KB scratch
    buffer and out to the socket in chunks (`Content-Length` set from the
    file's on-disk length), never buffered whole in RAM -- the same reasoning
    that already keeps PSRAM video clips from ever needing one giant in-memory
    copy. A file is deleted on `200`, dropped (not kept) on an unrecoverable
    `4xx` rejection (matching `UploadOutcome::is_retryable()`'s existing
    philosophy -- retrying an identical malformed request forever would
    otherwise block every file queued behind it), and left in place with the
    whole pass stopped early on any transient failure (no connection, timeout,
    I/O error, or `5xx`), on the assumption the server/WiFi is still down and
    hammering through the rest of the queue this pass won't help.
- **`firmware/src/bin/main.rs`** now initializes the SD card at boot (same proven
  `SdHostController`/`slot::<1>`/`BlockDevice::new_sd_card` sequence as
  `sd_regression_test.rs`), but as `Option<BlockDevice<...>>` rather than halting
  on failure -- unlike the regression test (whose entire purpose is proving the
  slot works), production firmware must keep camera/PIR/WiFi working even if the
  card is missing or fails to init; the queue is just disabled for that boot.
  Wired into the event loop: after `upload_event_with_retries` returns,
  `UploadOutcome::Success` triggers a best-effort `queue::drain_queue` (since
  connectivity's just been proven), and any retryable non-success
  (`is_retryable()`, i.e. not a `4xx` rejection) triggers `queue::save_event`.
  Also attempts one best-effort `queue::drain_queue` right at boot, before the
  first motion event, to catch up on a backlog left over from a previous
  outage/reboot without waiting for new motion (WiFi may not have connected yet
  this early since it connects in the background -- this attempt just fails
  fast in that case, and normal per-event draining picks up the backlog once
  connectivity exists).
- **`firmware/Cargo.toml`**: added `block-device-driver` as a direct dependency
  (previously only pulled in transitively, so `[patch.crates-io]` had nothing at
  the top level to redirect for `queue.rs`'s own `use` of it) and enabled
  `embedded-fatfs`'s `alloc`/`lfn` features.
- **`server/src/main.rs`**: `EventDedup` gained `new_with_persistence(capacity,
  path)`, loading previously recorded event IDs from a small file (one decimal
  number per line) if present, and rewriting that file (temp-file-then-rename,
  the same crash-safe pattern as `commit_part_file`) on every `record()` --
  closing the exact gap Milestone 16 flagged as a known limitation (dedup
  forgetting everything on a server restart), which the SD queue now makes a
  real, not just theoretical, scenario: a queued event can plausibly be replayed
  well after the server that already stored it has since restarted. Plain
  `new(capacity)` (in-memory only) is kept but now `#[cfg(test)]`-gated, since
  production code (`main()`) always goes through the persistent constructor.
  The dedup file lives at `server/event_dedup.log` (sibling to `uploads/`, added
  to `.gitignore`), bounded to `EVENT_DEDUP_CAPACITY` entries same as before --
  a full rewrite per record rather than an ever-growing append log, since the
  bounded ring is only a few KB even at capacity.
- **4 new `server` tests** (21 total, up from 17): a simulated restart (two
  `EventDedup` instances pointed at the same path) confirms a recorded ID
  survives; starting with no existing file behaves like starting empty;
  capacity eviction is confirmed to persist correctly across a restart (an
  evicted ID doesn't reappear after reloading); a corrupted line in the file
  is skipped rather than failing startup.
- **Verified**: `cargo build --release --bins` and `cargo clippy --release
  --bins` clean across all 14 firmware binaries (`queue.rs` is a new lib
  module, not a new binary); `cargo test -p server -p shared` -- 33 tests
  total (21 server + 12 shared), all passing.
- **Verified on real hardware, the actual server-outage scenario, end to end**:
  flashed the real `firmware` binary (not a test binary), stopped the Mac
  server, triggered a real motion event -- recorded fine (77 frames, 1.48MB),
  all 3 live upload attempts correctly failed (`ConnectFailed`, server down),
  and the event was saved to the SD card (`event #1: queued to SD`). Server
  restarted. A second real motion event recorded and uploaded successfully
  live (`event #2: WiFi upload SUCCEEDED`), which triggered a drain that found
  the queued file from event #1, delivered it, and removed it from the card
  (`queue: EVT_AB1073F900000001.BIN delivered, removing from queue` ->
  `Drained { uploaded: 1, dropped: 0 }`). Cross-checked independently against
  the server's own log: both events (`event_id` ending `...497` and `...498`)
  show up stored with their own thumbnail+video files, not just one. Test
  upload artifacts deleted afterward, per this project's established
  practice. **Milestone 18 is complete.**

## Milestone 19 — server-side AI thumbnail analysis via local Ollama, with a future-ready identity slot

Built on Codex's research (`qwen3.5:4b` via Ollama: Apache-2.0 weights, multimodal,
~3.4GB local build, free to run, keeps images on the user's own network -- see
https://huggingface.co/Qwen/Qwen3.5-4B / https://ollama.com/library/qwen3.5). Scoped
entirely to the `server` crate per explicit instruction -- no firmware changes; the
JPEG thumbnail firmware already sends in every event envelope needed nothing new.

- **New `server/src/ai.rs`**: `EventAnalysis` (`person`/`package`/`vehicle`/`animal`
  bools, `description: String`, `importance: Importance` (`Low`/`Medium`/`High`,
  serialized lowercase)) is parsed strictly from the model's JSON output -- an
  unexpected shape is a failure, never silently patched. `ThumbnailAnalyzer` trait
  (`analyze`/`provider`/`model`) is the swappable boundary: `OllamaAnalyzer` for real
  use, a `FakeAnalyzer` in tests so none of them require Ollama running.
  `OllamaAnalyzer::from_env` reads `OLLAMA_URL`/`OLLAMA_MODEL`, defaulting to
  `http://localhost:11434`/`qwen3.5:4b` -- constructing it never talks to Ollama, so
  server startup never depends on Ollama being up.
- **`identity` block, deliberately inert**: always `{"status": "not_enabled",
  "known_person_id": null, "display_name": null, "confidence": null}`. No face
  recognition, no embeddings, no people database, and the vision model is never asked
  to identify anyone -- the block exists purely so a future local
  known-person-recognition module can be plugged in without another pipeline change.
- **Saved `analysis.json`** (`{event_analysis, identity, ai}`, `event_analysis: null`
  on any failure) is written beside the event's other files via the same
  temp-file-then-rename pattern `commit_part_file` uses, keyed off the thumbnail's own
  filename (`event_<timestamp>_thumbnail.jpg` -> `event_<timestamp>_analysis.json`) --
  no changes needed to `store_event`/`commit_part_file`/`part_file_naming` at all.
- **Analysis runs strictly after the firmware upload's `200` response is already
  flushed, on a detached background thread -- a real bug found and fixed during
  testing, not just a design choice.** This server's accept loop is single-threaded
  and fully synchronous (`for stream in listener.incoming() { handle_connection(...) }`,
  no thread-per-connection). An early version called `analyze_and_save` in-line inside
  `process_request` after the response was written; a real end-to-end test (see below)
  showed the TCP connection was still held open for the full Ollama call, and -- more
  seriously -- the entire server couldn't accept a *second* connection until the first
  event's analysis finished. Fixed by wrapping the analyzer in `Arc<dyn
  ThumbnailAnalyzer>` (trait now requires `Send + Sync`) and spawning a detached
  `std::thread::spawn` per event for the analysis call; `process_request` returns
  immediately once the response is flushed. Verified with a real Ollama call: an
  upload now completes in ~50ms regardless of Ollama, and a second event can be
  uploaded and accepted immediately while the first's analysis is still running.
- **Real bug found and fixed against the actual running Ollama instance**:
  `qwen3.5:4b` is a hybrid-reasoning ("thinking") model. With `format: "json"` and no
  other setting, its entire answer -- confirmed by direct inspection -- lands in
  Ollama's separate `thinking` response field, leaving the `response` field (the one
  this code parses) empty, which then fails strict JSON parsing every time. Root
  cause confirmed directly (raw `/api/generate` calls against the real local Ollama
  instance) before fixing, not guessed. Fixed by adding `"think": false` to the
  request body -- verified this both populates `response` correctly *and* is faster
  (~4-12s vs ~10-19s, since the model skips its chain-of-thought pass).
- **6 new tests** in `ai.rs` using `FakeAnalyzer` (server now 27, up from 21): saves
  the success shape correctly (including `identity.status`); saves the failure shape
  without panicking when the analyzer itself errors; saves the failure shape when the
  thumbnail file is missing; a markdown-code-fenced model response still parses (a
  known, harmless VLM quirk -- tolerated defensively); a response missing required
  fields is rejected; `Identity::not_enabled()` is asserted directly.
- **Verified end-to-end against the real running Ollama instance** (not just fakes):
  built a raw envelope-format POST client to exercise the real server binary
  (`cargo test` alone can't reach a real Ollama process). Confirmed, in order: (1) a
  real event with a real JPEG thumbnail produces a correctly-shaped `analysis.json`
  with genuine model output and `ai.status = "success"`; (2) pointing `OLLAMA_URL` at
  an unreachable port still returns `200` to the client in ~50ms, with
  `analysis.json` saved as `ai.status = "failed"`, `event_analysis: null`,
  `identity.status` still `"not_enabled"` -- Ollama being offline never touches
  upload success; (3) the real prompt's privacy rules hold against the real model
  (tested directly against `/api/generate`): no identification, no race/age/etc.
  description, never called a "selfie", output stayed neutral and security-focused;
  (4) a real motion event from the actual board (still running from Milestone 18's
  hardware test) was independently observed going through the same pipeline
  correctly while this was being tested. Test artifacts (the synthetic events, not
  the real board's) cleaned up afterward, per this project's established practice;
  a handful of harmless test `event_id`s remain in the bounded/FIFO
  `event_dedup.log` (never collide with real event IDs, age out naturally).
- **User live-tested repeated real `analysis.json` outputs from `server/uploads/`**:
  several fresh events were inspected with
  `cat "$(ls -t server/uploads/*_analysis.json | head -1)"`. Results matched the
  intended production shape every time: `event_analysis` was populated,
  `identity.status` stayed `"not_enabled"` with all identity fields `null`, and
  `ai.provider = "ollama"`, `ai.model = "qwen3.5:4b"`, `ai.status = "success"`.
  The model correctly produced different practical classifications across real live
  samples: package-like close-up label/item (`package: true`, `importance: medium`);
  multiple person/close-to-camera cases (`person: true`, usually
  `importance: high`); and a dark/blurred no-subject frame (`person/package/vehicle/animal:
  false`, `importance: low`). This confirms the AI integration is not only a synthetic
  test harness result -- it is producing saved analysis files from the live camera
  event flow.
- `cargo test -p server` -- 27 tests, all passing. `cargo clippy -p server` clean.
  Firmware entirely untouched (`git diff --stat -- firmware/` empty) -- confirmed
  before and after, per the explicit scope instruction.
- **New dependencies** (`server/Cargo.toml` only): `ureq` (with the `json` feature,
  for the Ollama HTTP call), `serde`/`serde_json` (for both the strict Ollama-response
  parsing and the saved `analysis.json`), `base64` (thumbnail encoding).
- **Deliberately not done yet**, per explicit scope: face recognition, bounding
  boxes, video analysis (still thumbnail-only), and no model-downloading code
  anywhere (Ollama/model installation stays a deployment concern, not application
  code).

## Milestone 20 — server-side retention cleanup

Event files under `server/uploads/` were never deleted on their own; this milestone
adds a background sweep that removes complete old event sets after
`EVENT_RETENTION_DAYS` (default 30), so the directory doesn't grow forever on the
Mac/mini-computer. Scoped entirely to the `server` crate -- no firmware changes, and
the ESP32 SD-card offline queue (`firmware::queue`) is completely separate on-device
storage that this never touches.

- **New `server/src/retention.rs`**: `clean_expired_events(dir, retention, now)` scans
  `UPLOADS_DIR`, groups files by their `event_<timestamp>` prefix (the same naming
  `commit_part_file`/`ai::analyze_and_save` already use -- `event_<timestamp>_
  thumbnail.jpg`, `_video.bin`, `_analysis.json`, and any hard-link collision-suffixed
  variant like `_thumbnail_1.jpg`, all grouped under one key), and deletes every file
  in a group together, only once the *most recently modified* file in that group is
  itself older than the retention window.
- **Why "most recently modified", not "oldest"**: this is what makes the sweep safe
  against a set that's still being assembled -- e.g. `analysis.json` can land many
  seconds (or, if Ollama is slow, longer) after the thumbnail/video are already
  committed. Checking the newest member's age means a set is never touched while any
  part of it is fresh, satisfying "don't delete anything still being uploaded or
  written" by construction rather than by a separate special case. Proved directly by
  a test (`never_partially_deletes_a_set_still_gaining_files`) that backdates a
  thumbnail+video past retention but leaves `analysis.json` fresh, and confirms
  nothing is deleted.
- **Deliberately narrow file matching**: `event_key` only recognizes this server's own
  `event_<digits>_...` scheme -- anything else (a stray `.tmp_*` file from an
  interrupted commit, `event_dedup.log`, a user-dropped file, even a name that merely
  starts with `event_` but isn't followed by a pure-digit timestamp) is left
  completely alone, never considered for deletion at all.
- **Runs on its own detached background thread**, started once at boot alongside the
  AI analyzer -- same reasoning as `ai::analyze_and_save`'s background thread: this
  server's accept loop is single-threaded and synchronous, so a sweep (even a cheap
  one) must never be able to delay accepting an upload. Sweeps immediately at startup
  (catches anything that expired while the server was down), then every
  `RETENTION_SWEEP_INTERVAL` (1 hour -- far more granular than a day-scale retention
  window needs, but the sweep itself is just a directory listing + stat calls, so the
  cost is negligible).
- **`EVENT_RETENTION_DAYS` env var**, default `30`, read once at startup via
  `retention::retention_from_env()` -- unset or unparseable falls back to the default
  rather than failing startup.
- **8 new tests** (server now 35, up from 27): deletes a fully-expired set together;
  keeps a set that hasn't aged out; never partially deletes a set still gaining files
  (above); never touches files outside the naming scheme (`README.txt`, a `.tmp_*`
  file, `event_notes.md`, `eventually_something.jpg` -- all survive even when
  backdated 10x past retention); groups a hard-link collision suffix into the same
  set; two different events' expiry are independent of each other; a missing
  `UPLOADS_DIR` is not an error (just nothing to clean up yet); `event_key`'s
  matching logic directly.
- `cargo test -p server` -- 35 tests, all passing. `cargo clippy -p server` clean.
  Firmware entirely untouched (`git diff --stat -- firmware/` empty), confirmed.
- **Real-process smoke-tested, but not long-run aged-out verified yet**: a real
  restart of the running server confirmed the retention sweep thread starts cleanly
  and correctly leaves the current recent upload set alone (137 real event files at
  the time of the test). The actual deletion path is unit-tested with synthetic
  backdated files, but nobody has yet watched it delete naturally aged-out real files
  over a real 30-day server lifetime. Low risk (same file-age-comparison logic either
  way), but flagged here per this project's practice of being explicit about what's
  test-verified vs. long-run/real-data verified.

## Milestone 21 — server-side key-frame extraction from recorded video

Closes the candidate noted after Milestone 20: uploaded video clips were still only
the raw `firmware::recorder::PsramRecorder` wire format (`[frame_len][timestamp_ms]
[jpeg]` repeated) -- viewable only through this project's existing decode/assemble
scripts. Chose key-frame extraction over a full container conversion (the other
option the candidate note offered): no `ffmpeg`/external-process dependency, no new
failure surface from a tool that might not be installed, and a handful of
representative stills is a better fit for "server/AI can consume this" than a
full replayable video -- the explicit motivation given for this milestone. Full
container conversion (MJPEG/AVI or similar) remains available as a later option if
key frames turn out to be insufficient once real video-AI work starts.

- **New `server/src/video.rs`**: `extract_keyframes(video_path)` reads a stored
  `event_<timestamp>_video.bin`, parses every frame with the same `[frame_len: u32
  LE][timestamp_ms: u32 LE][jpeg bytes]` logic `scripts/decode_raw_capture.py`
  already uses for the USB-export path (the network-uploaded `.bin` file is that
  same byte sequence directly, no enclosing marker), and writes up to
  `MAX_KEYFRAMES` (6) representative stills as `event_<timestamp>_keyframe_<n>.jpg`
  beside it -- always including the first and last frame, evenly spaced in between;
  a clip with 6 or fewer frames keeps all of them.
  - **Free interoperability with Milestone 20's retention cleanup, no changes needed
    there**: `retention::event_key` already groups purely by the `event_<timestamp>`
    prefix regardless of what follows, so `_keyframe_<n>.jpg` files are
    automatically swept up together with the rest of an expired event set.
  - **Never touches the original `.bin`** -- only ever adds new files alongside it,
    matching "keep the raw file until conversion is trusted." A path that doesn't
    match the expected `..._video.bin` naming, a missing file, or malformed/truncated
    frame data is logged and simply results in zero keyframes written -- never a
    panic, never anything that could affect the upload.
- **Same detached-background-thread treatment as AI analysis**, wired into
  `process_request` right alongside it: after the response is already flushed, if a
  `Video` part was stored this request, a `std::thread::spawn` reads and processes it
  independently -- this server's accept loop is single-threaded, so extraction (like
  analysis) must never run in-line or it would block the next upload from being
  accepted.
- **9 new tests** (server now 44, up from 35): correct frame parsing; rejects an
  empty buffer and a truncated final frame; keeps every frame when at/under the
  6-frame cap; thins a 50-frame synthetic clip down to 6, keeping the first and last;
  a real end-to-end write test (encodes 3 synthetic frames, calls
  `extract_keyframes`, confirms exactly the right files appear, the original `.bin`
  is byte-for-byte untouched, and a specific keyframe's content matches the source
  frame exactly); writes nothing (no panic) when the source is malformed, missing,
  or doesn't match the expected naming.
- **Verified end-to-end against a real stored clip, not just synthetic tests**: sent
  a real thumbnail + a real, previously-recorded 71-frame/~1.4MB video part (reused
  from an actual earlier firmware upload) through the running release server.
  Response returned in ~20ms regardless of the ~1.4MB video part. Log confirmed
  `video: extracted 6/6 keyframe(s) ... (71 total frame(s), timestamps [112, 2020,
  3281, 4614, 6162, 7387]ms)` -- extraction is running concurrently with (not gating)
  the AI thumbnail analysis, which also completed normally on the same event.
  Confirmed all 6 `_keyframe_N.jpg` files are real, valid 640x480 JPEGs
  (`file` reported `JPEG image data, ... 640x480, components 3` for each), and the
  original `_video.bin` was confirmed byte-identical before/after. Test artifacts
  cleaned up afterward, per this project's established practice.
- `cargo test -p server` -- 44 tests, all passing. `cargo clippy -p server` clean.
  Firmware entirely untouched (`git diff --stat -- firmware/` empty), confirmed.
- **Deliberately not done**: no video AI analysis yet (still thumbnail-only, per
  explicit instruction) -- these key frames exist so that work has something ready
  to consume once it starts, not to trigger it now. No full container/MJPEG-AVI
  conversion either, per the design choice above.

## Milestone 22 — AI analysis now covers the whole event, not just the thumbnail

Extends Milestone 19's AI analysis to also use Milestone 21's extracted video key
frames: one combined `analysis.json` per event, describing the thumbnail and the
clip together, not the thumbnail alone. Still no *raw video* analysis -- Ollama is
never given the `.bin` file, only the already-extracted JPEG stills, and video AI
otherwise remains exactly as deferred as before (no per-frame analysis of a whole
clip, no bounding boxes, no face recognition, `identity.status` still always
`"not_enabled"`).

- **`server/src/ai.rs`**: the analyzer trait -- renamed `ThumbnailAnalyzer` ->
  `EventAnalyzer` since it's no longer analyzing just one image -- changed from
  `analyze(&self, jpeg_bytes: &[u8])` to `analyze(&self, images: &[&[u8]])`.
  `OllamaAnalyzer` now base64-encodes every image into one `/api/generate` request's
  `images` array (thumbnail first, then key frames in order) instead of exactly one.
  `PROMPT` updated to the new event-level wording (explicitly: "treat all provided
  images as one event over time", "the first image is the event thumbnail", "the
  remaining images are keyframes from the recorded video", plus the same
  privacy/security rules as before). `think: false` (the Milestone 19 fix for
  `qwen3.5:4b`'s hybrid-reasoning mode) carries over unchanged.
- **`analyze_and_save` signature grew a `keyframe_paths: &[PathBuf]` parameter.** The
  thumbnail is still required (unreadable thumbnail = hard failure, same as before);
  each key frame is read best-effort and simply skipped (logged, not fatal) if
  unreadable -- an empty or fully-unreadable `keyframe_paths` transparently falls
  back to exactly the thumbnail-only request Milestone 19 already made. There is no
  separate "fallback mode" in the code -- just fewer images in the same request path.
- **`server/src/video.rs`**: `extract_keyframes` now returns `Vec<PathBuf>` (the
  paths actually written, possibly empty) instead of nothing, so its caller knows
  exactly what's available to hand to analysis.
- **`server/src/main.rs`**: the two previously-independent background threads (AI
  analysis, key-frame extraction) are merged into one per event -- extraction runs
  first (fast, and its result is exactly what "analyze the whole event" needs),
  then analysis runs with whatever it produced. Still strictly after the upload's
  `200` response is flushed, still on a single detached thread, still never able to
  block the single-threaded accept loop from accepting the next connection.
- **6 new/rewritten tests** in `ai.rs` (server now 48, up from 44): a
  `RecordingAnalyzer` fake proves the exact images (and their order --
  thumbnail first) actually reaching the analyzer, not just that *something* was
  saved; missing/nonexistent key frame files fall back safely to thumbnail-only;
  an empty key frame list behaves identically to thumbnail-only; a failing analyzer
  still produces a valid failed-shape `analysis.json` even with key frames present
  (never a panic); existing shape/identity/thumbnail-missing tests carried over
  unchanged. `video.rs`'s existing extraction test now also asserts on the returned
  `Vec<PathBuf>` matching what was actually written, in order.
- **Verified end-to-end against a real running Ollama instance with a real
  multi-image event** (not just synthetic tests): built a synthetic-but-real
  `PsramRecorder`-format clip from 3 different real JPEGs (the same door-cam photo
  used elsewhere, plus two unrelated real photos) and sent it as a thumbnail+video
  event through the running release server. Confirmed: 3/3 key frames extracted,
  response returned in ~25ms, and the resulting `analysis.json`'s description
  mentioned *both* "headphones" (only in the thumbnail) *and* "sports jersey" (only
  in the unrelated keyframe photos) in one combined sentence -- direct proof the
  model was actually shown and used every image, not just the first one. A second,
  thumbnail-only event sent immediately after produced a description mentioning only
  the thumbnail's content, confirming the fallback path is unaffected and doesn't
  leak unrelated content when no video is present.
- `cargo test -p server` -- 48 tests, all passing. `cargo clippy -p server` clean.
  Firmware entirely untouched (`git diff --stat -- firmware/` empty), confirmed.
  Retention cleanup (Milestone 20) needed no changes at all -- keyframe files
  already shared the `event_<timestamp>` prefix it groups by.

## Milestone 23 — event coalescing / cooldown to stop motion-burst spam

Built after live testing showed one continuous real-world episode could still produce
multiple separate uploads/`analysis.json` files in bursts. Root cause: the recording
tail could end one clip after a short PIR-low gap, then the next PIR-high reading
immediately started a new event even though it was still the same person/episode.

- **Firmware-only fix, committed in `2bf7208` (`Fix event spam`)**: added
  `REARM_QUIET_SECS = 15` in `firmware/src/bin/main.rs` and a pure
  `firmware::pir::RearmGate` helper. After an event closes, the firmware now requires
  a full 15 seconds of continuous PIR-low quiet before a new event can start. Any
  motion during cooldown resets the quiet timer.
- **Behavior accepted by user live testing**: quick/normal events still upload, and
  the new cooldown behavior is acceptable for stopping bursts from one continuous
  episode. This was tested on real hardware after flashing the fix.
- **No server protocol/storage changes**: upload, SD queue, AI analysis, keyframe
  extraction, retention, and event file naming remain unchanged.

## Milestone 24 — actionable notifications after AI analysis

First pass at the notification layer, based on common smart-doorbell alert categories
(person/visitor, package/delivery, vehicle, animal, other motion) and the local
privacy-first design already used for AI analysis. Scoped to the `server` crate only;
firmware is untouched.

- **New `server/src/notify.rs`**: notification policy and notifier abstraction.
  Default policy notifies for actionable events only:
  `person == true` OR `package == true` OR `importance == high`. Vehicle and animal
  alerts are opt-in via env vars (`NOTIFY_VEHICLES`, `NOTIFY_ANIMALS`) because they
  can be noisy depending on where the door faces. Low/empty motion stays local-only
  by default.
- **Telegram support is environment-configured, not hardcoded**:
  `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` enable Telegram; if either is missing,
  the server falls back to a console notifier that prints what would have been sent.
  Telegram sends the thumbnail with a concise caption/summary; if `sendPhoto` fails,
  it falls back to a text message.
- **On-demand video retrieval added on top of alerts**: actionable alerts now include
  the event ID and a Telegram inline **Send video** button whose callback payload is
  `video:<event_id>`, so the user does not need to type a long event number. The
  command listener also accepts `video <event_id>`, `video latest`, and `video last`;
  it only responds to the configured `TELEGRAM_CHAT_ID`, only accepts numeric event
  IDs (besides latest/last), and only serves files matching
  `server/uploads/event_<id>_video.bin`. Videos are sent as Telegram documents for
  now, because the server still stores raw recorder-frame `.bin` clips rather than a
  standard playable MP4/AVI container.
- **Playable video path added after live phone test**: Telegram originally sent the
  raw `.bin`, which the phone could not play. `server/src/video.rs` now converts the
  raw recorder-frame stream to `event_<id>_video.mp4` via local `ffmpeg`, keeping the
  original `.bin`. Upload/AI still do not depend on conversion success. The Telegram
  **Send video** button/`video latest` path now prefers/sends the `.mp4` and converts
  on demand if needed.
- **FRIDAY-style alert wording added after live screenshot review**: notifications no
  longer expose local Mac file paths. The alert now reads like an assistant message:
  `Project FRIDAY`, `Event #...`, a human one-sentence summary, priority, confidence,
  detected category, short reasons, and a short timeline. The Ollama prompt/schema was
  expanded accordingly (`confidence`, `reason`, `timeline`, plus optional `critical`
  priority) while keeping the original booleans for notification policy.
- **Security-concern detection tightened after knife test**: the AI schema/prompt now
  explicitly asks for `concerning_object` and `concerning_behavior`, with rules to look
  for knives, sharp objects, weapon-like objects, tools near the door, handle/lock
  tampering, camera tampering, hiding, aggressive gestures, or suspicious lingering.
  If either concern is present, notifications include a `security concern` category
  and the model is instructed to raise priority to high/critical when appropriate.
- **Raw scene analysis tightened after gesture test**: the AI schema/prompt now asks
  FRIDAY to act less like a polite object classifier and more like a direct scene
  analyst. `event_analysis` now includes `plain_description`, `notable_actions`,
  `concerning_details`, `likely_intent`, and `recommended_action`. The prompt
  explicitly tells the model to say visible gestures and behavior plainly, including
  obscene gestures such as a raised middle finger, knife-like/sharp objects, door/lock
  interaction, camera interaction, hostile gestures, waiting, leaving, or unclear
  details. Telegram alerts now prefer `plain_description` and can show notable
  actions, concerns, likely intent, and recommended action, so the user sees the raw
  practical readout instead of only booleans plus a softened one-line summary.
- **Private FRIDAY comment added for nuisance/threat events**: the AI schema/prompt
  now includes `friday_comment`, an optional one-sentence Telegram-only personality
  line. Normal/low-risk events should leave it empty. Hostile gestures, tampering,
  threats, or concerning objects can get a short witty FRIDAY-style roast such as a
  "camera keeps receipts" comment. The prompt explicitly keeps it safe: no slurs, no
  protected-trait insults, no body shaming, no sexual comments, no violent threats,
  and no instructions to harm anyone. Telegram alerts show it as `FRIDAY says: ...`
  when present. If the model marks an event as concerning but forgets to fill the
  comment, the server adds a small safe fallback line so nuisance/threat alerts still
  get the FRIDAY personality treatment. Old `analysis.json` files remain compatible
  because the new field defaults to an empty string.
- **Telegram long-caption failure fixed after event `1784135006831`**: that event
  uploaded and processed correctly (301 frames / about 25 seconds, AI success, MP4 and
  locked threat MP4 generated), but Telegram rejected the initial `sendPhoto` with HTTP
  400 because the AI-generated alert caption had grown too long for a photo caption.
  The notifier now uses a shorter photo caption with the Send video button attached,
  and sends the full FRIDAY analysis as a separate text message when the full alert is
  too long. This keeps the photo/button path from failing while preserving the longer
  explanation.
- **Telegram video resend guard added**: after FRIDAY successfully sends a video for
  an event, tapping the button or requesting that same event again will not keep
  re-sending the clip. FRIDAY replies that it was already sent; an explicit
  `video <event_id> again` or `video latest again` forces another copy.
- **Telegram command help/control added**: the command listener now responds to
  `help`, `/start`, and `commands` with the supported command list, plus `events`
  for the last 5 analyzed events, `summary latest`, `photo latest`, `analysis latest`,
  `video latest`, and `resend latest`. The same commands also accept explicit numeric
  event IDs where useful, e.g. `photo 1783957936278` or `analysis 1783957936278`.
  This makes Telegram a small FRIDAY control panel instead of only an alert sink.
- **Annotated video overlay path added**: when Telegram sends a video and a successful
  AI analysis exists for that event, the server now prefers a generated
  `event_<id>_annotated.mp4` over the plain `event_<id>_video.mp4`. The annotation is
  burned into the video with ffmpeg as a first-pass Project-FRIDAY/Person-of-Interest
  style HUD: outer frame, face/person-region block, and colored labels derived from
  the AI result. Red is used for concerning object/behavior, yellow for high-attention
  actions, green for packages, and white for ordinary person/vehicle/animal labels.
  After the first live overlay test, the ffmpeg filter was fixed so annotated output
  is actually generated instead of falling back to the plain video.
- **AI region boxes added for lock-on style overlays**: the AI prompt/schema now asks
  for `regions` -- approximate normalized boxes for face/head, person, package,
  knife/sharp object, gesture, door, or other important objects. `server/src/video.rs`
  can burn those regions into the MP4, and `server/src/notify.rs` maps face/person,
  package, knife/object, and gesture regions to the FRIDAY overlay colors. Existing
  old events without regions still fall back to broad preset boxes, and old
  `analysis.json` shapes are now tolerated so older videos can still receive a
  visible overlay instead of silently falling back to plain video. The generated file
  name changed to `event_<id>_locked.mp4` so stale earlier annotated files are not
  reused. This AI-region path is still not full per-frame tracking; it is event-level
  lock boxes from AI-provided coordinates, kept as a fallback behind the real tracker
  sidecar described next.
- **Real YOLO/ByteTrack sidecar added for actual locked boxes**: after Fable's
  research confirmed that Qwen/ffmpeg prompting cannot produce true lock-on boxes,
  added `scripts/track_video.py` plus `scripts/requirements-tracker.txt`. The Rust
  server now tries this sidecar first when Telegram sends a video: it runs local
  Ultralytics YOLO tracking (`yolo11m.pt` by default, ByteTrack, low confidence),
  writes `event_<id>_tracks.json`, and renders `event_<id>_locked.mp4` with per-frame
  boxes burned in. `.venv-tracker/` is gitignored and the server auto-uses
  `.venv-tracker/bin/python` when present. Verified on the old knife event
  `1783956503532`: the initial YOLO-only pass generated 108 frames / 211 detections,
  with white person lock and a red object/knife-like lock on the blade area (YOLO
  labelled the blade-like object as `scissors` in one frame, but the red box is on the
  concerning object). The older ffmpeg/AI-region overlay remains as fallback if the
  tracker sidecar or dependencies fail.
- **Face/head lock fixed after live review**: the first tracker pass derived
  `FACE/HEAD` from the top of the YOLO person box, which failed badly with the
  sideways camera angle (the face box could sit over the torso/arm). The tracker
  sidecar now uses OpenCV YuNet (`scripts/models/face_detection_yunet_2023mar.onnx`,
  kept local/gitignored) and runs face detection across 0/90/180/270-degree rotated
  frame views, mapping detections back to the original 640x480 frame. The face box now
  appears only when the face is actually detected and disappears when it leaves/fails
  detection. Re-verified on knife event `1783956503532`: the regenerated locked video
  produced 108 frames / 242 detections, with YuNet face detections in 93/108 frames,
  plus the red blade/object lock.
- **Threat-level color policy added**: tracker rendering now accepts an event-level
  `--threat-level normal|minimal|threat`, and the Rust server derives that from
  `analysis.json` when calling the sidecar. Normal events keep person/face boxes
  white. Concerning behavior without a weapon/object (e.g. hostile/obscene gesture)
  becomes `minimal` and colors person/face yellow with `FRIDAY LOCK - MINIMAL THREAT`.
  Concerning object or `critical` events become `threat` and color person/face red
  with `FRIDAY LOCK - THREAT`; knife/object boxes remain red. Cached outputs are
  separated by filename (`*_locked_normal.mp4`, `*_locked_minimal.mp4`,
  `*_locked_threat.mp4`) so a previous color state is not reused. Re-generated the
  middle-finger event `1783957936278` as minimal/yellow and the knife event
  `1783956503532` as threat/red; face duplicate boxes were reduced by assigning only
  one YuNet face box per tracked person.
- **Frame-aware threat coloring added after live review**: event `1784135006831`
  revealed that the prior event-level `threat` color made the video start with red
  face/person boxes even before any concerning object appeared. `scripts/track_video.py`
  now computes a `rendered_threat_level` per frame: normal/white when no threat object
  is visible, red only on frames where YOLO detects a concerning object (`knife`,
  `scissors`, `baseball bat`, etc.), and yellow remains the event-level fallback for
  gesture-only/minimal-threat events because there is not yet a reliable local
  frame-by-frame gesture detector. Tracker output filenames were bumped to
  `*_locked_frame_<level>.mp4` / `*_tracks_frame_<level>.json` so old cached all-red
  locked videos are not reused. Re-generated `1784135006831`: 292/301 frames rendered
  normal/white, 9/301 rendered threat/red, with the first frame visually confirmed
  white and the object frame confirmed red.
- **Clear-human Telegram alert photo added**: Telegram no longer has to use the first
  PIR trigger thumbnail or a fixed later keyframe. Added `scripts/select_alert_frame.py`,
  a lightweight YuNet face-selector sidecar that scans the extracted keyframes (with
  0/90/180/270-degree rotated face detection for the sideways camera view), chooses the
  frame with the best visible face/head, and writes `event_<id>_alert.jpg`. The server
  now tries that file for the initial Telegram `sendPhoto`, falling back to keyframe 1,
  keyframe 0, then the original thumbnail if no face is found or the sidecar fails.
  Verified on knife event `1783956503532`: selected `keyframe_1` and produced a clear
  human alert image with face/object visible.
- **Alert thumbnail improved**: the stored event thumbnail is still the first capture
  at trigger time, but Telegram notifications now use a later extracted keyframe when
  available (keyframe 1, falling back to keyframe 0, then the original thumbnail).
  This avoids alert photos where the person has not fully stepped into frame yet.
- **Notification runs only after `analysis.json` is saved**, using the
  `AnalysisResult` returned by `ai::analyze_and_save`. A notification failure is only
  logged; it never affects upload success, event storage, AI analysis, keyframe
  extraction, or retention cleanup.
- **Live Telegram notification verified**: a real camera event uploaded from the
  ESP32, produced keyframes and `analysis.json`, and the server logged
  `notification(telegram): sent`. The model result had `person: true`, `importance:
  medium`, so it correctly notified under the actionable-event rule.
- **Tests/checks**: server now has 63 tests passing (policy, alert formatting, strict
  `video` command parsing/resend parsing, callback-data parsing, security concern
  notification, command help/list parsing, recent-event listing, and safe video-path
  resolution).
  `cargo clippy -p server -- -D warnings` passes. `cargo fmt -p server` could not run
  on this Mac because `rustfmt` is not installed for the active stable toolchain.

## Milestone 25 — longer minimum clip duration

Small firmware behavior tweak following Telegram playback testing: very short silent
clips can appear GIF-like on phones. `MIN_EVENT_DURATION` in `firmware/src/bin/main.rs`
was raised from 5 seconds to 10 seconds, so even a quick PIR trigger records a more
normal-feeling event clip and captures more context after the subject has entered the
camera view. Tail duration remains 5 seconds, and re-arm quiet gap remains 15 seconds.
Firmware builds cleanly in release mode; the new duration still needs flashing to the
ESP32 before it affects hardware behavior.

## Next steps (not yet started)

- **Actionable notifications: in progress (Milestone 24).** Telegram alerting and
  on-demand video retrieval have been live-tested. The newest raw scene-analysis
  prompt/schema change is unit/clippy-clean but still needs one real live gesture/
  concerning-object test before calling that specific behavior process-verified.
- **Event coalescing/cooldown: complete (Milestone 23).** Committed in `2bf7208` and
  accepted by user live testing on real hardware.
- **AI analysis now covers the whole event: complete (Milestone 22).** Verified
  against a real running Ollama instance with a real multi-image event, not just
  synthetic tests -- the model's output demonstrably used both the thumbnail and
  the video keyframes together. Video AI remains otherwise exactly as deferred as
  before (no per-frame/raw-video analysis, no bounding boxes, no face recognition).
- **Server-side key-frame extraction: complete (Milestone 21).** Verified against a
  real stored clip (not just synthetic tests). Sets up video-AI work for later
  (still explicitly deferred) without doing any of it yet.
- **Server-side retention cleanup: complete (Milestone 20).** Committed in `8ff4e1c`.
  Verified with unit tests (synthetic backdated files) plus a real server restart
  smoke test that left all 137 recent real event files alone. Not yet observed
  deleting naturally aged-out real files over a real 30-day lifetime; low risk, not
  blocking anything.
- **AI thumbnail analysis: complete (Milestone 19).** Superseded by Milestone 22 for
  the "thumbnail only" limitation specifically -- analysis now covers video key
  frames too when present. The Ollama integration, prompt design, and
  offline/failure handling proven here still apply unchanged. Natural future
  extensions, not started: the known-person-recognition module the `identity` block
  was built to support, bounding boxes -- none blocking anything else.
- **Audio part, optional, later.** `PartKind::Audio` already exists in the envelope
  (reserved, unused) -- add it only once there's an actual audio source to capture;
  the wire format and server storage path need no changes to support it when that
  happens.
- **Persistent offline queue: complete (Milestone 18).** Its real server-outage → SD
  save → recovery → drain-and-delete path is hardware-verified above. Optional extra
  hardening remains: test a missing/failed card and an actual duplicate replay after a
  server restart, but neither blocks moving to local AI analysis.
- Build out the `server` crate's request routing beyond the single `POST /upload`
  endpoint (structured per-part storage is now done, per Milestone 14) once there's an
  actual second concern to route to (e.g. an AI-processing trigger or a query endpoint).
- Decide a real container format for the video part once it's time to make clips
  independently playable server-side without a decode script (currently `.bin`,
  requiring `decode_raw_capture.py`-style parsing) -- e.g. actual MJPEG/AVI muxing, or
  running the existing ffmpeg assembly step directly in the `server` crate after
  storage.
- Optionally investigate the FPS gap noted in Milestone 7 (PSRAM copy overhead: ~12-14
  FPS vs. Milestone 6's 20.06 FPS) if smoother recorded video is wanted.
- Act on the image-quality findings from the investigation above once a sample image is
  available to diagnose against: mechanical focus check, sharpness register, lower
  JPEG quality — all cheap, safe, no code written yet pending user direction.
- Optionally hand the community/undocumented-tricks research (image quality
  investigation, point 7) to Codex.
- `FOR_CODEX.md`'s original content (UPDATE 1-8) can likely be deleted entirely now —
  it documents a hang that was never real, and is superseded by this file.

## Current exact codebase state

**Committed to git** (through `b30dde0`, Milestones 9-10): PIR + WS2812 (milestones
1-3), OV3660 camera capture (Milestone 4), reusable `CameraHandle` + PIR integration
(Milestone 5), continuous capture/MJPEG video/UXGA/3x FPS fix (Milestone 6),
PSRAM-backed record-then-export (Milestone 7), motion-triggered PSRAM recording as a
standalone test binary (Milestone 8), the doorbell-style event state machine moved
into `main.rs` with strict JPEG validation (Milestones 9-10), workspace structure,
`shared`/`server` scaffolding. `RECORDING_ENABLED` is `false` in the committed code
(verified via `git show b30dde0:firmware/src/bin/main.rs`) -- this was the value in
the working tree at commit time, so recording is currently off; flip to `true` and
reflash to re-enable motion-triggered recording.

**Committed** (`377f260`, Milestones 11-13): WiFi scan/connect (`wifi_scan_test.rs`,
`wifi_test.rs`), hardened HTTP POST receiver (`server/src/main.rs`) with auth token,
bounded header/body sizes, read timeout, exact-body validation, collision-safe
filenames, generic `404`s, and both rounds of post-review fixes; `http_post_test.rs`;
gitignored-credential templates; `DEPLOYMENT.md`.

**Committed** (`65940e9`, Milestone 14, v2 envelope after post-review hardening,
described above):
- `shared/src/lib.rs` — the event envelope, v2: `MAGIC`/`VERSION` constants,
  `PartKind`/`Encoding`/`EnvelopeError` enums, `encode_envelope_header` (now takes
  `event_id: u64`) / `encode_part_header` (now takes `encoding`/`timestamp_ms`/
  `duration_ms`) (fixed-size arrays, no allocation), `decode_envelope_header`/
  `PartsIter` (borrow from caller's slice, no allocation; strictly rejects zero-part
  envelopes, undeclared trailing bytes, and kind/encoding mismatches via an explicit
  whitelist). 11 `#[cfg(test)]` unit tests.
- `firmware/src/bin/event_upload_test.rs` — new, WiFi + real camera capture, uploads
  one `Thumbnail` part, verified on hardware (real JPEGs stored and opened correctly),
  re-verified again after the v2 bump.
- `firmware/src/bin/event_upload_video_test.rs` — new, WiFi + thumbnail capture + 5s
  PSRAM-recorded burst, uploads both as a two-part envelope with real
  `timestamp_ms`/`duration_ms` per part, verified on hardware (70-frame clips decoded
  and assembled into playable `.mp4`), re-verified again after the v2 bump.
- `firmware/Cargo.toml` — `[[bin]]` entries for both new binaries.
- `server/src/main.rs` — `store_upload()` (single raw-bytes file) replaced by
  `store_event()` (delegates to `store_event_with_committer()`, added for fault-injection
  testability) + `commit_part_file()`: parses (and fully validates) the whole envelope
  before writing anything, then writes each part to a private temp file (`sync_all`,
  not just `flush`, for real durability), committing it to its final
  `event_<timestamp>_<label>.<ext>` name via `hard_link` (collision-safe and
  rollback-safe, not `rename` -- see the post-review subsections above for why, including
  the fix for a committed part being able to escape rollback bookkeeping if temp-file
  cleanup failed afterward); malformed envelopes -> `RequestError::InvalidEnvelope` ->
  `400`; storage-layer I/O failures -> `RequestError::StorageFailure` -> `500`, distinct
  from connection-layer `Io` -> `400`. Both storage functions take `dir` as a parameter
  (not hardcoded to `UPLOADS_DIR`) so tests use a scratch temp directory. 12
  `#[cfg(test)]` unit tests total (`cargo test -p server`), including two
  fault-injection tests pinning down exactly which part fails and which files get
  rolled back.

**Committed** (`d6cd733`, Milestone 15): WiFi connect/DHCP at boot; `UploadOutcome` enum
and `upload_event()` (builds the two-part envelope, sends it, parses the response);
called right after each real motion event finishes recording; USB raw export still
always runs afterward regardless of the WiFi outcome; `RECORDING_ENABLED` flipped from
`false` to `true`. Verified on real hardware across 4 real motion events including a
real failed-server scenario and automatic recovery once the server came back.

**Committed** (`0a2feb3`, Milestone 16, described above):
- `shared/src/lib.rs` — new `EnvelopeError::EmptyPart(PartKind)`, enforced in
  `PartsIter` (`len == 0` rejected). 12 `#[cfg(test)]` unit tests (up from 11).
- `server/src/main.rs` — new `EventDedup` (bounded/FIFO ring of recently seen
  `event_id`s), threaded through `store_event_with_committer` -> `store_event` ->
  `process_request` -> `handle_connection` -> `main` (one instance per server run,
  `EVENT_DEDUP_CAPACITY = 256`); a duplicate `event_id` short-circuits to `Ok(vec![])`
  (200, nothing written twice) right after envelope-header parsing, before the parts
  list is even built -- `is_duplicate` (read-only check) and `record` (only called after
  every part is actually committed) are deliberately separate methods, not one
  check-and-record, so a request that fails validation or storage never burns its
  `event_id` (a post-review fix; see the dedup-timing subsection above). 17
  `#[cfg(test)]` unit tests total (up from 12).
- `firmware/src/bin/main.rs`:
  - `CONNECT_TIMEOUT`/`TRANSFER_TIMEOUT` bound every upload attempt via
    `embassy_time::with_timeout`; new `UploadOutcome::TimedOut`.
  - `wifi_maintain_task` (new background task: connect, wait for disconnect, reconnect,
    forever) replaces the old blocking connect-then-`wait_config_up` sequence in
    `main()` -- boot no longer waits on WiFi at all.
  - `upload_event_with_retries()` (new) wraps `upload_event()` in a bounded retry loop
    (`MAX_UPLOAD_ATTEMPTS = 3`, `UPLOAD_RETRY_BACKOFF = 3s`), skipping retry for `4xx`
    rejections via `UploadOutcome::is_retryable()`.
  - `event_id` now `(boot_nonce << 32) | recording_count`, `boot_nonce` from
    `esp_hal::rng::Rng` -- reboot-safe input to the server's new dedup.
  - `recorder.frame_count() == 0` now skips the WiFi upload entirely (USB export still
    runs) instead of sending empty parts.
  - `part_timestamp_ms` (measured the moment the first frame actually succeeds, not
    hardcoded `0`) passed to both the thumbnail and video part headers;
    `video_duration_ms` now `event_duration_ms - part_timestamp_ms`.
  - Module doc comment's stale "2s tail" corrected to "5s", and expanded to describe
    the background WiFi supervisor and retry behavior.
- `NEXT_STEP.md` — untracked by design (user preference, never `git add` this file)

**Committed** (`9292db7`, then `76862d1`, Milestone 17): isolated `sdmmc_test/` crate
proving the onboard microSD slot works via an unreleased `esp-hal` fork; `firmware/`'s
real dependencies migrated to that same pinned revision; the `esp_radio::wifi` API
migration across 6 files; the `lto = 'thin'` fix for an LLVM backend crash the fork
triggers on camera/DMA_CH0 binaries; new `sd_regression_test.rs` combined hardware
regression binary; the `capture_jpeg` DMA-buffer-reuse fix in `camera.rs` (see
Milestone 17 above for the full account).

**Committed** (`1a4298a`, Milestone 18): new `firmware/src/queue.rs` (SD-backed offline
event queue: `save_event`/`drain_queue`); `main.rs` initializes the SD card as an
optional subsystem and wires save-on-failure/drain-on-success into the event loop;
`server/src/main.rs`'s `EventDedup` gained `new_with_persistence` so dedup survives a
server restart; `server/event_dedup.log` added to `.gitignore`. Verified at the
build/test level only -- not yet a real hardware pass (see Milestone 18 above).

**Committed** (`85e1cf8`, then `7c9463f`, Milestone 19): new `server/src/ai.rs`
(`ThumbnailAnalyzer` trait, `OllamaAnalyzer`, `EventAnalysis`/`Identity`/`AiMeta`/
`AnalysisResult`, `analyze_and_save`); `server/src/main.rs` adds `mod ai;`, threads an
`Arc<dyn ThumbnailAnalyzer>` through `process_request`/`handle_connection`/`main`, and
spawns a detached background thread per event for the analysis call (never in-line,
per the single-threaded-accept-loop finding above); `server/Cargo.toml` gained `ureq`
(`json` feature), `serde`/`serde_json`, `base64`; new `scripts/send_test_event.py` dev
tool. `firmware/` untouched.

**Committed** (`8ff4e1c`, Milestone 20): new `server/src/retention.rs`
(`clean_expired_events`, `event_key`, `retention_from_env`); `server/src/main.rs` adds
`mod retention;` and spawns a second detached background thread at startup for the
hourly sweep, alongside the existing AI-analysis and WiFi-independent design
philosophy of never blocking the single-threaded accept loop. `server` now has 35
tests passing; `cargo clippy -p server` clean; `firmware/` untouched. A real restart
of the running server confirmed the sweep starts cleanly and leaves recent real
events alone.

**Committed** (Milestone 21): new `server/src/video.rs`
(`extract_keyframes`, frame parsing, `MAX_KEYFRAMES`-based even-spacing selection);
`server/src/main.rs` spawned a third kind of detached background thread in
`process_request` (alongside AI analysis) whenever a `Video` part was stored this
request. `server` had 44 tests passing; `cargo clippy -p server` clean;
`firmware/` untouched. Verified against a real, previously-recorded 71-frame clip
sent through the running release server, not just synthetic tests.

**Committed** (`8edd102`, Milestone 22, described above): `server/src/ai.rs`'s
`ThumbnailAnalyzer` trait renamed `EventAnalyzer` and its `analyze` method changed
to take `images: &[&[u8]]` instead of one JPEG; `PROMPT` updated to the new
event-level wording; `analyze_and_save` gained a `keyframe_paths` parameter.
`server/src/video.rs`'s `extract_keyframes` now returns the written `Vec<PathBuf>`
instead of nothing. `server/src/main.rs` merged the AI-analysis and key-frame-
extraction background threads into one per event (extraction first, then analysis
with whatever it produced). `server` now has 48 tests passing; `cargo clippy -p
server` clean; `firmware/` untouched. Verified against a real running Ollama
instance with a real multi-image event built from 3 different real JPEGs, and a
real thumbnail-only fallback event sent immediately after.

**Committed** (`2bf7208`, Milestone 23): firmware event coalescing/cooldown fix.
`firmware/src/bin/main.rs` adds `REARM_QUIET_SECS = 15` and waits for a real quiet
gap before re-arming after an event; `firmware/src/pir.rs` adds `RearmGate` and
plain-logic tests for the quiet-window behavior. User live-tested the behavior and
accepted it.

**Uncommitted, current** (Milestone 24, described above): new `server/src/notify.rs`
(`NotificationPolicy`, console/Telegram notifier implementations, actionable-event
rules, Telegram command/callback listener, safe `video <id>`/`video latest` retrieval);
`server/src/ai.rs` now returns the saved `AnalysisResult` and uses the expanded raw
scene-analysis schema (`plain_description`, `notable_actions`,
`concerning_details`, `likely_intent`, `recommended_action`); `server/src/main.rs`
creates the notifier/policy at startup, starts the Telegram command listener, and
calls notification after successful AI analysis. `server/Cargo.toml` enables ureq's
`multipart` feature so Telegram can send the thumbnail/photo, adds `dotenvy` so
gitignored `server/.env.local` is loaded at startup, and `server/src/video.rs` can
convert the raw recorder-frame `.bin` to a playable `.mp4` via ffmpeg for Telegram
video requests. `server` has 58 tests passing; `cargo clippy -p server -- -D
warnings` clean; firmware untouched for this notification/AI wording milestone.

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

Nothing blocking — **the entire camera phase (Milestones 1-10) is complete and
committed**, **the WiFi/transport foundation (Milestones 11-13) is committed
(`377f260`) and verified on hardware end-to-end**, and **real event data (Milestone 14)
is now flowing over that path, also verified on hardware, including two rounds of
post-review hardening** (strict envelope parsing including kind/encoding validation,
genuinely transactional hard-link-based storage with real fault-injection test
coverage, `sync_all`-based durability instead of a no-op `flush`, richer per-part
metadata via a v2 wire-format bump, and correct `500`-vs-`400` status codes -- see both
post-review subsections above): the board captures a real JPEG thumbnail and a real
5-second PSRAM-recorded video clip, wraps both in the `shared` crate event envelope
(v2), and uploads them together to the hardened receiver, which fully validates the
envelope before writing anything and stores each part atomically -- including correctly
rolling back an already-committed earlier part if a later part fails, now proven by a
test that forces exactly that sequence via an injectable committer, not just "some
failure leaves nothing behind." The thumbnail opens correctly as a JPEG; the video part
decodes cleanly into 70 frames and assembles into a real, playable H.264 clip via
`ffmpeg` -- confirmed with `ffprobe` and by visually opening a frame, not just checking
exit codes. The envelope format has 11 unit tests (`shared`) and the storage path has
12 more (`server`), all passing. Both firmware test binaries were re-flashed and
re-verified on hardware after the v2 wire-format bump specifically; the second
hardening round's fixes don't change the wire byte layout, so that round was
live-verified against the real running server binary via `curl` instead (a valid
envelope stored correctly, a kind/encoding mismatch correctly rejected) plus a firmware
rebuild to confirm no compile breakage, rather than a full hardware reflash.

**`main.rs` now uploads real motion events over WiFi (Milestone 15), verified on
hardware including a real failed-server scenario**: the real PIR-triggered
doorbell-style event state machine (Milestones 9-10) -- not a test binary's
fixed-duration loop -- now uploads its thumbnail + video as one envelope right after
each event finishes recording, logs a clear success/failure line via a new
`UploadOutcome` enum, and always still runs the pre-existing USB raw export regardless
of the WiFi outcome. 4 consecutive real motion events were tested: 2 succeeded normally
with the server up, 1 correctly failed (`ConnectFailed`, logged, no crash, USB export
still ran, state machine still rearmed) when the server was killed mid-test, and 1
succeeded again immediately once the server came back -- with zero code changes or
intervention, confirming the single-attempt design recovers on its own. `main.rs`'s
`RECORDING_ENABLED` was flipped from `false` to `true` to run this test and left that
way, since the project has moved from "developing near the board" into real
reliability testing.

**Milestone 15 was committed (`d6cd733`), then a review of it found seven reliability
gaps -- all fixed and verified on hardware (Milestone 16)**: no socket timeouts (fixed:
`CONNECT_TIMEOUT`/`TRANSFER_TIMEOUT` via `embassy_time::with_timeout`), boot blocking
forever on WiFi so the board couldn't record during an outage (fixed: a background
`wifi_maintain_task` connects/reconnects independently of `main()`'s motion loop, which
now proceeds immediately), WiFi reconnection after a later disconnect not being handled
(fixed: the same task also covers this via `wait_for_disconnect_async`), an all-frames-
failed event being able to upload empty parts and get a real `200` (fixed at both the
`shared` wire-format level -- `EnvelopeError::EmptyPart` -- and in `main.rs`, which now
skips the upload entirely when `frame_count() == 0`), retries needing a reboot-safe
event identity and server-side deduplication to avoid duplicate files (fixed: a random
per-boot nonce folded into `event_id`, plus a new server-side `EventDedup`, together
making a real bounded retry loop -- `upload_event_with_retries`, 3 attempts, 3s backoff
-- actually safe to add), the video part's `timestamp_ms` being hardcoded to `0` despite
real warm-up latency before the first frame (fixed: measured and passed through, with
`video_duration_ms` adjusted to match), and a stale "2s tail" doc comment (corrected to
5s). `shared` now has 12 unit tests, `server` 14, all passing. Hardware verification
specifically targeted the new behavior: a temporarily-scaffolded forced disconnect
proved live reconnection (reverted before commit); a temporarily-wrong WiFi password
proved degraded boot, the full 3-attempt retry cycle with correct backoff, and graceful
final failure, all in one real event, with the board rearming and continuing normally
afterward (credentials restored, clean firmware reflashed before further testing);
multiple normal events confirmed real nonzero `timestamp_ms` values. The one
sub-scenario not independently pinned down by a single live test -- a retry succeeding
mid-cycle after an earlier attempt in the *same* call failed -- is covered by combining
the degraded-boot test (proves the retry loop's mechanics) with every other successful
test (proves a fresh attempt succeeds once connectivity is back), rather than by exact
timing coordination with live human-triggered motion.

**Critical fix on top of Milestone 16 (server-only, no hardware retest needed)**: a
review found `EventDedup` recorded `event_id` as seen *before* validation/storage
actually succeeded -- so a failed attempt (`400`/`500`) would permanently mark that
`event_id` as already-stored, and firmware's own retry (same `event_id`, per Milestone
16's design) would then get a `200` without anything actually being written. Fixed by
splitting `check_and_record` into a read-only `is_duplicate` and a `record` called only
after every part of an event is actually committed. 3 new tests (`server` now 17, up
from 14) prove: a storage failure followed by a retry with the same ID stores
correctly; an invalid payload followed by a corrected retry with the same ID stores
correctly; and the fix's own tests were confirmed to actually catch the bug (temporarily
reintroduced the old early-record behavior, watched the two new tests fail, restored the
fix). This is a `server`-only change to code the firmware side already assumed worked
correctly, so no wire format or firmware behavior changed -- no hardware reflash needed.
`EventDedup` is still explicitly in-memory only (a server restart forgets recorded
`event_id`s); that's flagged as an intentional, documented limitation to be addressed
together with the future persistent offline queue, not patched in isolation now.

**Codex SDMMC research is complete; no longer blocked on research.** A newly opened,
still-unmerged `esp-hal` PR provides an initial native ESP32-S3 SDMMC host driver and an
async FAT smoke test using this board's exact built-in slot pins (GPIO38/39/40). Codex
also proved in an isolated copy that the current complete firmware can compile with
that driver when the matching `esp-hal` ecosystem is pinned coherently to exact
revision `cef6d86604d91abcf62afc9804724a637eb7af3a`; patching only the HAL is not
compatible. See "Future local fallback storage — onboard microSD" above for the full
sources, API, compatibility experiment, ESP-IDF conclusion, and risk analysis.

**Done: the onboard slot smoke test passed, twice, on real hardware (Claude).** A fully
isolated `sdmmc_test/` crate (own workspace, not touching `firmware/`) was pinned to the
exact revision Codex identified and built successfully. The first flash attempt failed
("appdesc segment not found"); root-caused (by comparing ELF layout against the known-
working `firmware` binary) to the new crate simply missing the `build.rs` every esp-hal
app needs, which is what actually supplies the `-Tlinkall.x` linker script -- copying
`firmware/build.rs` over fixed it immediately. After that, two separate BOOT-button
presses against the user's real inserted 1GB FAT32 card both produced `async FAT CRUD:
PASS` (a genuine create/read/update/read/delete cycle, not a mock). See "Future local
fallback storage — onboard microSD" above for the full account. **The onboard slot is
viable; no external SPI breakout is needed.**

**Done (Milestone 17): `sdmmc_test/` committed, `firmware/` fully migrated to the pinned
SDMMC-capable esp-hal fork, and the combined camera+PIR+LED+WiFi+SD hardware regression
gate genuinely passes** -- including finding and fixing a real latent bug (`capture_jpeg`
could only succeed once per boot post-migration; see Milestone 17 above) that hadn't
surfaced yet because no binary had been re-run twice on hardware since the fork
migration until this test existed.

**Done (Milestone 18): the SD-backed offline queue and persistent server-side dedup are
implemented, building directly on Milestone 17's proven migration.** `firmware::queue`
saves an event's exact envelope bytes to the SD card (atomically, via a temp-name-then-
rename) whenever `upload_event_with_retries` exhausts its attempts for a transient reason,
and drains (uploads + deletes-on-`200`) the queue both at boot and after every successful
live upload. `main.rs` now initializes the SD card as an optional subsystem -- a missing
or failed card just disables the queue for that boot rather than blocking camera/PIR/WiFi.
`EventDedup` on the server gained `new_with_persistence`, closing the exact "forgets
everything on restart" gap Milestone 16 flagged, which the SD queue makes a real scenario
(a queued event can be replayed well after a server restart). All build/test-level
verification passes (14 firmware binaries build+clippy clean, 33 host tests pass, 4 new),
and it has since been **verified on real hardware, the exact server-outage scenario**:
server stopped, a real motion event recorded and failed to upload after 3 attempts, saved
to SD (`event #1: queued to SD`); server restarted; a second real motion event uploaded
live and its success triggered a drain that found, delivered, and removed the queued file
(`Drained { uploaded: 1, dropped: 0 }`); both events independently confirmed stored in the
server's own log. **Milestone 18 is complete.**

**Immediate next step**: none blocking -- the SD-backed offline queue is implemented and
hardware-verified end to end. Optionally worth a future pass: confirming behavior with the
card removed/failing (queue should just disable itself, not block recording -- implemented
but not separately hardware-exercised), and a scenario with two separate server restarts to
directly exercise the persisted-dedup-across-restart path with an actual duplicate replay
(this pass only restarted the server once, before the drain that delivered the queued
event, which isn't quite the same as a duplicate arriving after a restart). Battery work
remains parked until the user decides to pick it back up.

Also still open, lower priority: trawling GitHub issues/forums/Reddit/Discord for
OV3660-specific community register tweaks for image quality (see "Image quality
investigation" above, point 7) — deliberately not fabricated here since there's no live
access to those discussions in this session.

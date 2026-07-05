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
+ `cargo clippy`, both with zero warnings) and flashed to hardware. **The
slow-lowering re-test to confirm this actually fixes the early-stop behavior has not
been run/confirmed yet** -- the user was mid-test (via `scripts/capture_psram_video.sh`)
when this doc was last updated; result pending.

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

## Next steps (not yet started)

- **Confirm the 5s tail actually fixes the early-stop behavior**: `TAIL_DURATION` is
  already raised to 5s and flashed (see Milestone 10 above) -- what's left is running
  the same slow-lowering motion test again and confirming the recording now covers the
  whole motion, including the drop. If it still cuts off early, the AM312's own
  blind/retrigger timing may need more than 5s of margin, or a different mitigation.
- Decide what happens to a recorded clip after export (export automatically over
  serial like the test binary? only on request? relates directly to the still-pending
  WiFi/upload milestone below, which is the real intended transport).
- Optionally investigate the FPS gap noted in Milestone 7 (PSRAM copy overhead: ~12-14
  FPS vs. Milestone 6's 20.06 FPS) if smoother recorded video is wanted.
- Act on the image-quality findings from the investigation above once a sample image is
  available to diagnose against: mechanical focus check, sharpness register, lower
  JPEG quality — all cheap, safe, no code written yet pending user direction.
- Optionally hand the community/undocumented-tricks research (image quality
  investigation, point 7) to Codex.
- **WiFi upload to the server.** The actual next major project milestone per the
  stated goal above — right now a captured JPEG/clip only ever sits in RAM/PSRAM and
  gets dumped to serial for local debugging. The real pipeline needs: connect to WiFi
  (`esp-radio`/`embassy-net`, both already in `Cargo.toml`), POST the captured bytes to
  the `server` crate (already scaffolded in the workspace, not yet implemented), get a
  response back, decide what to do with it (eventually: phone notification). During
  this milestone, replace the untouched `shared` crate stub with the extensible event
  envelope described above: thumbnail/video now, optional audio later.
- `FOR_CODEX.md`'s original content (UPDATE 1-8) can likely be deleted entirely now —
  it documents a hang that was never real, and is superseded by this file.

## Current exact codebase state

**Committed to git** (through `6da4771`, Milestone 8): PIR + WS2812 (milestones 1-3),
OV3660 camera capture (Milestone 4), reusable `CameraHandle` + PIR integration
(Milestone 5), continuous capture/MJPEG video/UXGA/3x FPS fix (Milestone 6),
PSRAM-backed record-then-export (Milestone 7), motion-triggered PSRAM recording as a
standalone test binary (Milestone 8), workspace structure, `shared`/`server`
scaffolding.

**Uncommitted, current** (Milestones 9 and 10, described above):
- `firmware/src/bin/main.rs` — Milestone 9's relocation, then Milestone 10's full
  doorbell-style event state machine on top (min duration, tail, renewed-motion
  handling, thumbnail preservation, no fixed max), then `TAIL_DURATION` raised
  2s → 5s (built, clippy-checked, flashed; hardware re-test result pending), then a
  `RECORDING_ENABLED` dev-convenience toggle added -- **currently set to `false`**
  in the working tree (user flipped it directly in the editor for their current dev
  session; not yet reflashed with this value as of this doc update)
- `firmware/src/camera.rs` — `trim_to_jpeg()` now returns `Result`, new
  `CameraError::InvalidJpeg` variant
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

Nothing blocking — **Milestones 5 through 10 are complete** (reusable `CameraHandle`,
PIR-triggered capture, continuous-capture MJPEG video pipeline, UXGA resolution, 3x FPS
fix, PSRAM-backed record-then-export, motion-triggered PSRAM recording, and now a real
doorbell-style event state machine in `main.rs` -- min duration, continue-while-active,
post-motion tail with restart-on-renewed-motion, no fixed maximum, thumbnail
preservation, strict JPEG validation -- all verified repeatedly on hardware, including
a real 33-second walk-around producing a valid clip). Of the user's original
product-behavior gap list, everything is now done except: an actual export-timing
policy decision (still immediate raw USB dump every time, not yet tied to the
still-pending WiFi milestone), and circular pre-roll (explicitly deferred, not needed
yet).

**Resolved behavior question:** AM312 timing research found that its approximately
2-second trigger/hold and blocking behavior leaves no useful margin when paired with
the firmware's previous 2-second tail. `TAIL_DURATION` is now raised to 5 seconds,
built and flashed -- the same slow-lowering hardware test needs to be repeated to
confirm this actually resolves the early-stop behavior. Not yet confirmed either way.

Also still open, lower priority: trawling GitHub issues/forums/Reddit/Discord for
OV3660-specific community register tweaks for image quality (see "Image quality
investigation" above, point 7) — deliberately not fabricated here since there's no live
access to those discussions in this session. Otherwise open to input on export-timing
policy, or the next major milestone, WiFi + upload to the server (see "Next steps"
above), e.g. `esp-radio`/`embassy-net` setup specifics or server request format, but
nothing is currently blocked.

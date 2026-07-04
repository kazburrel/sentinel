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
verified on real hardware, not yet committed to git.

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

## Next steps (not yet started)

- Act on the image-quality findings above once a sample image is available to diagnose
  against: mechanical focus check, sharpness register, lower JPEG quality — all cheap,
  safe, no code written yet pending user direction.
- Optionally hand the community/undocumented-tricks research (point 7 above) to Codex.
- **WiFi upload to the server.** The actual next major project milestone per the
  stated goal above — right now a captured JPEG only ever sits in RAM and gets
  hex-dumped to serial for local debugging. The real pipeline needs: connect to WiFi
  (`esp-radio`/`embassy-net`, both already in `Cargo.toml`), POST the captured JPEG
  bytes to the `server` crate (already scaffolded in the workspace, not yet
  implemented), get a response back, decide what to do with it (eventually: phone
  notification).
- `FOR_CODEX.md`'s original content (UPDATE 1-8) can likely be deleted entirely now —
  it documents a hang that was never real, and is superseded by this file.
- Milestones 5 and 6 (everything above) are not yet committed to git.

## Current exact codebase state

**Committed to git** (through `499bb41`, Milestone 4): PIR + WS2812 (milestones 1-3),
OV3660 camera capture proven working standalone via `camera_test.rs` (Milestone 4),
workspace structure, `shared`/`server` scaffolding.

**Uncommitted, current** (Milestones 5 and 6, described above):
- `firmware/src/camera.rs` — `CameraHandle` struct, `capture_jpeg` +
  `capture_jpeg_continuous` (shared `capture_jpeg_with_warmup` internal), `Framesize`
  parameter on `new()`
- `firmware/src/ov3660.rs` — `Framesize` enum, `OV3660_FRAMESIZE_UXGA` table,
  `init_jpeg()` takes a `Framesize` argument
- `firmware/src/bin/camera_test.rs` — uses `CameraHandle` + `Framesize::Vga`
- `firmware/src/bin/main.rs` — real PIR-triggered camera capture wired in, unchanged
  since Milestone 5, `Framesize::Vga`
- `firmware/src/bin/video_test.rs` — new, continuous-capture FPS/stability benchmark
- `firmware/src/bin/mjpeg_test.rs` — new, N-frame capture + hex-dump for video assembly
- `firmware/src/bin/uxga_test.rs` — new, single-shot UXGA capture test
- `firmware/src/bin/uxga_video_test.rs` — new, continuous UXGA burst capture
- `firmware/src/lib.rs` — declares `pub mod hexdump;`
- `firmware/src/hexdump.rs` — shared serial hex-dump helper
- `firmware/Cargo.toml` — `[[bin]]` entries for all of the above
- `scripts/capture_photo.sh`, `scripts/decode_capture.py` — local dev tooling for
  triggering a single capture and viewing the resulting photo (`decode_capture.py` now
  always numbers output frames, even a single one, for consistent ffmpeg input)
- `scripts/capture_video.sh` — new, waits for a board-side MJPEG capture to finish,
  assembles frames into a playable `.mp4` via ffmpeg
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

Nothing blocking — **Milestones 5 and 6 are complete** (reusable `CameraHandle`,
PIR-triggered capture in `main.rs`, continuous-capture MJPEG video pipeline, UXGA
resolution, 3x FPS fix — all verified repeatedly on hardware). One optional,
non-blocking research task if useful: trawling GitHub issues/forums/Reddit/Discord for
OV3660-specific community-discovered register tweaks or undocumented tricks for image
quality (see "Image quality investigation" above, point 7) — deliberately not
fabricated here since there's no live access to those discussions in this session.
Otherwise open to input on the next milestone, WiFi + upload to the server (see "Next
steps" above), e.g. `esp-radio`/`embassy-net` setup specifics or server request format,
but nothing is currently blocked.

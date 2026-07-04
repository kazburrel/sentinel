# Handoff to Codex — camera capture silent-boot, narrowed down

---
## ⚠️ RESOLVED / OBSOLETE — READ `PROJECT_STATUS.md` INSTEAD

Everything below this point (UPDATE 1-8) was chasing a hang that **turned out not to
exist**. It was a test-methodology artifact: every diagnostic firmware in this log
printed its result exactly once, and that single print consistently landed in the
narrow window our serial reader misses right after a board reset/replug. A test that
repeats its result forever (rather than once) immediately proved the camera hardware
was working correctly the whole time — confirmed via a completely different toolchain
(Arduino + the official `esp32-camera` library), which got a real, valid JPEG photo
off the board.

Separately, the camera sensor was also misidentified as OV2640 the entire time this
log was written — it's actually an **OV3660** (confirmed by reading the label on the
camera module's ribbon cable). This may have contributed to confusion but is NOT what
caused the "hang," since even Arduino's correct, auto-detecting OV3660 driver looked
silent at first for the exact same one-shot-print reason.

**Current real state and the actual open question (a genuine, different, not-yet-solved
bug) are in `PROJECT_STATUS.md`.** Short version: mid-porting the working OV3660 setup
to Rust, hitting an I2C `AcknowledgeCheckFailed(Address)` error that isn't explained by
any of pull-ups/frequency/clock-ordering (all checked against actual `esp-hal` source,
not assumed). That's the real ask right now — this file is kept only for historical
reference on the toolchain/debugging techniques used (`probe-rs`, `addr2line`, register
decoding), not as an active problem description.
---


## UPDATE 8: removed the RMT/WS2812 LED setup entirely, kept full camera code — still silent

Ran the pending test exactly as specified: removed the RMT channel setup, `.with_pin(GPIO48)`, and both LED `transmit()` calls (in the initial off-write and in the motion loop) from `main.rs`, keeping every line of camera code (LCD-CAM config, `AsyncCameraDriver::new`, I2C/SCCB, `Ov2640::check_id`/`init`, the DMA capture loop) completely unchanged. Built `--release` (clean, no warnings besides now-unused nothing), flashed, unplug/replugged, listened on the serial port for 20+ seconds.

**Still completely silent** — no boot print, nothing at all.

This means the camera code hangs/crashes on its own, independent of whether the LED/RMT code precedes it. So there isn't a special LED-vs-camera interaction — the camera driver code itself doesn't get through cleanly in isolation either. (Note: since the LED code is gone, this is *not* the same GPIO48-assertion crash from UPDATE 6 — that exact code no longer exists in this build. This is a fresh unknown failure point within the camera-only code, not yet re-traced with the debugger.)

## Ask

Given camera-only (no LED at all) still hangs/crashes silently, do you want us to re-run the `probe-rs` debugger trace (break at universal `panic_fmt` entry `0x420170c4`, read `a0`, mask/resolve with `addr2line`) against *this* camera-only build to find its actual failure point? Or is there a more targeted code-level test you'd want first (e.g., stub out the DMA capture loop and stop right after `AsyncCameraDriver::new`/`Ov2640::init` to isolate driver construction from the actual capture loop)?


## UPDATE 7: clean incremental bisection from the known-good baseline

Instead of continuing to reverse-engineer the fully-loaded diagnostic build, reverted `firmware/` to the exact last-known-working commit (`305d75f`, pure PIR/LED, no camera anything) and added pieces back one at a time, flashing/testing on real hardware at each step:

1. **Baseline alone** (unchanged from `305d75f`) — works. `motion detected`/`motion stopped` print fine.
2. **Baseline + the two static buffers only** (`DMA_CHUNK: [u8; 4096]`, `JPEG_OUT: [u8; 16384]`, both genuinely initialized via `StaticCell::init_with` so they can't be dead-code-eliminated, no camera dependency in `Cargo.toml` at all) — **works fine.** LED/PIR loop runs normally. This rules out the static-buffer-size/RAM-pressure theory from UPDATE 6 in isolation.
3. **Baseline + statics + the `esp32s3-cam-async` dependency in Cargo.toml, still zero camera driver code referenced anywhere** — **works fine.** Binary size identical to step 2 (linker strips the unused dependency entirely).
4. **Baseline + statics + dependency + the actual camera driver code** (LCD-CAM/DMA/I2C/OV2640 init + capture loop, same as the version that crashed before) — **reproduces the exact same silent failure.**

So it's conclusively the camera driver code itself (not just its presence as a dependency, not just the buffer sizes) that triggers this — even though, per the earlier `addr2line`-verified backtrace (UPDATE 6), the actual crash address is in our LED setup (`main.rs:104`, `.with_pin(peripherals.GPIO48)`), which runs *before* any camera code in source order. So including the camera driver code changes something about how the *already-executed, unchanged* LED setup code behaves — most likely a link-time/memory-layout effect from the additional monomorphized code and data (OV2640 register tables, DMA/LCD-CAM driver state, embedded-hal trait impls, etc.) that the actual camera code execution pulls in, as opposed to merely declaring the dependency.

Side note: also confirmed the missing early boot `println!`s (`"boot: peripherals initialized"` etc.) are a **benign, expected timing quirk**, not a crash symptom — they fire before our host-side serial reader has time to attach after replug, and get lost with no buffering (later prints, like `motion stopped`, show up fine once the reader has had time to connect). Not evidence of anything wrong.

## Ask

Given steps 1-3 above rule out simple static memory footprint and mere dependency presence, and step 4 conclusively pins it on the actual camera driver code being compiled in and reached — is there a way to bisect *within* the camera driver code itself for its effect on unrelated prior code (e.g., binary size/section-placement diffing between the step-3 build and step-4 build, to see exactly what grew/moved)? We have the exact working (step 3) and broken (step 4) release ELFs available to diff if that's useful (e.g. via `nm`/section-size comparison). Also open to trying `cargo bloat` or a linker map comparison if you think that's the fastest path from here.

Both ELFs are saved and available right now at:
- `/Users/mac/camera-project/bisection-artifacts/step3-working-dep-and-statics-only`
- `/Users/mac/camera-project/bisection-artifacts/step4-broken-with-camera-code`


## UPDATE 6: definitive panic site — it's in our WS2812 setup, not the camera code

Followed your exact test: `break *0x420170c4` (universal `panic_fmt` entry — confirmed hit, PC read back as `0x420170c4`), then read `a0 = 0x820147a5`. Applied the Xtensa call-window correction (`(a0 & 0x3FFFFFFF) | 0x40000000`, verified against the earlier known-good `0x820170E0 -> 0x420170E0` case) to get the real caller: `0x420147a5`. Resolved with `addr2line -Cfipe`:

```
<esp_hal::gpio::AnyPin>::set_output_enable at esp-hal-1.1.1/src/gpio/mod.rs:1853
 (inlined by) <esp_hal::gpio::interconnect::OutputSignal>::set_output_enable at .../interconnect.rs:892
 (inlined by) <esp_hal::rmt::Channel<Blocking, Tx>>::with_pin::<GPIO48> at .../rmt.rs:1309
 (inlined by) firmware main.rs:104
```

`main.rs:104` is `.with_pin(peripherals.GPIO48)` on the RMT TX channel — **the already-proven-working WS2812 LED setup from milestone 3, completely unchanged.** The actual source at `gpio/mod.rs:1853`:
```rust
pub(crate) fn set_output_enable(&self, enable: bool) {
    assert!(self.is_output() || !enable);   // <- this assertion fails
    self.bank().write_out_en(self.mask(), enable);
}
```
`self.is_output()` returns `false` for GPIO48 here, which never happened in the verified milestone-3 build with the identical code.

This crash happens **before any camera code executes at all** (the camera block is further down in `main()`, never reached). The only differences from the known-good milestone-3 binary: our added boot `println!`s + a blocking 5s `Delay`, and ~20KiB of new `static` buffers (`JPEG_OUT` at 16KiB, `DMA_CHUNK` at 4KiB) sitting unused in `.bss`. This lines up with your very first hypothesis (tight RAM margin, ~10KiB below the DRAM boundary) possibly corrupting unrelated HAL/GPIO driver state — not a camera-code bug at all, and not the `slice_index_fail`/DMA-peek theory from UPDATE 5 (that was based on unverified candidate call sites; this is the actual confirmed one).

## Ask

Does `esp-hal`'s GPIO pin-bank tracking (whatever `self.is_output()`/`self.bank()` reads — likely some small static bitmask struct) live in a memory region that could be corrupted or mis-initialized if `.bss` is under RAM pressure? Should we test whether *just declaring* the `JPEG_OUT`/`DMA_CHUNK` statics (without any camera code executing) is enough to reproduce this by itself — e.g., temporarily removing the whole camera block but keeping the two `static` declarations in a minimal test? That would directly confirm whether static memory footprint alone (independent of any camera runtime code) is what's breaking GPIO48 setup.


## UPDATE 5: exact panic site found via addr2line (not just the debugger)

The `break *0x420170e0` breakpoint never triggered — that address is a *return address* (from register `a0`), never actually executed since panic functions diverge. Rather than more live debugger round-trips, used the toolchain's own `xtensa-esp32s3-elf-addr2line -i` (inline-aware) directly against `target/xtensa-esp32s3-none-elf/release/firmware` to resolve every call site into the three `slice_index_fail` variants (`do_panic`, `s0_do_panic`, `s1_do_panic` — found via `nm`). All six call sites are within one function: `firmware::__main::____embassy_main_task::____embassy_main_task_inner_function` (i.e., our compiled `async fn main`), specifically in the address range `0x42015bc1`–`0x42015c2e`.

**5 of 6 call sites resolve through the identical inline chain:**
```
<core::ops::range::RangeFrom<usize>/Range<usize> as SliceIndex<[u8]>>::index
  <- esp_hal::dma::buffers::DmaRxStreamBufView::peek_internal
       (esp-hal-1.1.1/src/dma/buffers.rs lines 1388, 1401, 1427, 1428)
  <- DmaRxStreamBufView::peek_until_eof (buffers.rs:1328)
  <- esp32s3_cam_async::capture::CameraCapture::next_chunk::{closure#0}
       (esp32s3-cam-async-0.1.0/src/capture.rs:210)
  <- firmware main.rs:169 (our `capture.next_chunk(dma_chunk).await` call)
```
**The 6th** resolves to our own `&jpeg_out[..frame_len]` at `main.rs:179`, via a different, unrelated inline path (`[u8; 4096] as Index<RangeTo<usize>>`).

Given 5-of-6 point to the same chain, and it's reached on the very *first* DMA read (line 169, before line 179 is ever reached), this looks like the real culprit: **something inside `esp-hal`'s own `DmaRxStreamBufView::peek_internal`/`peek_until_eof` (buffers.rs) is indexing out of bounds**, called via `esp32s3-cam-async`'s `CameraCapture::next_chunk`. Possibly a mismatch between our `DMA_SIZE = 4096` / `DMA_BLOCK = 512` config and what these internal ring-buffer view functions expect (alignment? a required relationship between the two constants? a known issue with this exact esp-hal/esp32s3-cam-async version pairing?).

## Ask

Can you check `esp-hal-1.1.1/src/dma/buffers.rs` around lines 1328–1428 (`peek_internal`/`peek_until_eof` on `DmaRxStreamBufView`) against our `dma_rx_stream_buffer!(DMA_SIZE, DMA_BLOCK)` call with `DMA_SIZE = 4096, DMA_BLOCK = 512`? Is there a required relationship between these two constants (e.g. must divide evenly, minimum block count, alignment requirement) that we're violating? This would also explain why nothing ever printed to serial — if prints before this point aren't flushed until the executor yields/USB is serviced, and we crash before that happens (compounded by our blocking 5-second `Delay::delay_millis` early in `main`), all that buffered output would be lost even though the `println!` calls did execute.


## UPDATE 4: got a real debugger attached (probe-rs), here's what it shows

Installed `probe-rs-tools` and attached via the ESP32-S3's built-in USB-JTAG (`probe-rs debug --chip esp32s3 --exe target/xtensa-esp32s3-none-elf/release/firmware`, run interactively by the user in their own terminal since it needs a real TTY). This is against the JPEG_BUF_SIZE=16KiB / 5s-delay diagnostic build from UPDATE 3.

**Frame 0** (the actual halted location): `firmware/src/bin/main.rs:46` — that's the `loop {}` inside our own panic handler:
```rust
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");   // line 45
    loop {}                       // line 46 <- halted here
}
```
So the panic handler did reach its intended final resting state. Register values at this frame:
- SP (a1) = `0x3FCDB370`
- PC = `0x42016176`
- a0/LR = `0x820170E0`
- a6 = `0x00000100` (256), a12 = `0x00000004` (4) — possibly argument remnants, unclear.

**The backtrace beyond frame 0 is corrupted/bogus**: frames 1 through 500 (the tool's max depth) are ALL identically `panic_fmt` at `core/src/panicking.rs:80:14` — literally the same function/line repeated 500 times. That's not a real call chain; it looks like the stack unwinder walking corrupted/overwritten stack memory, consistent with an actual stack overflow having happened before this panic (the overflow itself likely corrupted the return-address chain, so we can't unwind back to find the real original panic site/message this way).

**Couldn't retrieve the panic message directly**: `p info` → "No variable named Named(\"info\") found for frame: \"panic\"" (probably optimized away/out of scope in release mode). `info locals` at frame 0 → blank.

Full raw backtrace (91,014 lines, all 500 frames) saved at `firmware/backtrace.yaml` if you want the complete register dumps per frame.

## Ask

Given SP=`0x3FCDB370` at the point our panic handler settles into its loop — is that within or already dangerously close to/past this project's actual stack bounds for the memory.x/linker config `esp-generate` produced? You'd have better visibility into the exact stack region size than we do (you'd previously computed roughly `.bss: ~140KiB, heap: ~72KiB, reserved stack: ~182KiB`). Also: is there a way to get the actual panic message text out via probe-rs given the local variable isn't resolvable normally (e.g., reading raw memory at a computed address for the `PanicInfo`/message bytes, or a way to force the compiler to not optimize it away for this diagnostic build)? We have full interactive probe-rs access now if you want us to run specific `x` (examine memory) or other commands.


## UPDATE 3: shrunk JPEG_BUF_SIZE to 16KiB — still silent

Changed only `const JPEG_BUF_SIZE: usize = 16 * 1024;` (was 128KiB), kept everything else (camera code, 5s delay, `init_with`). Built `--release`, flashed (image size unchanged at 123,920 bytes, as expected since `.bss` isn't stored in the flash image), unplug/replugged, listened 18+ seconds.

**Still completely silent, no boot print.**

Per your branching: this doesn't confirm the RAM-layout/JPEG_OUT-size hypothesis — next step per your own plan is to bisect the 4KiB DMA macro allocation (`dma_rx_stream_buffer!(DMA_SIZE, DMA_BLOCK)`). Haven't touched that yet, waiting for your specific guidance on what exactly to test there (e.g., shrink `DMA_SIZE`/`DMA_BLOCK` further, or temporarily swap the macro call for a same-sized plain static to isolate the macro itself vs. just its size).

## UPDATE 2: 5s delay test — failure is before the first statement

Added the exact test you suggested:
```rust
println!("boot: peripherals initialized");
let delay = esp_hal::delay::Delay::new();
delay.delay_millis(5_000);
```
Built `--release`, flashed, unplug/replugged, listened for 18+ seconds (well past the 5s window). **Still completely silent, no boot print ever appeared.**

Per your own framing: this rules out "camera/DMA setup disrupts USB *after* the earlier print" and confirms "the failure genuinely occurs before the first statement, likely from memory layout/startup rather than camera execution." So this is not about anything camera-related executing — something about the compiled/linked binary itself (with the camera code present anywhere in the source, regardless of whether it runs) prevents `main()` from ever reaching its first line.

## UPDATE: init_with fix applied, still silent

Applied your `StaticCell::init_with` fix exactly as given:
```rust
let dma_chunk = DMA_CHUNK.init_with(|| [0u8; DMA_SIZE]);
let jpeg_out = JPEG_OUT.init_with(|| [0u8; JPEG_BUF_SIZE]);
```
Built clean in `--release`, flashed, unplug/replugged for a clean boot, listened on `/dev/cu.usbmodem1101` for 24+ seconds. **Still completely silent** — not even the very first `println!("boot: peripherals initialized")`.

So the `init()` vs `init_with()` stack-construction issue, while real and correctly identified, was not the (only) cause of Issue B below. There may be another large value getting stack-constructed somewhere in the camera init path — e.g. `Ov2640Config { ..Default::default() }`, `JpegStreamParser::default()`, or something inside `AsyncCameraDriver::new`'s internals — that we don't have visibility into without reading the crate's actual struct definitions/sizes. Worth checking whether any of those types embed a large inline array (e.g. an OV2640 register-init table stored as `[u8; N]`/similar inside the config struct itself rather than as a `&'static` reference) that would get stack-built the same way `JPEG_OUT` did.


## Where we are

Full bisection completed per your suggested diagnostic plan:

1. Reset button alone (no reflash) — silent.
2. Full USB power-cycle — silent.
3. Dependency-only build (last verified PIR/LED `main.rs`, zero camera imports/statics/code, `esp32s3-cam-async` still listed in `Cargo.toml`) in **debug** — silent.
4. `cargo clean` + rebuild, `espflash save-image` vs `espflash read-flash` + `cmp` on that same dependency-only debug build — **confirmed the correct bytes are on the chip.** The only differences were the ESP image header's flash-size/freq byte (expected, since `save-image` and live auto-detection default differently) and the trailing 32-byte SHA-256 hash (which necessarily changes when that header byte does). All actual code/data bytes were identical. This rules out "stale/wrong image flashed."
5. Same dependency-only source, built `--release` instead of `dev` — **works.** Got `motion stopped` in the log, PIR/LED loop alive. Confirms your third hypothesis: debug-build-specific, code-layout-sensitive (almost certainly a stack overflow — debug builds have much larger stack frames from disabled inlining, and this firmware already has `#![deny(clippy::large_stack_frames)]`, suggesting stack headroom was already tight).
6. Restored the **actual** camera capture code (SCCB probe, `AsyncCameraDriver::new`, DMA capture loop — everything in `firmware/src/bin/main.rs`'s camera block) and built+flashed that in `--release` too.
7. **Still completely silent** — not even `println!("boot: peripherals initialized")`, the very first line after `esp_hal::init()`.

## What this narrows down to

Two separate issues, now cleanly separated:
- **Issue A (solved):** debug-build stack overflow from merely having `esp32s3-cam-async` as a dependency, even unused. Fixed by `--release`.
- **Issue B (open, this is the real blocker):** with the actual camera init/capture code executing, it's silent **even in `--release`.** Since release fixed issue A, this is not the same stack-overflow class of bug — something in the real camera code path (SCCB/I2C init, `AsyncCameraDriver::new`, or the DMA capture loop) is hanging or crashing before or during execution, before reaching even the very first `println!` in `main()` (which executes unconditionally, before any camera code, in source order).

## Current code

`firmware/src/bin/main.rs` has the full camera block (see file directly — it's committed to the working tree, uncommitted in git). Camera init happens after the already-proven PIR/LED setup, in a scoped block, using the exact pin map from your milestone doc and the crate's own `capture_jpeg.rs` example (adapted: plain `println!` instead of `defmt`, one-shot instead of infinite loop, `Resolution::Vga` instead of `Uxga`, `StaticCell`-backed buffers instead of task-stack arrays).

## Ask

Since even the unconditional first boot print doesn't appear once the camera code block exists in the same function (regardless of release mode), and we've ruled out stale images and the debug-only stack overflow — what else could cause a **release-mode** silent hang before `main()`'s very first instruction, purely from having this camera code present later in the same function? Possible leads to check: whether `AsyncCameraDriver::new`/the LCD-CAM peripheral setup could be doing something at a point that isn't actually "later" in the compiled binary (e.g., static initializers, `#[esp_hal::ram]`-placed statics colliding with something), or whether resource/pin conflicts (e.g., I2C0 or LCD_CAM peripheral clock setup) could brown out or hang the chip even before user code visibly starts. No logic analyzer/scope available on this end — need a specific next test to run, same as before.

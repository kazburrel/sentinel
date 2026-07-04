//! Records a fixed-duration burst of VGA JPEG frames straight into PSRAM
//! (via `PsramRecorder`), with no serial output at all during recording --
//! only after recording stops does it export the whole clip as one raw
//! binary dump (not hex), which is roughly 2x more serial-efficient than
//! the hex approach used by `mjpeg_test.rs`/`uxga_video_test.rs`.
//!
//! This is the standalone recorder test (not PIR-triggered yet) -- confirms
//! record-then-export works before wiring it to motion events.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_println::{println, Printer};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::recorder::PsramRecorder;
use static_cell::StaticCell;

extern crate alloc;

// Per-frame scratch buffer in internal RAM -- capture_jpeg_continuous writes
// here first, then we copy into PSRAM. Keeps the DMA-adjacent buffer in
// internal RAM as recommended (PSRAM DMA alignment isn't a concern here
// since this is a plain CPU copy, not a DMA transfer into PSRAM).
const SCRATCH_BUF_SIZE: usize = 64 * 1024;
static SCRATCH_BUF: StaticCell<[u8; SCRATCH_BUF_SIZE]> = StaticCell::new();

const RECORD_DURATION: Duration = Duration::from_secs(5);

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    println!("boot: peripherals initialized");

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
    let _ = spawner;

    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (psram_ptr, psram_size) = psram.raw_parts();
    println!("PSRAM: {psram_size} bytes available for recording");

    let mut recorder = unsafe { PsramRecorder::new(psram_ptr, psram_size) };
    let scratch = SCRATCH_BUF.init_with(|| [0u8; SCRATCH_BUF_SIZE]);

    let camera = CameraHandle::new(
        peripherals.LCD_CAM,
        peripherals.DMA_CH0,
        peripherals.I2C0,
        peripherals.GPIO15, // MCLK
        peripherals.GPIO13, // PCLK
        peripherals.GPIO6,  // VSYNC
        peripherals.GPIO7,  // HREF
        peripherals.GPIO11, // D0
        peripherals.GPIO9,  // D1
        peripherals.GPIO8,  // D2
        peripherals.GPIO10, // D3
        peripherals.GPIO12, // D4
        peripherals.GPIO18, // D5
        peripherals.GPIO17, // D6
        peripherals.GPIO16, // D7
        peripherals.GPIO4,  // SDA
        peripherals.GPIO5,  // SCL
        Framesize::Vga,
    )
    .await;

    let mut camera = match camera {
        Ok(c) => c,
        Err(e) => loop {
            println!("CameraHandle::new FAILED: {e:?}");
            Timer::after(Duration::from_secs(10)).await;
        },
    };

    println!(
        "recording for {}s into PSRAM (no serial output until done)...",
        RECORD_DURATION.as_secs()
    );

    let mut fail_count: u32 = 0;
    let record_start = Instant::now();

    while record_start.elapsed() < RECORD_DURATION {
        match camera.capture_jpeg_continuous(scratch).await {
            Ok(len) => {
                let timestamp_ms = record_start.elapsed().as_millis() as u32;
                if recorder.record_frame(&scratch[..len], timestamp_ms).is_err() {
                    println!("PSRAM full at {} frames -- stopping early", recorder.frame_count());
                    break;
                }
            }
            Err(_) => fail_count += 1,
        }
    }

    let elapsed = record_start.elapsed();
    let elapsed_secs = elapsed.as_millis() as f32 / 1000.0;
    let camera_fps = if elapsed_secs > 0.0 {
        recorder.frame_count() as f32 / elapsed_secs
    } else {
        0.0
    };

    println!(
        "recording done: {} frames, {} failures, {:.2}s, {:.2} FPS, {} of {} PSRAM bytes used",
        recorder.frame_count(),
        fail_count,
        elapsed_secs,
        camera_fps,
        recorder.bytes_used(),
        recorder.capacity()
    );

    println!("RAW EXPORT BEGIN {}", recorder.bytes_used());
    Printer::write_bytes(recorder.recorded_bytes());
    println!();
    println!("RAW EXPORT END");

    // Report repeatedly forever, not once -- a one-shot print can be missed
    // by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "still alive: frames={} failures={fail_count} fps={camera_fps:.2}",
            recorder.frame_count()
        );
    }
}

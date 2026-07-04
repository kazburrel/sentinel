//! Continuous-capture benchmark: reuses `CameraHandle` to capture JPEG frames
//! back-to-back for a fixed duration, measuring frame count/size/failures and
//! achieved FPS. No hex-dumping during the capture window -- printing during
//! a live capture loop is known to starve the DMA (see `PROJECT_STATUS.md`,
//! Milestone 4, Blocker #2b). Not PIR-triggered; this is a standalone stress
//! test of repeated `capture_jpeg()` calls, run on boot.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use static_cell::StaticCell;

extern crate alloc;

const JPEG_BUF_SIZE: usize = 64 * 1024;
static JPEG_BUF: StaticCell<[u8; JPEG_BUF_SIZE]> = StaticCell::new();

const CAPTURE_WINDOW: Duration = Duration::from_secs(10);

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

    let jpeg_buf = JPEG_BUF.init_with(|| [0u8; JPEG_BUF_SIZE]);

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

    println!("starting continuous capture for {}s", CAPTURE_WINDOW.as_secs());

    let mut frame_count: u32 = 0;
    let mut fail_count: u32 = 0;
    let mut total_bytes: u64 = 0;
    let mut min_len: usize = usize::MAX;
    let mut max_len: usize = 0;

    let start = Instant::now();
    while start.elapsed() < CAPTURE_WINDOW {
        match camera.capture_jpeg_continuous(jpeg_buf).await {
            Ok(len) => {
                frame_count += 1;
                total_bytes += len as u64;
                min_len = min_len.min(len);
                max_len = max_len.max(len);
            }
            Err(_) => fail_count += 1,
        }
    }
    let elapsed = start.elapsed();

    let elapsed_secs = elapsed.as_millis() as f32 / 1000.0;
    let fps = frame_count as f32 / elapsed_secs;
    let avg_len = if frame_count > 0 {
        total_bytes / frame_count as u64
    } else {
        0
    };
    if frame_count == 0 {
        min_len = 0;
    }

    println!("capture window complete: {elapsed_secs:.2}s elapsed");
    println!("frames captured: {frame_count}, failures: {fail_count}");
    println!("frame size: min={min_len} avg={avg_len} max={max_len} bytes");
    println!("achieved FPS: {fps:.2}");

    // Report repeatedly forever, not once -- a one-shot print can be missed
    // by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "still alive: frames={frame_count} failures={fail_count} fps={fps:.2}"
        );
    }
}

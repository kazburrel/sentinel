//! Thin hardware test for `firmware::camera::CameraHandle` -- verifies the
//! reusable capture module works on real hardware before it gets wired up
//! to PIR-triggered capture in the real `main.rs`. See `PROJECT_STATUS.md`
//! (Milestone 4) for the full history of how this camera path was debugged.
//!
//! Wiring (Freenove ESP32-S3 CAM, confirmed working pin map):
//! - SIOD  => GPIO4
//! - SIOC  => GPIO5
//! - XCLK  => GPIO15
//! - VSYNC => GPIO6
//! - HREF  => GPIO7
//! - PCLK  => GPIO13
//! - D0..D7 => GPIO11, GPIO9, GPIO8, GPIO10, GPIO12, GPIO18, GPIO17, GPIO16

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use firmware::camera::CameraHandle;
use firmware::hexdump::print_hex_dump;
use static_cell::StaticCell;

extern crate alloc;

const JPEG_BUF_SIZE: usize = 64 * 1024;
static JPEG_BUF: StaticCell<[u8; JPEG_BUF_SIZE]> = StaticCell::new();

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
    )
    .await;

    let frame_len = match camera {
        Err(e) => {
            println!("CameraHandle::new FAILED: {e:?}");
            0
        }
        Ok(mut camera) => match camera.capture_jpeg(jpeg_buf).await {
            Ok(len) => {
                println!("capture_jpeg succeeded: {len} bytes");
                len
            }
            Err(e) => {
                println!("capture_jpeg FAILED: {e:?}");
                0
            }
        },
    };

    // Report/dump repeatedly forever, not once -- a one-shot print can be
    // missed by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        if frame_len == 0 {
            println!("still alive: capture failed, see error above");
            continue;
        }
        print_hex_dump(&jpeg_buf[..frame_len]);
    }
}

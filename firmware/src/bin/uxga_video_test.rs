//! Same idea as `mjpeg_test.rs` (capture N frames back-to-back, hex-dump
//! each one right after capture, reusing `CameraHandle`) but at UXGA
//! (1600x1200) instead of VGA. Frame count is lower than `mjpeg_test.rs`'s
//! 40 -- a UXGA frame is ~5x the bytes of a VGA frame, so hex-dumping it
//! over 115200-baud serial takes proportionally longer per frame.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use firmware::camera::CameraHandle;
use firmware::hexdump::print_hex_dump;
use firmware::ov3660::Framesize;
use static_cell::StaticCell;

extern crate alloc;

const JPEG_BUF_SIZE: usize = 256 * 1024;
static JPEG_BUF: StaticCell<[u8; JPEG_BUF_SIZE]> = StaticCell::new();

const FRAME_COUNT: u32 = 8;

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
        Framesize::Uxga,
    )
    .await;

    let mut camera = match camera {
        Ok(c) => c,
        Err(e) => loop {
            println!("CameraHandle::new FAILED: {e:?}");
            Timer::after(Duration::from_secs(10)).await;
        },
    };

    println!("MJPEG CAPTURE START {FRAME_COUNT}");

    let mut frame_count: u32 = 0;
    let mut fail_count: u32 = 0;
    let mut capture_time = Duration::from_ticks(0);

    for _ in 0..FRAME_COUNT {
        let t0 = Instant::now();
        let result = camera.capture_jpeg_continuous(jpeg_buf).await;
        capture_time += t0.elapsed();

        match result {
            Ok(len) => {
                frame_count += 1;
                print_hex_dump(&jpeg_buf[..len]);
            }
            Err(e) => {
                fail_count += 1;
                println!("frame FAILED: {e:?}");
            }
        }
    }

    let capture_secs = capture_time.as_millis() as f32 / 1000.0;
    let camera_fps = if capture_secs > 0.0 {
        frame_count as f32 / capture_secs
    } else {
        0.0
    };

    println!("MJPEG CAPTURE DONE frames={frame_count} failures={fail_count}");
    println!("camera-only capture time: {capture_secs:.2}s, camera FPS: {camera_fps:.2}");

    // Report repeatedly forever, not once -- a one-shot print can be missed
    // by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "still alive: frames={frame_count} failures={fail_count} camera_fps={camera_fps:.2}"
        );
    }
}

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use firmware::camera::CameraHandle;
use firmware::hexdump::print_hex_dump;
use firmware::pir::{MotionEdge, MotionSensor};
use firmware::ws2812::ws2812_frame;
use static_cell::StaticCell;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern crate alloc;

const JPEG_BUF_SIZE: usize = 64 * 1024;
static JPEG_BUF: StaticCell<[u8; JPEG_BUF_SIZE]> = StaticCell::new();

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.3.0
    // generator parameters: --chip esp32s3 -o wifi -o embassy -o alloc -o unstable-hal

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // TODO: Spawn some tasks
    let _ = spawner;

    let pir = Input::new(peripherals.GPIO21, InputConfig::default());

    let jpeg_buf = JPEG_BUF.init_with(|| [0u8; JPEG_BUF_SIZE]);
    let mut camera = CameraHandle::new(
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
    .await
    .unwrap();

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let tx_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true)
        .with_carrier_modulation(false)
        .with_memsize(1);
    let mut channel = rmt
        .channel0
        .configure_tx(&tx_config)
        .unwrap()
        .with_pin(peripherals.GPIO48);

    let off = ws2812_frame(0, 0, 0);
    channel = channel.transmit(&off).unwrap().wait().unwrap();

    let mut motion = MotionSensor::new(pir.is_high());
    loop {
        match motion.update(pir.is_high()) {
            Some(MotionEdge::Detected) => {
                println!("motion detected");
                let red = ws2812_frame(8, 0, 0);
                channel = channel.transmit(&red).unwrap().wait().unwrap();

                match camera.capture_jpeg(jpeg_buf).await {
                    Ok(len) => {
                        println!("photo captured: {len} bytes");
                        print_hex_dump(&jpeg_buf[..len]);
                    }
                    Err(e) => println!("photo capture FAILED: {e:?}"),
                }
            }
            Some(MotionEdge::Stopped) => {
                println!("motion stopped");
                let off = ws2812_frame(0, 0, 0);
                channel = channel.transmit(&off).unwrap().wait().unwrap();
            }
            None => {}
        }
        Timer::after(Duration::from_millis(50)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.1.0/examples
}

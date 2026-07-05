//! Motion-triggered PSRAM recording: on PIR motion, turns the LED red,
//! records a fixed 5-second VGA clip straight into PSRAM
//! (`PsramRecorder`/`capture_jpeg_continuous`, same as `psram_record_test.rs`),
//! then exports the clip as a raw binary dump, turns the LED off, resets the
//! recorder, and goes back to waiting for the next motion event. Verifies
//! multiple recordings can happen in one boot before this gets folded into
//! `main.rs`.

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level};
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::{println, Printer};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::pir::{MotionEdge, MotionSensor};
use firmware::recorder::PsramRecorder;
use firmware::ws2812::ws2812_frame;
use static_cell::StaticCell;

extern crate alloc;

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

    let pir = Input::new(peripherals.GPIO21, InputConfig::default());

    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (psram_ptr, psram_size) = psram.raw_parts();
    println!("PSRAM: {psram_size} bytes available for recording");
    let mut recorder = unsafe { PsramRecorder::new(psram_ptr, psram_size) };
    let scratch = SCRATCH_BUF.init_with(|| [0u8; SCRATCH_BUF_SIZE]);

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
        Framesize::Vga,
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

    let mut recording_count: u32 = 0;
    let mut motion = MotionSensor::new(pir.is_high());
    let mut last_heartbeat = Instant::now();
    loop {
        if last_heartbeat.elapsed() >= Duration::from_secs(10) {
            println!("still alive: waiting for motion, recordings so far={recording_count}");
            last_heartbeat = Instant::now();
        }
        match motion.update(pir.is_high()) {
            Some(MotionEdge::Detected) => {
                recording_count += 1;
                println!("motion detected -- starting recording #{recording_count}");
                let red = ws2812_frame(8, 0, 0);
                channel = channel.transmit(&red).unwrap().wait().unwrap();

                recorder.reset();
                let mut fail_count: u32 = 0;
                let record_start = Instant::now();

                while record_start.elapsed() < RECORD_DURATION {
                    match camera.capture_jpeg_continuous(scratch).await {
                        Ok(len) => {
                            let timestamp_ms = record_start.elapsed().as_millis() as u32;
                            if recorder.record_frame(&scratch[..len], timestamp_ms).is_err() {
                                println!(
                                    "PSRAM full at {} frames -- stopping early",
                                    recorder.frame_count()
                                );
                                break;
                            }
                        }
                        Err(_) => fail_count += 1,
                    }
                }

                let elapsed_secs = record_start.elapsed().as_millis() as f32 / 1000.0;
                println!(
                    "recording #{recording_count} done: {} frames, {fail_count} failures, {elapsed_secs:.2}s, {} bytes",
                    recorder.frame_count(),
                    recorder.bytes_used()
                );

                println!("RAW EXPORT BEGIN {}", recorder.bytes_used());
                Printer::write_bytes(recorder.recorded_bytes());
                println!();
                println!("RAW EXPORT END");

                let off = ws2812_frame(0, 0, 0);
                channel = channel.transmit(&off).unwrap().wait().unwrap();
                println!("waiting for next motion event...");

                // Re-sync the debounced motion state so a still-present body
                // right after recording doesn't immediately look like a
                // fresh edge.
                motion = MotionSensor::new(pir.is_high());
            }
            Some(MotionEdge::Stopped) => {
                println!("motion stopped");
            }
            None => {}
        }
        Timer::after(Duration::from_millis(50)).await;
    }
}

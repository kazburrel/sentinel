//! Standalone WiFi scan: lists visible networks (SSID, channel, signal
//! strength) so a real network to connect to can be picked before writing
//! any connection code. No credentials needed for this one, no camera code.

#![no_std]
#![no_main]

use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::scan::ScanConfig;

extern crate alloc;

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
async fn main(_spawner: embassy_executor::Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    println!("boot: peripherals initialized");

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    // Scheduler MUST start before initializing the radio.
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let (mut controller, _interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).unwrap();

    loop {
        println!("scanning...");
        let scan_config = ScanConfig::default().with_max(20);
        match controller.scan_async(&scan_config).await {
            Ok(mut aps) => {
                aps.sort_by_key(|ap| -(ap.signal_strength as i16));
                println!("found {} network(s):", aps.len());
                for ap in &aps {
                    println!(
                        "  {:<32} ch={:<2} rssi={:<4} auth={:?}",
                        ap.ssid.as_str(),
                        ap.channel,
                        ap.signal_strength,
                        ap.auth_method
                    );
                }
            }
            Err(e) => println!("scan FAILED: {e:?}"),
        }
        Timer::after(Duration::from_secs(15)).await;
    }
}

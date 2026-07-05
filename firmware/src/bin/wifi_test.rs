//! Standalone WiFi connectivity smoke test -- confirms the ESP32-S3 can join
//! a real network and get an IP address via DHCP, before any HTTP/server
//! work gets built on top. No camera code here at all.
//!
//! Requires `firmware/src/wifi_credentials.rs` (gitignored, not committed --
//! copy `wifi_credentials.rs.example` next to it and fill in your real
//! SSID/password).

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_net::{Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface};
use static_cell::StaticCell;

extern crate alloc;

include!("../wifi_credentials.rs");

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
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
    // Scheduler MUST start before initializing the radio.
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let (mut controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).unwrap();

    let sta_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );
    controller.set_config(&sta_config).unwrap();
    println!("WiFi controller started, connecting to \"{WIFI_SSID}\"...");

    loop {
        match controller.connect_async().await {
            Ok(_) => break,
            Err(e) => {
                println!("connect failed: {e:?}, retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
    println!("WiFi connected");

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let resources = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(interfaces.station, net_config, resources, 0x1234_5678);

    spawner.spawn(net_task(runner).unwrap());

    println!("waiting for DHCP...");
    stack.wait_config_up().await;
    let ip_config = stack.config_v4();
    println!("DHCP done: {ip_config:?}");

    // Report repeatedly forever, not once -- a one-shot print can be missed
    // by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "still alive: wifi_connected={} ip={:?}",
            controller.is_connected(),
            stack.config_v4()
        );
    }
}

//! Minimal HTTP POST test: connects to WiFi (same as `wifi_test.rs`), then
//! repeatedly POSTs a small test body to the `server` crate running on the
//! Mac, over a raw TCP socket with a hand-written HTTP/1.1 request -- no
//! HTTP client crate, since this is just proving the pipe works before any
//! real event/media upload exists.
//!
//! Requires `firmware/src/wifi_credentials.rs` (gitignored -- see
//! `wifi_credentials.rs.example`), including `SERVER_IP`/`SERVER_PORT` for
//! the Mac's `server` crate.

#![no_std]
#![no_main]

use alloc::format;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Runner, StackResources};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write as _;
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

const SOCKET_BUF_SIZE: usize = 4096;
static RX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();

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
    println!("DHCP done: {:?}", stack.config_v4());

    let rx_buf = RX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);
    let tx_buf = TX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);

    let server_ip = IpAddress::v4(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]);

    let mut post_count: u32 = 0;
    loop {
        post_count += 1;
        let body = format!("hello from esp32-s3, post #{post_count}");
        let request = format!(
            "POST /upload HTTP/1.1\r\n\
             Host: {}.{}.{}.{}:{SERVER_PORT}\r\n\
             X-Upload-Token: {UPLOAD_TOKEN}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            SERVER_IP[0],
            SERVER_IP[1],
            SERVER_IP[2],
            SERVER_IP[3],
            body.len()
        );

        let mut socket = TcpSocket::new(stack, &mut *rx_buf, &mut *tx_buf);
        match socket.connect((server_ip, SERVER_PORT)).await {
            Ok(()) => {
                println!("connected to server, sending post #{post_count}...");
                if let Err(e) = socket.write_all(request.as_bytes()).await {
                    println!("write FAILED: {e:?}");
                } else {
                    let mut resp_buf = [0u8; 512];
                    match socket.read(&mut resp_buf).await {
                        Ok(n) => {
                            let resp = core::str::from_utf8(&resp_buf[..n]).unwrap_or("<non-utf8>");
                            println!("response ({n} bytes): {resp}");
                        }
                        Err(e) => println!("read FAILED: {e:?}"),
                    }
                }
            }
            Err(e) => println!("connect FAILED: {e:?}"),
        }
        socket.abort();

        Timer::after(Duration::from_secs(10)).await;
    }
}

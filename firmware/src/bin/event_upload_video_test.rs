//! Second real-media upload test: adds a recorded video clip alongside the
//! thumbnail proven by `event_upload_test.rs`. Captures one still JPEG
//! (thumbnail), then records a fixed-duration burst of frames straight into
//! PSRAM via `PsramRecorder` (same approach as `psram_record_test.rs`), and
//! uploads both as a single two-part event envelope over WiFi.
//!
//! The video part's payload is `PsramRecorder::recorded_bytes()` verbatim --
//! the same `[frame_len: u32 LE][timestamp_ms: u32 LE][JPEG bytes]`
//! per-frame format the Rust server's `decode-raw` command already knows how to
//! turn into a playable clip, just delivered over WiFi instead of serial.
//!
//! Wiring (Freenove ESP32-S3 CAM, confirmed working pin map, see
//! `camera_test.rs`):
//! - SIOD  => GPIO4
//! - SIOC  => GPIO5
//! - XCLK  => GPIO15
//! - VSYNC => GPIO6
//! - HREF  => GPIO7
//! - PCLK  => GPIO13
//! - D0..D7 => GPIO11, GPIO9, GPIO8, GPIO10, GPIO12, GPIO18, GPIO17, GPIO16
//!
//! Requires `firmware/src/wifi_credentials.rs` (gitignored -- see
//! `wifi_credentials.rs.example`).

#![no_std]
#![no_main]

use alloc::format;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Runner, StackResources};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_hal::clock::CpuClock;
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::recorder::PsramRecorder;
use shared::{
    encode_envelope_header, encode_part_header, Encoding, PartKind, ENVELOPE_HEADER_LEN, PART_HEADER_LEN,
};
use static_cell::StaticCell;

extern crate alloc;

include!("../wifi_credentials.rs");

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

const RECORD_DURATION: Duration = Duration::from_secs(5);

// Dedicated buffer for the thumbnail frame -- must survive untouched through
// the whole recording burst, unlike SCRATCH_BUF below which gets overwritten
// once per recorded frame.
const THUMB_BUF_SIZE: usize = 64 * 1024;
static THUMB_BUF: StaticCell<[u8; THUMB_BUF_SIZE]> = StaticCell::new();

// Per-frame scratch buffer in internal RAM, reused for every frame of the
// recording burst -- capture_jpeg_continuous writes here, then we copy into
// PSRAM (see psram_record_test.rs for why this two-step copy exists).
const SCRATCH_BUF_SIZE: usize = 64 * 1024;
static SCRATCH_BUF: StaticCell<[u8; SCRATCH_BUF_SIZE]> = StaticCell::new();

static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

const SOCKET_BUF_SIZE: usize = 4096;
static RX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
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

    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (psram_ptr, psram_size) = psram.raw_parts();
    println!("PSRAM: {psram_size} bytes available for recording");
    let mut recorder = unsafe { PsramRecorder::new(psram_ptr, psram_size) };

    let thumb_buf = THUMB_BUF.init_with(|| [0u8; THUMB_BUF_SIZE]);
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
        Ok(camera) => camera,
        Err(e) => {
            println!("CameraHandle::new FAILED: {e:?}, halting");
            loop {
                Timer::after(Duration::from_secs(10)).await;
                println!("still alive: camera init failed, see error above");
            }
        }
    };
    println!("camera initialized");

    let wifi_interface = Interface::station();
    let mut controller = WifiController::new(peripherals.WIFI, Default::default()).unwrap();

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
    let (stack, runner) = embassy_net::new(wifi_interface, net_config, resources, 0x1234_5678);

    spawner.spawn(net_task(runner).unwrap());

    println!("waiting for DHCP...");
    stack.wait_config_up().await;
    println!("DHCP done: {:?}", stack.config_v4());

    let rx_buf = RX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);
    let tx_buf = TX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);

    let server_ip = IpAddress::v4(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]);

    let mut event_count: u32 = 0;
    loop {
        event_count += 1;
        let event_start = Instant::now();

        let thumb_len = match camera.capture_jpeg(thumb_buf).await {
            Ok(len) => {
                println!("event #{event_count}: thumbnail captured, {len} bytes");
                len
            }
            Err(e) => {
                println!("event #{event_count}: thumbnail capture FAILED: {e:?}, skipping event");
                Timer::after(Duration::from_secs(15)).await;
                continue;
            }
        };

        recorder.reset();
        println!("event #{event_count}: recording {}s clip into PSRAM...", RECORD_DURATION.as_secs());
        // How long after the event started (thumbnail capture kicking off)
        // recording actually began -- lets the server line up the video
        // part's timeline against the thumbnail's without guessing.
        let video_start_offset_ms = event_start.elapsed().as_millis() as u32;
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
        println!(
            "event #{event_count}: recording done: {} frames, {} failures, {} bytes",
            recorder.frame_count(),
            fail_count,
            recorder.bytes_used()
        );

        let video_duration_ms = record_start.elapsed().as_millis() as u32;
        let video_bytes = recorder.recorded_bytes();
        let envelope_header = encode_envelope_header(2, event_count as u64);
        let thumb_header = encode_part_header(PartKind::Thumbnail, Encoding::Jpeg, 0, 0, thumb_len as u32);
        let video_header = encode_part_header(
            PartKind::Video,
            Encoding::RecorderFrames,
            video_start_offset_ms,
            video_duration_ms,
            video_bytes.len() as u32,
        );
        let body_len =
            ENVELOPE_HEADER_LEN + PART_HEADER_LEN + thumb_len + PART_HEADER_LEN + video_bytes.len();

        let request_head = format!(
            "POST /upload HTTP/1.1\r\n\
             Host: {}.{}.{}.{}:{SERVER_PORT}\r\n\
             X-Upload-Token: {UPLOAD_TOKEN}\r\n\
             Content-Length: {body_len}\r\n\
             Connection: close\r\n\
             \r\n",
            SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3],
        );

        let mut socket = TcpSocket::new(stack, &mut *rx_buf, &mut *tx_buf);
        match socket.connect((server_ip, SERVER_PORT)).await {
            Ok(()) => {
                println!(
                    "connected to server, sending event #{event_count} ({thumb_len}-byte thumbnail + {}-byte video)...",
                    video_bytes.len()
                );
                let send_result: Result<(), embassy_net::tcp::Error> = async {
                    socket.write_all(request_head.as_bytes()).await?;
                    socket.write_all(&envelope_header).await?;
                    socket.write_all(&thumb_header).await?;
                    socket.write_all(&thumb_buf[..thumb_len]).await?;
                    socket.write_all(&video_header).await?;
                    socket.write_all(video_bytes).await?;
                    Ok(())
                }
                .await;

                match send_result {
                    Err(e) => println!("write FAILED: {e:?}"),
                    Ok(()) => {
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
            }
            Err(e) => println!("connect FAILED: {e:?}"),
        }
        socket.abort();

        Timer::after(Duration::from_secs(15)).await;
    }
}

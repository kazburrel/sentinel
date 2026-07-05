//! Motion-triggered PSRAM recording, doorbell-style: one motion event is one
//! clip, not a fixed-duration snippet.
//!
//! On PIR motion: LED red, record at least 5s, keep recording for as long as
//! PIR stays HIGH (no fixed maximum), then record a further 2s "tail" once
//! PIR goes LOW -- if motion returns during that tail the same event just
//! continues and the tail restarts. Only stops when PIR has genuinely
//! stopped (past the tail) or PSRAM fills up. All of that is one clip;
//! renewed motion within it never starts a second event.
//! LED off, upload the event (thumbnail + video, as the `shared` crate's
//! event envelope) to the server over WiFi, then export the same clip as a
//! raw binary dump over USB serial regardless of whether the upload
//! succeeded -- USB export is a debugging/recovery fallback, not something
//! the WiFi path's success or failure should ever gate. Wait for PIR to
//! actually go LOW before rearming (so a person who's still standing there
//! can't immediately trigger a second event).
//!
//! Still not built: pre-roll before motion, and upload retries/offline
//! queuing if the server is unreachable (deliberately deferred until this
//! single-attempt path has been proven reliable on real hardware -- see
//! `PROJECT_STATUS.md`).

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use alloc::format;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Runner, StackResources};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level};
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::{println, Printer};
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::pir::{MotionEdge, MotionSensor};
use firmware::recorder::PsramRecorder;
use firmware::ws2812::ws2812_frame;
use shared::{
    encode_envelope_header, encode_part_header, Encoding, PartKind, ENVELOPE_HEADER_LEN, PART_HEADER_LEN,
};
use static_cell::StaticCell;

extern crate alloc;

include!("../wifi_credentials.rs");

/// Set to `false` to make PIR motion a no-op -- useful while developing near
/// the board without triggering real recordings every time you move. Flip
/// and reflash to toggle; no new hardware required. Camera/PSRAM/LED still
/// initialize normally either way, only the recording action is skipped.
const RECORDING_ENABLED: bool = true;

const SCRATCH_BUF_SIZE: usize = 64 * 1024;
static SCRATCH_BUF: StaticCell<[u8; SCRATCH_BUF_SIZE]> = StaticCell::new();

const THUMBNAIL_BUF_SIZE: usize = 64 * 1024;
static THUMBNAIL_BUF: StaticCell<[u8; THUMBNAIL_BUF_SIZE]> = StaticCell::new();

/// Minimum event length, regardless of how quickly PIR drops back to LOW.
const MIN_EVENT_DURATION: Duration = Duration::from_secs(5);
/// How long to keep recording after PIR goes LOW before ending the event --
/// restarted from scratch any time PIR goes back HIGH before this expires.
/// Was 2s; raised to 5s because the AM312 PIR module has roughly a 2s
/// trigger/hold plus a 2s blocking period of its own, leaving a 2s tail
/// almost no margin against a real sensor LOW gap or missed slow movement.
const TAIL_DURATION: Duration = Duration::from_secs(5);
// No fixed maximum -- an event only ends when PIR has genuinely stopped
// (past the tail) or PSRAM fills up, however long that takes.

static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

const SOCKET_BUF_SIZE: usize = 4096;
static RX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

/// What happened trying to deliver one event over WiFi -- deliberately not
/// just "did it work", so the caller can log *why* it didn't (unreachable
/// server vs. a mid-transfer I/O error vs. the server rejecting the request
/// outright) without needing to re-derive that from raw response bytes
/// itself.
enum UploadOutcome {
    Success,
    /// The server responded, but not with `200` -- carries the parsed
    /// status code (`0` if the response couldn't even be parsed that far)
    /// for logging.
    Rejected(u16),
    ConnectFailed,
    /// A write or read failed after a connection was established (e.g. the
    /// server closed the connection mid-transfer).
    IoError,
}

/// Uploads one event (a real JPEG thumbnail + a PSRAM-recorded video clip)
/// as a single two-part `shared` crate event envelope, over one TCP
/// connection, exactly the request shape `event_upload_video_test.rs`
/// proved on real hardware. A single attempt, no retries -- retry/offline
/// handling is deliberately later work (see `PROJECT_STATUS.md`); today the
/// caller just needs to know whether this attempt succeeded so it can log
/// it, since the recording itself is preserved regardless via the USB
/// export fallback.
#[allow(clippy::too_many_arguments)]
#[allow(
    clippy::large_stack_frames,
    reason = "the request header string plus the 512-byte response buffer push this over \
    clippy's default threshold; both are short-lived, one-at-a-time, non-recursive -- the \
    same shape already proven fine on hardware in event_upload_video_test.rs"
)]
async fn upload_event(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    server_ip: IpAddress,
    event_id: u64,
    thumbnail: &[u8],
    video: &[u8],
    video_duration_ms: u32,
) -> UploadOutcome {
    let envelope_header = encode_envelope_header(2, event_id);
    let thumb_header = encode_part_header(PartKind::Thumbnail, Encoding::Jpeg, 0, 0, thumbnail.len() as u32);
    let video_header = encode_part_header(
        PartKind::Video,
        Encoding::RecorderFrames,
        0,
        video_duration_ms,
        video.len() as u32,
    );
    let body_len = ENVELOPE_HEADER_LEN + PART_HEADER_LEN + thumbnail.len() + PART_HEADER_LEN + video.len();

    let request_head = format!(
        "POST /upload HTTP/1.1\r\n\
         Host: {}.{}.{}.{}:{SERVER_PORT}\r\n\
         X-Upload-Token: {UPLOAD_TOKEN}\r\n\
         Content-Length: {body_len}\r\n\
         Connection: close\r\n\
         \r\n",
        SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3],
    );

    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
    if let Err(e) = socket.connect((server_ip, SERVER_PORT)).await {
        println!("upload: connect FAILED: {e:?}");
        socket.abort();
        return UploadOutcome::ConnectFailed;
    }

    let send_result: Result<(), embassy_net::tcp::Error> = async {
        socket.write_all(request_head.as_bytes()).await?;
        socket.write_all(&envelope_header).await?;
        socket.write_all(&thumb_header).await?;
        socket.write_all(thumbnail).await?;
        socket.write_all(&video_header).await?;
        socket.write_all(video).await?;
        Ok(())
    }
    .await;

    if let Err(e) = send_result {
        println!("upload: write FAILED: {e:?}");
        socket.abort();
        return UploadOutcome::IoError;
    }

    let mut resp_buf = [0u8; 512];
    let outcome = match socket.read(&mut resp_buf).await {
        Ok(n) if n >= 12 && &resp_buf[..12] == b"HTTP/1.1 200" => UploadOutcome::Success,
        Ok(n) => {
            let status_code = core::str::from_utf8(&resp_buf[..n])
                .ok()
                .and_then(|s| s.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            println!(
                "upload: server rejected the event (status {status_code}): {:?}",
                core::str::from_utf8(&resp_buf[..n]).unwrap_or("<non-utf8>")
            );
            UploadOutcome::Rejected(status_code)
        }
        Err(e) => {
            println!("upload: read FAILED: {e:?}");
            UploadOutcome::IoError
        }
    };
    socket.abort();
    outcome
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

    let pir = Input::new(peripherals.GPIO21, InputConfig::default());

    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (psram_ptr, psram_size) = psram.raw_parts();
    println!("PSRAM: {psram_size} bytes available for recording");
    let mut recorder = unsafe { PsramRecorder::new(psram_ptr, psram_size) };
    let scratch = SCRATCH_BUF.init_with(|| [0u8; SCRATCH_BUF_SIZE]);
    let thumbnail_buf = THUMBNAIL_BUF.init_with(|| [0u8; THUMBNAIL_BUF_SIZE]);

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

    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).unwrap();

    let sta_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );
    wifi_controller.set_config(&sta_config).unwrap();
    println!("WiFi controller started, connecting to \"{WIFI_SSID}\"...");

    loop {
        match wifi_controller.connect_async().await {
            Ok(_) => break,
            Err(e) => {
                println!("WiFi connect failed: {e:?}, retrying in 5s");
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
            Some(MotionEdge::Detected) if !RECORDING_ENABLED => {
                println!("motion detected -- recording disabled, ignoring");
            }
            Some(MotionEdge::Detected) => {
                recording_count += 1;
                println!("motion detected -- starting event #{recording_count}");
                let red = ws2812_frame(8, 0, 0);
                channel = channel.transmit(&red).unwrap().wait().unwrap();

                recorder.reset();
                let mut fail_count: u32 = 0;
                let mut thumbnail_len: usize = 0;
                let mut first_frame = true;
                let event_start = Instant::now();
                let mut tail_deadline: Option<Instant> = None;

                loop {
                    // First frame after idle needs the settling/warmup
                    // capture (sensor's auto-exposure hasn't adjusted yet);
                    // every later frame in this same event reuses the fast
                    // no-warmup path.
                    let capture_result = if first_frame {
                        camera.capture_jpeg(scratch).await
                    } else {
                        camera.capture_jpeg_continuous(scratch).await
                    };

                    match capture_result {
                        Ok(len) => {
                            if first_frame {
                                // Preserve this frame as the event's future
                                // thumbnail -- no separate photo taken, just
                                // an extra copy of the frame already in hand.
                                let copy_len = len.min(thumbnail_buf.len());
                                thumbnail_buf[..copy_len].copy_from_slice(&scratch[..copy_len]);
                                thumbnail_len = copy_len;
                                first_frame = false;
                            }

                            let timestamp_ms = event_start.elapsed().as_millis() as u32;
                            if recorder.record_frame(&scratch[..len], timestamp_ms).is_err() {
                                println!(
                                    "PSRAM full at {} frames -- stopping event early",
                                    recorder.frame_count()
                                );
                                break;
                            }
                        }
                        Err(_) => fail_count += 1,
                    }

                    // Check PIR between captured frames, not on a separate
                    // timer. Any HIGH reading cancels a pending tail
                    // deadline outright -- this is what makes renewed
                    // motion during the tail restart the countdown instead
                    // of ending the event.
                    let now = Instant::now();
                    if pir.is_high() {
                        tail_deadline = None;
                    } else if tail_deadline.is_none() {
                        tail_deadline = Some(now + TAIL_DURATION);
                    }

                    let tail_expired = tail_deadline.is_some_and(|deadline| now >= deadline);
                    let min_reached = event_start.elapsed() >= MIN_EVENT_DURATION;

                    if tail_expired && min_reached {
                        break;
                    }
                }

                let event_duration_ms = event_start.elapsed().as_millis() as u32;
                let elapsed_secs = event_duration_ms as f32 / 1000.0;
                println!(
                    "event #{recording_count} done: {} frames, {fail_count} failures, {elapsed_secs:.2}s, {} bytes, thumbnail {thumbnail_len} bytes",
                    recorder.frame_count(),
                    recorder.bytes_used()
                );

                println!("event #{recording_count}: uploading over WiFi...");
                match upload_event(
                    stack,
                    &mut *rx_buf,
                    &mut *tx_buf,
                    server_ip,
                    recording_count as u64,
                    &thumbnail_buf[..thumbnail_len],
                    recorder.recorded_bytes(),
                    event_duration_ms,
                )
                .await
                {
                    UploadOutcome::Success => {
                        println!("event #{recording_count}: WiFi upload SUCCEEDED");
                    }
                    UploadOutcome::Rejected(code) => {
                        println!("event #{recording_count}: WiFi upload FAILED -- server rejected it (status {code})");
                    }
                    UploadOutcome::ConnectFailed => {
                        println!("event #{recording_count}: WiFi upload FAILED -- could not connect to server");
                    }
                    UploadOutcome::IoError => {
                        println!("event #{recording_count}: WiFi upload FAILED -- I/O error during send/receive");
                    }
                }

                // USB export always runs regardless of the WiFi upload
                // outcome above -- a debugging/recovery fallback, not
                // something the upload's success or failure should gate.
                println!("RAW EXPORT BEGIN {}", recorder.bytes_used());
                Printer::write_bytes(recorder.recorded_bytes());
                println!();
                println!("RAW EXPORT END");

                let off = ws2812_frame(0, 0, 0);
                channel = channel.transmit(&off).unwrap().wait().unwrap();
                println!("waiting for PIR to go low before rearming...");

                // Re-sync the debounced motion state -- if PIR is still
                // HIGH right now (person still present), this starts with
                // was_high=true, so no new Detected edge fires until PIR
                // actually cycles LOW then HIGH again.
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

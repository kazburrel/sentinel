//! Motion-triggered PSRAM recording, doorbell-style: one motion event is one
//! clip, not a fixed-duration snippet.
//!
//! On PIR motion: LED red, record at least 5s, keep recording for as long as
//! PIR stays HIGH (no fixed maximum), then record a further 5s "tail" once
//! PIR goes LOW -- if motion returns during that tail the same event just
//! continues and the tail restarts. Only stops when PIR has genuinely
//! stopped (past the tail) or PSRAM fills up. All of that is one clip;
//! renewed motion within it never starts a second event.
//!
//! After a clip ends, a cooldown (`REARM_QUIET_SECS`, `firmware::pir::
//! RearmGate`) requires the area to actually be quiet -- not just past the
//! shorter recording tail -- before a new event is allowed to start at
//! all. Without this, a brief PIR gap during one continuous real episode
//! (the AM312's own post-trigger blocking period, or someone briefly
//! pausing) would let the tail alone end the clip, and the very next PIR
//! reading would immediately start a whole new event -- fragmenting one
//! real episode into a burst of separate uploads. Any motion during
//! cooldown just resets the quiet timer; only a real quiet gap re-arms.
//! LED off, upload the event (thumbnail + video, as the `shared` crate's
//! event envelope) to the server over WiFi (bounded retries on transient
//! failures), then export the same clip as a raw binary dump over USB
//! serial regardless of whether the upload succeeded -- USB export is a
//! debugging/recovery fallback, not something the WiFi path's success or
//! failure should ever gate.
//!
//! WiFi connects in the background (a dedicated supervisor task keeps
//! retrying/reconnecting for as long as the board runs), so motion
//! recording and USB export work even through a WiFi outage -- an upload
//! attempted with no connectivity just fails fast and the recording is
//! still preserved via USB.
//!
//! If an event's in-memory upload retries (`upload_event_with_retries`) are
//! exhausted for any transient reason, it's saved to the onboard SD card as
//! a queued file (`firmware::queue`) instead of being lost once its PSRAM
//! buffers get reused by the next event. Queued files are retried whenever
//! an upload next succeeds (proving connectivity's back) and once at boot,
//! deleted only once the server confirms `200`. SD is optional: if the card
//! fails to initialize, the queue is simply disabled for that boot rather
//! than blocking camera/PIR/WiFi.
//!
//! Still not built: pre-roll before motion.

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
use embassy_time::{with_timeout, Delay, Duration, Instant, Timer};
use embedded_io_async::Write as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level};
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::rng::Rng;
use esp_hal::sdmmc::{Config as SdConfig, DelayPhase, SdHostController, SlotConfig};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::{println, Printer};
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::pir::{MotionEdge, MotionSensor, RearmGate};
use firmware::queue::{self, DrainOutcome};
use firmware::recorder::PsramRecorder;
use firmware::ws2812::ws2812_frame;
use sdio::sd::Card;
use sdio::BlockDevice;
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
/// After an event ends, the area must stay quiet (PIR low) for this long
/// before a new event is allowed to start -- separate from, and longer
/// than, `TAIL_DURATION`, which only ends the *current* recording. Without
/// this, a brief PIR gap during one continuous real motion episode (the
/// AM312's own ~2s post-trigger blocking period, or someone briefly
/// pausing) would let the tail alone end the event, and the very next
/// high reading would immediately start a whole new one -- fragmenting
/// one real episode into a burst of separate uploads/`analysis.json`
/// files. See `firmware::pir::RearmGate`.
const REARM_QUIET_SECS: Duration = Duration::from_secs(15);
// No fixed maximum -- an event only ends when PIR has genuinely stopped
// (past the tail) or PSRAM fills up, however long that takes.

static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

const SOCKET_BUF_SIZE: usize = 4096;
static RX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();
static TX_BUF: StaticCell<[u8; SOCKET_BUF_SIZE]> = StaticCell::new();

/// Bounds the TCP connect step of an upload -- a server that's up but not
/// accepting connections (or a network black hole) must not be able to
/// leave `upload_event()` waiting forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds the whole send-then-read-response step. Generous relative to the
/// clip sizes seen so far (low single-digit MB over a local WiFi LAN), but
/// still bounded -- a server that accepts the connection and then stalls
/// (never reads, never responds) must not hang the event loop forever.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(20);

/// How many times to attempt delivering one event before giving up on it
/// (this attempt cycle -- the recording itself is never lost, since USB
/// export always runs regardless). Transient failures (no connection, a
/// mid-transfer I/O error, or the server's own `5xx` meaning "valid
/// request, transient storage problem") are retried; the server rejecting
/// the request outright (`4xx`) is not, since retrying an identical
/// request won't change that outcome.
const MAX_UPLOAD_ATTEMPTS: u32 = 3;
const UPLOAD_RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// SD card clock/timing, matching `sd_regression_test.rs`'s already
/// hardware-verified values for this board's onboard slot.
const CARD_HZ: u32 = 40_000_000;
const INPUT_DELAY_PHASE: DelayPhase = DelayPhase::_0;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Keeps WiFi connected for as long as the board runs: connects, waits for a
/// disconnect, reconnects, forever. Runs independently of `main()`'s motion
/// loop -- boot never blocks on this succeeding (motion recording and USB
/// export work with or without WiFi), and a later drop (AP reboot, board
/// walking out of range, etc.) is picked up and retried here without any
/// special handling needed at the call site: `upload_event` just sees a
/// failed connection attempt if WiFi happens to be down when it runs, the
/// same as it would for any other network failure.
#[embassy_executor::task]
async fn wifi_maintain_task(mut controller: WifiController<'static>) {
    loop {
        if controller.is_connected() {
            match controller.wait_for_disconnect_async().await {
                Ok(info) => println!("WiFi disconnected ({info:?}), reconnecting..."),
                Err(_) => continue, // raced with an already-in-flight disconnect; loop and re-check
            }
        }

        match controller.connect_async().await {
            Ok(_) => println!("WiFi connected"),
            Err(e) => {
                println!("WiFi connect attempt failed: {e:?}, retrying in 5s");
                Timer::after(Duration::from_secs(5)).await;
            }
        }
    }
}

/// What happened trying to deliver one event over WiFi -- deliberately not
/// just "did it work", so the caller can log *why* it didn't (unreachable
/// server vs. a mid-transfer I/O error vs. the server rejecting the request
/// outright) without needing to re-derive that from raw response bytes
/// itself.
#[derive(Debug)]
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
    /// Connect, or the send-then-read-response step, didn't finish within
    /// its bound (`CONNECT_TIMEOUT` / `TRANSFER_TIMEOUT`) -- a stalled
    /// server or a network black hole, not a clean failure.
    TimedOut,
}

impl UploadOutcome {
    /// Whether retrying the *same* event is worth attempting. `4xx`
    /// rejections aren't -- an identical retry will just get the same
    /// answer -- but everything else (no connection, a timeout, a
    /// mid-transfer I/O error, or the server's own `5xx` meaning "valid
    /// request, transient storage problem") might succeed next time.
    fn is_retryable(&self) -> bool {
        match self {
            UploadOutcome::Success => false,
            UploadOutcome::Rejected(code) => *code >= 500,
            UploadOutcome::ConnectFailed | UploadOutcome::IoError | UploadOutcome::TimedOut => true,
        }
    }
}

/// Uploads one event (a real JPEG thumbnail + a PSRAM-recorded video clip)
/// as a single two-part `shared` crate event envelope, over one TCP
/// connection, exactly the request shape `event_upload_video_test.rs`
/// proved on real hardware. A single attempt -- see `upload_event_with_retries`
/// for the retrying wrapper actually called from the event loop.
///
/// `part_timestamp_ms` is both parts' start offset relative to the event
/// (motion detected, `event_start`): the thumbnail and the video's first
/// frame are the exact same captured frame, so they share the same real
/// offset, which is *not* `0` -- capturing that first frame includes the
/// camera's warm-up delay (see `capture_jpeg`'s warmup capture), so by the
/// time it's available, `event_start.elapsed()` is already nonzero. Passing
/// the true measured offset (rather than hardcoding `0`) is what a future
/// audio part would need to line up against this video accurately.
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
    part_timestamp_ms: u32,
    video_duration_ms: u32,
) -> UploadOutcome {
    let envelope_header = encode_envelope_header(2, event_id);
    let thumb_header = encode_part_header(
        PartKind::Thumbnail,
        Encoding::Jpeg,
        part_timestamp_ms,
        0,
        thumbnail.len() as u32,
    );
    let video_header = encode_part_header(
        PartKind::Video,
        Encoding::RecorderFrames,
        part_timestamp_ms,
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
    match with_timeout(CONNECT_TIMEOUT, socket.connect((server_ip, SERVER_PORT))).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            println!("upload: connect FAILED: {e:?}");
            socket.abort();
            return UploadOutcome::ConnectFailed;
        }
        Err(_timeout) => {
            println!("upload: connect TIMED OUT after {}s", CONNECT_TIMEOUT.as_secs());
            socket.abort();
            return UploadOutcome::TimedOut;
        }
    }

    let mut resp_buf = [0u8; 512];
    let transfer_result = with_timeout(TRANSFER_TIMEOUT, async {
        socket.write_all(request_head.as_bytes()).await?;
        socket.write_all(&envelope_header).await?;
        socket.write_all(&thumb_header).await?;
        socket.write_all(thumbnail).await?;
        socket.write_all(&video_header).await?;
        socket.write_all(video).await?;
        socket.read(&mut resp_buf).await
    })
    .await;

    let n = match transfer_result {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            println!("upload: write/read FAILED: {e:?}");
            socket.abort();
            return UploadOutcome::IoError;
        }
        Err(_timeout) => {
            println!("upload: send/response TIMED OUT after {}s", TRANSFER_TIMEOUT.as_secs());
            socket.abort();
            return UploadOutcome::TimedOut;
        }
    };

    let outcome = if n >= 12 && &resp_buf[..12] == b"HTTP/1.1 200" {
        UploadOutcome::Success
    } else {
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
    };
    socket.abort();
    outcome
}

/// Retries `upload_event` a bounded number of times with a fixed backoff,
/// stopping early on success or on a non-retryable outcome (see
/// `UploadOutcome::is_retryable`). Reuses the same `event_id` across
/// attempts -- if an earlier attempt actually reached the server and was
/// stored, but this function never saw a clean response (e.g. a timeout
/// right as the response was being written), the server's own event-ID
/// deduplication recognizes the retry and returns success again without
/// creating a duplicate file.
#[allow(clippy::too_many_arguments)]
#[allow(
    clippy::large_stack_frames,
    reason = "wraps upload_event's own already-justified large frame in a retry loop; \
    same short-lived, one-at-a-time, non-recursive shape"
)]
async fn upload_event_with_retries(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
    tx_buf: &mut [u8],
    server_ip: IpAddress,
    event_id: u64,
    thumbnail: &[u8],
    video: &[u8],
    part_timestamp_ms: u32,
    video_duration_ms: u32,
) -> UploadOutcome {
    for attempt in 1..=MAX_UPLOAD_ATTEMPTS {
        let outcome = upload_event(
            stack,
            &mut *rx_buf,
            &mut *tx_buf,
            server_ip,
            event_id,
            thumbnail,
            video,
            part_timestamp_ms,
            video_duration_ms,
        )
        .await;

        if !outcome.is_retryable() || attempt == MAX_UPLOAD_ATTEMPTS {
            return outcome;
        }
        println!(
            "upload: attempt {attempt}/{MAX_UPLOAD_ATTEMPTS} failed ({outcome:?}), retrying in {}s...",
            UPLOAD_RETRY_BACKOFF.as_secs()
        );
        Timer::after(UPLOAD_RETRY_BACKOFF).await;
    }
    unreachable!("loop always returns by the last iteration")
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

    let wifi_interface = Interface::station();
    let mut wifi_controller = WifiController::new(peripherals.WIFI, Default::default()).unwrap();

    let sta_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(WIFI_SSID)
            .with_password(WIFI_PASSWORD.into()),
    );
    wifi_controller.set_config(&sta_config).unwrap();

    let net_config = embassy_net::Config::dhcpv4(Default::default());
    let resources = STACK_RESOURCES.init(StackResources::new());
    let (stack, runner) = embassy_net::new(wifi_interface, net_config, resources, 0x1234_5678);

    spawner.spawn(net_task(runner).unwrap());
    // Connect/reconnect happens entirely in the background from here on --
    // boot does not wait for it, so motion recording and USB export work
    // even through a WiFi outage (see the module doc comment). Any upload
    // attempted before the first successful connect (or during a later
    // outage) just fails fast in `upload_event`, exactly like any other
    // network failure.
    spawner.spawn(wifi_maintain_task(wifi_controller).unwrap());
    println!("WiFi supervisor started, connecting to \"{WIFI_SSID}\" in the background...");

    let rx_buf = RX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);
    let tx_buf = TX_BUF.init_with(|| [0u8; SOCKET_BUF_SIZE]);
    let server_ip = IpAddress::v4(SERVER_IP[0], SERVER_IP[1], SERVER_IP[2], SERVER_IP[3]);

    // A per-boot random nonce, not just the recording counter, so
    // `event_id` doesn't collide with a previous boot's counter values --
    // needed for the server's retry-deduplication to actually distinguish
    // "the same event retried" from "a new event that happens to reuse an
    // old ID after a reboot". No persistent counter/RTC on this board, so
    // this is what "reboot-safe" means here: astronomically unlikely to
    // repeat, not globally unique forever.
    let boot_nonce = Rng::new().random();
    println!("boot nonce for event IDs: {boot_nonce:#010x}");

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

    // SD card is optional: a failure here disables the offline queue for
    // this boot (events that fail to upload are simply not persisted
    // locally, same as before this milestone), but must not block camera/
    // PIR/WiFi from working -- unlike sd_regression_test.rs, which
    // deliberately halts on SD failure since proving the slot works is its
    // entire purpose, this is production firmware and SD is a fallback, not
    // a dependency.
    let sd_controller = SdHostController::new(peripherals.SDHOST, SdConfig::default()).unwrap();
    let slot_config = SlotConfig::default().with_input_delay_phase(INPUT_DELAY_PHASE);
    let mut sd_card: Option<BlockDevice<Card, _, _, 512>> = match sd_controller.slot::<1>(slot_config) {
        Ok(slot) => {
            let slot = slot
                .with_clk(peripherals.GPIO39)
                .with_cmd(peripherals.GPIO38)
                .with_data0(peripherals.GPIO40)
                .into_async();
            match BlockDevice::new_sd_card(slot, CARD_HZ, Delay).await {
                Ok(card) => {
                    println!("SD card initialized: offline queue enabled");
                    Some(card)
                }
                Err(e) => {
                    println!("SD card init FAILED ({e:?}) -- offline queue disabled this boot");
                    None
                }
            }
        }
        Err(e) => {
            println!("SD slot config FAILED ({e:?}) -- offline queue disabled this boot");
            None
        }
    };

    // Catch up on anything left over from a previous boot/outage before
    // waiting for the first new motion event -- best-effort: WiFi may not
    // even be connected yet this early (it connects in the background), in
    // which case this just fails fast and normal per-event draining (below)
    // picks up the backlog once connectivity is available.
    if let Some(card) = sd_card.as_mut() {
        match queue::drain_queue(
            card,
            stack,
            &mut *rx_buf,
            &mut *tx_buf,
            scratch,
            SERVER_IP,
            SERVER_PORT,
            UPLOAD_TOKEN,
        )
        .await
        {
            Ok(DrainOutcome::Empty) => {}
            Ok(outcome) => println!("queue: boot-time drain attempt: {outcome:?}"),
            Err(e) => println!("queue: boot-time drain attempt failed: {e:?}"),
        }
    }

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
                // When the first frame actually became available, relative
                // to event_start -- not 0, since capturing it includes the
                // camera's warm-up delay. Both the thumbnail and the video
                // (whose first recorded frame is this exact same capture)
                // share this as their real start offset in the envelope.
                let mut part_timestamp_ms: u32 = 0;

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
                                part_timestamp_ms = event_start.elapsed().as_millis() as u32;
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

                if recorder.frame_count() == 0 {
                    // Every capture attempt failed -- there is no real
                    // thumbnail or video content, just a PIR trigger and a
                    // camera that never produced a frame. Uploading empty
                    // parts would get a real 200 back and look like a
                    // legitimate (if boring) event; skip it entirely
                    // instead. USB export below still runs for
                    // completeness, but will just show 0 bytes.
                    println!("event #{recording_count}: 0 frames captured, skipping WiFi upload (nothing to send)");
                } else {
                    let event_id = ((boot_nonce as u64) << 32) | recording_count as u64;
                    let video_duration_ms = event_duration_ms.saturating_sub(part_timestamp_ms);
                    println!("event #{recording_count}: uploading over WiFi (event_id={event_id:#018x})...");
                    let outcome = upload_event_with_retries(
                        stack,
                        &mut *rx_buf,
                        &mut *tx_buf,
                        server_ip,
                        event_id,
                        &thumbnail_buf[..thumbnail_len],
                        recorder.recorded_bytes(),
                        part_timestamp_ms,
                        video_duration_ms,
                    )
                    .await;

                    match &outcome {
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
                        UploadOutcome::TimedOut => {
                            println!("event #{recording_count}: WiFi upload FAILED -- timed out");
                        }
                    }

                    if let Some(card) = sd_card.as_mut() {
                        if matches!(outcome, UploadOutcome::Success) {
                            // Just proved the server's reachable -- also try
                            // to clear any backlog from an earlier outage
                            // instead of waiting for the next event.
                            match queue::drain_queue(
                                card,
                                stack,
                                &mut *rx_buf,
                                &mut *tx_buf,
                                scratch,
                                SERVER_IP,
                                SERVER_PORT,
                                UPLOAD_TOKEN,
                            )
                            .await
                            {
                                Ok(DrainOutcome::Empty) => {}
                                Ok(drain_outcome) => println!("queue: {drain_outcome:?}"),
                                Err(e) => println!("queue: drain attempt failed: {e:?}"),
                            }
                        } else if outcome.is_retryable() {
                            // Attempts genuinely exhausted for now (not a
                            // 4xx rejection, which retrying wouldn't fix
                            // anyway) -- persist it so a later reconnect or
                            // reboot can still deliver it instead of losing
                            // it once these buffers are reused by the next
                            // event.
                            println!("event #{recording_count}: saving to SD queue for later retry...");
                            match queue::save_event(
                                card,
                                event_id,
                                &thumbnail_buf[..thumbnail_len],
                                recorder.recorded_bytes(),
                                part_timestamp_ms,
                                video_duration_ms,
                            )
                            .await
                            {
                                Ok(()) => println!("event #{recording_count}: queued to SD"),
                                Err(e) => println!("event #{recording_count}: failed to queue to SD: {e:?}"),
                            }
                        }
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
                println!(
                    "event #{recording_count} closed -- entering cooldown, waiting for {}s quiet before re-arming",
                    REARM_QUIET_SECS.as_secs()
                );

                // The event's own tail logic (above) only ends once PIR has
                // already been low for TAIL_DURATION, so PIR is guaranteed
                // low right now -- the gate always starts already counting
                // down. Any PIR-high reading during this loop resets it
                // (RearmGate::update), which is what merges a brief pause-
                // then-resume within one real motion episode into a single
                // suppressed re-arm instead of a fresh event; only a real
                // quiet gap opens it.
                let cooldown_start = Instant::now();
                let mut rearm_gate = RearmGate::start(REARM_QUIET_SECS.as_millis(), 0);
                let mut pir_was_high_in_cooldown = false;
                let mut last_cooldown_heartbeat = Instant::now();
                loop {
                    if last_cooldown_heartbeat.elapsed() >= Duration::from_secs(10) {
                        println!("cooldown/re-arm waiting: still quieting down before a new event can start");
                        last_cooldown_heartbeat = Instant::now();
                    }
                    let pir_high = pir.is_high();
                    if pir_high && !pir_was_high_in_cooldown {
                        println!("cooldown/re-arm waiting: motion detected during cooldown, quiet timer reset");
                    }
                    pir_was_high_in_cooldown = pir_high;

                    let now_ms = cooldown_start.elapsed().as_millis();
                    if rearm_gate.update(pir_high, now_ms) {
                        break;
                    }
                    Timer::after(Duration::from_millis(50)).await;
                }
                println!("re-armed after {}s quiet gap", REARM_QUIET_SECS.as_secs());

                // PIR is guaranteed low here (that's what just opened the
                // gate), so this always starts with was_high=false -- the
                // next real motion is a genuinely new episode.
                motion = MotionSensor::new(false);
            }
            Some(MotionEdge::Stopped) => {
                println!("motion stopped");
            }
            None => {}
        }
        Timer::after(Duration::from_millis(50)).await;
    }
}

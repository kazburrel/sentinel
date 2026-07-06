//! Combined hardware regression test: proves camera, PIR, LED, and WiFi
//! (all already independently hardware-verified) still work correctly with
//! the SDMMC/SD card peripheral *also* initialized at the same time, using
//! the same unreleased `esp-hal` fork/revision `firmware/`'s real
//! dependencies were just migrated to (see `PROJECT_STATUS.md`'s "Future
//! local fallback storage" section, Milestone 17). This is step 5 of that
//! migration's own verification plan -- checking that simultaneous
//! ownership of every peripheral (camera/LCD-CAM/DMA, PIR GPIO, WS2812 over
//! RMT, WiFi radio, and now SDMMC) doesn't produce resource conflicts
//! (shared DMA channels, GPIO contention, heap pressure, interrupt
//! priority clashes) that a plain compile can't catch.
//!
//! Boot sequence: init PIR/LED/camera/WiFi (connects, blocking, like
//! `wifi_test.rs`), then init the SD card over SDMMC (like `sdmmc_test.rs`).
//! After that, all four subsystems run concurrently:
//! - PIR motion -> captures one real photo, flashes the LED red then off.
//! - BOOT button (GPIO0) -> runs the same non-destructive FAT
//!   create/read/update/delete cycle as `sdmmc_test.rs`, refusing to touch
//!   an existing file with the same name.
//! - A 10s heartbeat reports WiFi/DHCP status the whole time.
//!
//! Wiring: camera pins match `camera_test.rs`; PIR is GPIO21; LED is
//! GPIO48; SD is CLK=GPIO39/CMD=GPIO38/DATA0=GPIO40 (SDMMC slot 1) -- none
//! of these overlap.

#![no_std]
#![no_main]

use block_device_adapters::BufStream;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::{Runner, StackResources};
use embassy_time::{Delay, Duration, Timer};
use embedded_fatfs::{FileSystem, FsOptions, ReadWriteSeek};
use embedded_io_async::{Read as _, Write as _};
use embedded_partitions::mbr::Scheme;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Pull};
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::sdmmc::{Config as SdConfig, DelayPhase, SdHostController, SlotConfig};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};
use firmware::camera::CameraHandle;
use firmware::ov3660::Framesize;
use firmware::ws2812::ws2812_frame;
use sdio::sd::Card;
use sdio::BlockDevice;
use static_cell::StaticCell;

extern crate alloc;

include!("../wifi_credentials.rs");

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

const CARD_HZ: u32 = 40_000_000;
const INPUT_DELAY_PHASE: DelayPhase = DelayPhase::_0;
const TEST_FILE: &str = "ESPQA.TXT";
const MSG_CREATE: &[u8] = b"esp-hal sdmmc async create\n";
const MSG_UPDATE: &[u8] = b"esp-hal sdmmc async update (longer payload)\n";

const JPEG_BUF_SIZE: usize = 64 * 1024;
static JPEG_BUF: StaticCell<[u8; JPEG_BUF_SIZE]> = StaticCell::new();

static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

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

    let mut pir = Input::new(peripherals.GPIO21, InputConfig::default());
    let mut boot_button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

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

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let tx_config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true)
        .with_carrier_modulation(false)
        .with_memsize(1);
    let mut led = rmt.channel0.configure_tx(&tx_config).unwrap().with_pin(peripherals.GPIO48);
    let off = ws2812_frame(0, 0, 0);
    led = led.transmit(&off).unwrap().wait().unwrap();
    println!("PIR/LED initialized");

    let wifi_interface = Interface::station();
    let mut wifi_controller = WifiController::new(peripherals.WIFI, Default::default()).unwrap();
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
    let (stack, runner) = embassy_net::new(wifi_interface, net_config, resources, 0x1234_5678);
    spawner.spawn(net_task(runner).unwrap());
    println!("waiting for DHCP...");
    stack.wait_config_up().await;
    println!("DHCP done: {:?}", stack.config_v4());

    let sd_controller = SdHostController::new(peripherals.SDHOST, SdConfig::default()).unwrap();
    let slot_config = SlotConfig::default().with_input_delay_phase(INPUT_DELAY_PHASE);
    let slot = sd_controller.slot::<1>(slot_config).unwrap();
    let slot = slot
        .with_clk(peripherals.GPIO39)
        .with_cmd(peripherals.GPIO38)
        .with_data0(peripherals.GPIO40);
    let slot = slot.into_async();
    let mut card: BlockDevice<Card, _, _, 512> = match BlockDevice::new_sd_card(slot, CARD_HZ, Delay).await {
        Ok(card) => card,
        Err(e) => {
            println!("SD card init FAILED: {e:?} -- this is itself a real regression finding, halting");
            loop {
                Timer::after(Duration::from_secs(10)).await;
                println!("still alive: SD card init failed, see error above, halting");
            }
        }
    };
    println!("SD card initialized: {:?}", card.card());
    println!("all subsystems up: camera, PIR/LED, WiFi, SD. Waiting for PIR motion or BOOT button...");

    let mut last_heartbeat = embassy_time::Instant::now();
    loop {
        if last_heartbeat.elapsed() >= Duration::from_secs(10) {
            println!("still alive: wifi_ip={:?}", stack.config_v4());
            last_heartbeat = embassy_time::Instant::now();
        }

        match select(pir.wait_for_high(), boot_button.wait_for_falling_edge()).await {
            Either::First(()) => {
                println!("motion detected -- capturing...");
                let red = ws2812_frame(8, 0, 0);
                led = led.transmit(&red).unwrap().wait().unwrap();
                match camera.capture_jpeg(jpeg_buf).await {
                    Ok(len) => println!("capture succeeded: {len} bytes"),
                    Err(e) => println!("capture FAILED: {e:?}"),
                }
                let off = ws2812_frame(0, 0, 0);
                led = led.transmit(&off).unwrap().wait().unwrap();
                pir.wait_for_low().await;
            }
            Either::Second(()) => {
                let stream = BufStream::<_, 512>::new(&mut card);
                match Scheme::open(stream).await {
                    Ok(Scheme::Mbr(mut mbr)) => {
                        let fat_idx = mbr.iter_used().find(|(_, p)| p.is_fat()).map(|(i, _)| i);
                        match fat_idx {
                            Some(idx) => match mbr.open_partition(idx).await {
                                Ok(part) => fat_crud(part).await,
                                Err(e) => println!("open_partition failed: {e:?}"),
                            },
                            None => println!("no FAT partition found"),
                        }
                    }
                    Ok(Scheme::Superfloppy(io)) => fat_crud(io).await,
                    Ok(Scheme::Unknown(_)) => println!("unrecognised volume layout"),
                    Err(e) => println!("partition scan failed: {e:?}"),
                }
                boot_button.wait_for_high().await;
            }
        }

        Timer::after(Duration::from_millis(50)).await;
    }
}

async fn fat_crud<IO: ReadWriteSeek>(io: IO) {
    match fat_crud_inner(io).await {
        Ok(true) => println!("async FAT CRUD: PASS"),
        Ok(false) => println!("async FAT CRUD: FAIL (content mismatch)"),
        Err(FatCrudError::TestFileAlreadyExists) => println!(
            "async FAT CRUD: REFUSED -- {TEST_FILE} already exists on this card; not overwriting."
        ),
        Err(FatCrudError::Fs(e)) => println!("async FAT CRUD: FAIL: {e:?}"),
        Err(FatCrudError::ReadExact(e)) => println!("async FAT CRUD: FAIL: {e:?}"),
    }
}

enum FatCrudError<E> {
    TestFileAlreadyExists,
    Fs(embedded_fatfs::Error<E>),
    ReadExact(embedded_io_async::ReadExactError<embedded_fatfs::Error<E>>),
}

impl<E> From<embedded_fatfs::Error<E>> for FatCrudError<E> {
    fn from(e: embedded_fatfs::Error<E>) -> Self {
        FatCrudError::Fs(e)
    }
}

impl<E> From<embedded_io_async::ReadExactError<embedded_fatfs::Error<E>>> for FatCrudError<E> {
    fn from(e: embedded_io_async::ReadExactError<embedded_fatfs::Error<E>>) -> Self {
        FatCrudError::ReadExact(e)
    }
}

async fn fat_crud_inner<IO: ReadWriteSeek>(io: IO) -> Result<bool, FatCrudError<IO::Error>> {
    let fs = FileSystem::new(io, FsOptions::new()).await?;

    let already_exists = {
        let root = fs.root_dir();
        root.open_file(TEST_FILE).await.is_ok()
    };
    if already_exists {
        fs.unmount().await?;
        return Err(FatCrudError::TestFileAlreadyExists);
    }

    let ok = {
        let root = fs.root_dir();
        let mut buf = [0u8; 64];

        let mut f = root.create_file(TEST_FILE).await?;
        f.truncate().await?;
        f.write_all(MSG_CREATE).await?;
        f.flush().await?;
        drop(f);

        let mut f = root.open_file(TEST_FILE).await?;
        f.read_exact(&mut buf[..MSG_CREATE.len()]).await?;
        let created_ok = &buf[..MSG_CREATE.len()] == MSG_CREATE;
        drop(f);

        let mut f = root.open_file(TEST_FILE).await?;
        f.truncate().await?;
        f.write_all(MSG_UPDATE).await?;
        f.flush().await?;
        drop(f);

        let mut f = root.open_file(TEST_FILE).await?;
        f.read_exact(&mut buf[..MSG_UPDATE.len()]).await?;
        let updated_ok = &buf[..MSG_UPDATE.len()] == MSG_UPDATE;
        drop(f);

        root.remove(TEST_FILE).await?;
        created_ok && updated_ok
    };
    fs.unmount().await?;
    Ok(ok)
}

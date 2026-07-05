//! Standalone smoke test for this board's onboard microSD slot, using an
//! unreleased upstream `esp-hal` SDMMC/SDIO host driver (esp-rs/esp-hal#5760,
//! branch `bugadani/esp-hal:sdmmc`, pinned to an exact revision -- see this
//! crate's `Cargo.toml` and `PROJECT_STATUS.md`'s "Future local fallback
//! storage" section for why). No released `esp-hal` has any SDMMC support,
//! and this board's slot exposes only CMD/CLK/DATA0 (no CS), which also
//! rules out the SPI-mode fallback most Rust SD card crates target.
//!
//! Adapted from the upstream `qa-test/src/bin/sdmmc_sd_async.rs` example in
//! that same branch, trimmed to this board's one chip (ESP32-S3, 1-bit mode)
//! instead of the original's multi-chip `cfg_select!`. Deliberately kept as
//! its own fully separate crate (own `Cargo.toml`/`Cargo.lock`, not part of
//! `firmware/`'s dependency graph) so this experimental, unreleased
//! dependency pin can't destabilize the camera/WiFi firmware even if it
//! turns out broken.
//!
//! Wiring (Freenove ESP32-S3-WROOM CAM onboard slot, confirmed pin map,
//! 1-bit mode, SDMMC slot 1):
//! - CLK   => GPIO39
//! - CMD   => GPIO38
//! - DATA0 => GPIO40
//!
//! Non-destructive by design: does nothing until the BOOT button (GPIO0) is
//! pressed, then creates/reads/updates/reads/deletes one test-owned file
//! (`ESPQA.TXT`) on whatever FAT volume it finds (an MBR partition or a
//! superfloppy layout), leaving the card exactly as it found it either way.
//! No other file on the card is touched.

#![no_std]
#![no_main]

use block_device_adapters::BufStream;
use embassy_executor::Spawner;
use embassy_time::{Delay, Duration, Timer};
use embedded_fatfs::{FileSystem, FsOptions, ReadWriteSeek};
use embedded_io_async::{Read, Write};
use embedded_partitions::mbr::Scheme;
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::sdmmc::{Config, DelayPhase, SdHostController, SlotConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use sdio::sd::Card;
use sdio::BlockDevice;

esp_bootloader_esp_idf::esp_app_desc!();

/// Target card clock. 40 MHz exercises the SD high-speed (SDR25) path.
const CARD_HZ: u32 = 40_000_000;

/// Input sampling phase for the high-speed path. If 40 MHz init fails with a
/// timeout, try `_1`, `_2`, then `_3` and reflash.
const INPUT_DELAY_PHASE: DelayPhase = DelayPhase::_0;

/// Test-owned file (8.3 name); assumed not to exist on the card already.
const TEST_FILE: &str = "ESPQA.TXT";
const MSG_CREATE: &[u8] = b"esp-hal sdmmc async create\n";
const MSG_UPDATE: &[u8] = b"esp-hal sdmmc async update (longer payload)\n";

#[esp_hal::main]
async fn main(_spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    esp_println::logger::init_logger(log::LevelFilter::Info);
    println!("boot: peripherals initialized");

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let controller = SdHostController::new(peripherals.SDHOST, Config::default()).unwrap();
    let slot_config = SlotConfig::default().with_input_delay_phase(INPUT_DELAY_PHASE);
    let slot = controller.slot::<1>(slot_config).unwrap();
    let slot = slot
        .with_clk(peripherals.GPIO39)
        .with_cmd(peripherals.GPIO38)
        .with_data0(peripherals.GPIO40);
    let slot = slot.into_async();

    let mut card: BlockDevice<Card, _, _, 512> = match BlockDevice::new_sd_card(slot, CARD_HZ, Delay).await {
        Ok(card) => card,
        Err(e) => {
            println!("card init FAILED: {e:?}");
            loop {
                Timer::after(Duration::from_secs(10)).await;
                println!("still alive: card init failed, see error above, halting");
            }
        }
    };
    println!("card initialized: {:?}", card.card());

    let mut button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));

    loop {
        println!("waiting for BOOT button (GPIO0) press to run the SD card test...");
        // Report repeatedly, not once -- a one-shot print can be missed by
        // the host's serial reader (this project's own established lesson;
        // see PROJECT_STATUS.md).
        loop {
            match embassy_futures::select::select(
                button.wait_for_falling_edge(),
                Timer::after(Duration::from_secs(10)),
            )
            .await
            {
                embassy_futures::select::Either::First(()) => break,
                embassy_futures::select::Either::Second(()) => {
                    println!("still alive: waiting for BOOT button press");
                }
            }
        }

        let stream = BufStream::<_, 512>::new(&mut card);
        match Scheme::open(stream).await {
            Ok(Scheme::Mbr(mut mbr)) => {
                let fat_idx = mbr.iter_used().find(|(_, p)| p.is_fat()).map(|(i, _)| i);
                match fat_idx {
                    Some(idx) => match mbr.open_partition(idx).await {
                        Ok(slice) => fat_crud(slice).await,
                        Err(e) => println!("open_partition failed: {e:?}"),
                    },
                    None => println!("no FAT partition found"),
                }
            }
            Ok(Scheme::Superfloppy(io)) => fat_crud(io).await,
            Ok(Scheme::Unknown(_)) => println!("unrecognised volume layout"),
            Err(e) => println!("partition scan failed: {e:?}"),
        }

        println!("test done -- release the BOOT button, then press again to re-run.");
        button.wait_for_high().await;
    }
}

/// Runs the CRUD cycle and reports the outcome.
async fn fat_crud<IO: ReadWriteSeek>(io: IO) {
    match fat_crud_inner(io).await {
        Ok(true) => println!("async FAT CRUD: PASS"),
        Ok(false) => println!("async FAT CRUD: FAIL (content mismatch)"),
        Err(FatCrudError::TestFileAlreadyExists) => println!(
            "async FAT CRUD: REFUSED -- {TEST_FILE} already exists on this card; \
             not overwriting a file this test doesn't own. Remove or rename it, \
             or change TEST_FILE, then re-run."
        ),
        Err(FatCrudError::Fs(e)) => println!("async FAT CRUD: FAIL: {e:?}"),
        Err(FatCrudError::ReadExact(e)) => println!("async FAT CRUD: FAIL: {e:?}"),
    }
}

enum FatCrudError<E> {
    /// [`TEST_FILE`] already exists -- refusing to touch a file this test
    /// doesn't own, rather than silently overwriting real data that might
    /// already be on the card.
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

/// Creates, reads, updates and deletes [`TEST_FILE`]; returns whether the
/// round-tripped contents matched. Refuses to run at all if [`TEST_FILE`]
/// is already present, rather than assuming it's safe to overwrite.
async fn fat_crud_inner<IO: ReadWriteSeek>(io: IO) -> Result<bool, FatCrudError<IO::Error>> {
    let fs = FileSystem::new(io, FsOptions::new()).await?;

    // Refuse to proceed if a file with this name already exists -- this
    // test only ever operates on a file it created itself. Scoped so
    // `root` is dropped before the early-return path unmounts `fs` below.
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

        // Create + write.
        let mut f = root.create_file(TEST_FILE).await?;
        f.truncate().await?;
        f.write_all(MSG_CREATE).await?;
        f.flush().await?;
        drop(f);

        // Read back the created content.
        let mut f = root.open_file(TEST_FILE).await?;
        f.read_exact(&mut buf[..MSG_CREATE.len()]).await?;
        let created_ok = &buf[..MSG_CREATE.len()] == MSG_CREATE;
        drop(f);

        // Update: truncate and rewrite with a different payload.
        let mut f = root.open_file(TEST_FILE).await?;
        f.truncate().await?;
        f.write_all(MSG_UPDATE).await?;
        f.flush().await?;
        drop(f);

        // Read back the updated content.
        let mut f = root.open_file(TEST_FILE).await?;
        f.read_exact(&mut buf[..MSG_UPDATE.len()]).await?;
        let updated_ok = &buf[..MSG_UPDATE.len()] == MSG_UPDATE;
        drop(f);

        // Delete, leaving the card as we found it.
        root.remove(TEST_FILE).await?;
        created_ok && updated_ok
    };
    fs.unmount().await?;
    Ok(ok)
}

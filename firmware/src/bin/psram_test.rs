//! Standalone PSRAM smoke test -- confirms the Freenove N8R8 board's 8MB
//! octal PSRAM initializes and is actually readable/writable, before it's
//! trusted to hold camera frame data. No camera code here at all.
//!
//! Verifies:
//! - `Psram::new()` reports a size close to 8MB (auto-detected).
//! - A repeating byte pattern written across the whole region reads back
//!   correctly, more than once (to catch anything that only works once).

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::psram::{Psram, PsramConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {info}");
    loop {}
}

fn check_pattern(psram_ptr: *mut u8, size: usize, seed: u8) -> Result<(), usize> {
    let slice = unsafe { core::slice::from_raw_parts_mut(psram_ptr, size) };

    for (i, b) in slice.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(seed);
    }

    for (i, &b) in slice.iter().enumerate() {
        let expected = (i as u8).wrapping_add(seed);
        if b != expected {
            return Err(i);
        }
    }

    Ok(())
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

    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (ptr, size) = psram.raw_parts();

    println!("PSRAM initialized: {size} bytes ({:.2} MB) at {ptr:p}", size as f32 / (1024.0 * 1024.0));

    let mut pass_count: u32 = 0;
    let mut fail_count: u32 = 0;

    for round in 0..3u8 {
        match check_pattern(ptr, size, round) {
            Ok(()) => {
                pass_count += 1;
                println!("pattern check round {round}: PASS ({size} bytes verified)");
            }
            Err(offset) => {
                fail_count += 1;
                println!("pattern check round {round}: FAIL at byte offset {offset}");
            }
        }
    }

    // Report repeatedly forever, not once -- a one-shot print can be missed
    // by the host's serial reader and looks identical to a hang.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "still alive: psram_size={size} pass={pass_count} fail={fail_count}"
        );
    }
}

use esp_hal::gpio::Level;
use esp_hal::rmt::PulseCode;

/// Freenove's measured GPIO48 WS2812 timings at an 80 MHz RMT clock, GRB, MSB first.
pub fn ws2812_frame(r: u8, g: u8, b: u8) -> [PulseCode; 25] {
    let zero = PulseCode::new(Level::High, 32, Level::Low, 64);
    let one = PulseCode::new(Level::High, 64, Level::Low, 32);

    let mut frame = [PulseCode::end_marker(); 25];
    let mut index = 0;
    for byte in [g, r, b] {
        for bit in (0..8).rev() {
            frame[index] = if byte & (1 << bit) != 0 { one } else { zero };
            index += 1;
        }
    }
    frame
}

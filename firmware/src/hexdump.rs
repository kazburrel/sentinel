//! Serial hex-dump of a byte buffer, framed with `JPEG BEGIN`/`JPEG END`
//! markers so a host-side script can extract and decode the image. This is
//! a transport/debug concern, not a camera concern -- kept separate from
//! `camera.rs` so the capture module stays free of print/formatting details.

use esp_println::println;

#[inline]
fn hex_nibble(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'a' + (v - 10) }
}

/// Prints `data` as lowercase hex, 64 bytes per line, bracketed by
/// `JPEG BEGIN {len}` / `JPEG END` marker lines.
pub fn print_hex_dump(data: &[u8]) {
    println!("JPEG BEGIN {}", data.len());
    let mut line = [0u8; 128];
    for chunk in data.chunks(64) {
        for (i, &b) in chunk.iter().enumerate() {
            line[i * 2] = hex_nibble(b >> 4);
            line[i * 2 + 1] = hex_nibble(b & 0x0F);
        }
        let s = core::str::from_utf8(&line[..chunk.len() * 2]).unwrap_or("");
        println!("{s}");
    }
    println!("JPEG END");
}

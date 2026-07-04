//! PSRAM-backed frame recording arena: length-prefixed JPEG frames plus
//! timestamps, appended sequentially into a raw external-memory byte region
//! (e.g. from `esp_hal::psram::Psram::raw_parts()`). Kept separate from
//! `camera.rs` (camera protocol) and `hexdump.rs` (serial transport) --
//! this is a third, distinct concern: buffering captured frames in memory
//! for later export, decoupled from both capture and transmission.
//!
//! Record format, back-to-back for each frame: `[frame_len: u32 LE]
//! [timestamp_ms: u32 LE][frame_len bytes of JPEG data]`.
//!
//! Deliberately holds no atomics and nothing but plain bytes -- PSRAM on
//! ESP32-S3 doesn't support atomic instructions correctly, so this type is
//! not `Sync`/shared, just a plain owned buffer used from one task.

#[derive(Debug)]
pub enum RecorderError {
    OutOfSpace,
}

pub struct PsramRecorder {
    buf: &'static mut [u8],
    cursor: usize,
    frame_count: u32,
}

impl PsramRecorder {
    /// # Safety
    /// `ptr` must point to `len` bytes of valid memory, exclusively owned
    /// by this recorder for its entire lifetime (e.g. the region returned
    /// by `Psram::raw_parts()`, used by nothing else).
    pub unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        let buf = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        Self {
            buf,
            cursor: 0,
            frame_count: 0,
        }
    }

    /// Discards all recorded frames, ready to record a new clip from
    /// scratch (does not zero the underlying memory, just resets the
    /// cursor -- old bytes past the new recording's end are simply never
    /// read again).
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.frame_count = 0;
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    pub fn bytes_used(&self) -> usize {
        self.cursor
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Appends one frame's record (8-byte header + JPEG bytes). Fails
    /// without writing anything if there isn't enough room left for the
    /// whole record.
    pub fn record_frame(&mut self, jpeg: &[u8], timestamp_ms: u32) -> Result<(), RecorderError> {
        let record_len = 8 + jpeg.len();
        if self.cursor + record_len > self.buf.len() {
            return Err(RecorderError::OutOfSpace);
        }

        let frame_len = jpeg.len() as u32;
        self.buf[self.cursor..self.cursor + 4].copy_from_slice(&frame_len.to_le_bytes());
        self.buf[self.cursor + 4..self.cursor + 8].copy_from_slice(&timestamp_ms.to_le_bytes());
        self.buf[self.cursor + 8..self.cursor + 8 + jpeg.len()].copy_from_slice(jpeg);

        self.cursor += record_len;
        self.frame_count += 1;
        Ok(())
    }

    /// The raw recorded bytes so far, ready to be exported byte-for-byte.
    pub fn recorded_bytes(&self) -> &[u8] {
        &self.buf[..self.cursor]
    }
}

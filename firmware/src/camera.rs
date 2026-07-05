//! Reusable OV3660 camera capture: LCD-CAM/DMA peripheral setup + SCCB sensor
//! init happen once (`CameraHandle::new`), then `capture_jpeg` can be called
//! repeatedly (e.g. once per PIR motion event) reusing the already-configured
//! hardware, instead of redoing the ~200-register SCCB init every time.
//!
//! Factored out of the `camera_test` proof-of-concept now that it's verified
//! working on real hardware (see `PROJECT_STATUS.md`, Milestone 4).

use embassy_time::{Duration, Timer};
use esp_hal::{
    i2c::master::{Config as I2cConfig, I2c},
    lcd_cam::{
        cam::{self, Camera},
        LcdCam,
    },
    peripherals::{DMA_CH0, I2C0, LCD_CAM},
    time::Rate,
    Blocking,
};

use crate::ov3660::{Framesize, Ov3660};

#[derive(Debug)]
pub enum CameraError {
    I2c(esp_hal::i2c::master::Error),
    I2cConfig(esp_hal::i2c::master::ConfigError),
    DmaSetup,
    FrameTooBigForBuffer,
    DmaFinishedBeforeEof,
    /// Captured bytes were missing a JPEG SOI (`FF D8`) or EOI (`FF D9`)
    /// marker -- a corrupted/incomplete frame, not a real photo.
    InvalidJpeg,
}

/// Owns the already-configured camera hardware (LCD-CAM/DMA peripheral +
/// OV3660 sensor) so `capture_jpeg` can be called repeatedly without
/// redoing setup each time.
pub struct CameraHandle<'d> {
    camera: Option<Camera<'d>>,
    _sensor: Ov3660<'d>,
}

impl<'d> CameraHandle<'d> {
    /// Configures the LCD-CAM/DMA peripheral and the OV3660 sensor over
    /// SCCB. Does the full sensor register init (soft reset, ~200 default
    /// registers, JPEG format, `framesize`'s crop/scale/compression-enable
    /// registers) exactly once -- `capture_jpeg` reuses this configured state.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        lcd_cam_periph: LCD_CAM<'d>,
        dma_ch0: DMA_CH0<'d>,
        i2c0: I2C0<'d>,
        mclk: impl esp_hal::gpio::interconnect::PeripheralOutput<'d>,
        pclk: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        vsync: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        href: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d0: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d1: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d2: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d3: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d4: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d5: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d6: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        d7: impl esp_hal::gpio::interconnect::PeripheralInput<'d>,
        sda: impl esp_hal::gpio::interconnect::PeripheralInput<'d>
            + esp_hal::gpio::interconnect::PeripheralOutput<'d>,
        scl: impl esp_hal::gpio::interconnect::PeripheralInput<'d>
            + esp_hal::gpio::interconnect::PeripheralOutput<'d>,
        framesize: Framesize,
    ) -> Result<Self, CameraError> {
        let cam_config = cam::Config::default().with_frequency(Rate::from_mhz(20));
        let lcd_cam = LcdCam::new(lcd_cam_periph);
        let camera = Camera::new(lcd_cam.cam, dma_ch0, cam_config)
            .map_err(|_| CameraError::DmaSetup)?
            .with_master_clock(mclk)
            .with_pixel_clock(pclk)
            .with_vsync(vsync)
            .with_h_enable(href)
            .with_data0(d0)
            .with_data1(d1)
            .with_data2(d2)
            .with_data3(d3)
            .with_data4(d4)
            .with_data5(d5)
            .with_data6(d6)
            .with_data7(d7);

        // SCCB/I2C setup happens BEFORE the first camera.receive() --
        // starting the small DMA descriptor chain too early lets it exhaust
        // itself while the ~1s+ of I2C setup is still running, before
        // anything ever reads from it.
        Timer::after(Duration::from_millis(500)).await;

        let i2c = I2c::new(i2c0, I2cConfig::default())
            .map_err(CameraError::I2cConfig)?
            .with_sda(sda)
            .with_scl(scl);

        let mut sensor = Ov3660::new(i2c);

        loop {
            match sensor.check_id() {
                Ok(()) => break,
                Err(_) => Timer::after(Duration::from_millis(500)).await,
            }
        }

        sensor.init_jpeg(10, framesize).await.map_err(CameraError::I2c)?;

        Ok(Self {
            camera: Some(camera),
            _sensor: sensor,
        })
    }

    /// Captures exactly one VGA JPEG frame into `jpeg_buf`, returning the
    /// length of the actual JPEG data (from SOI `FF D8` through EOI `FF D9`,
    /// excluding any trailing DMA padding). Can be called repeatedly --
    /// each call starts a fresh DMA receive on the already-configured sensor
    /// and hands the camera hardware back to `self` when done.
    ///
    /// Skips 2 settling frames before keeping the real one, since the
    /// sensor's auto-exposure/gain need a moment to adjust after sitting
    /// idle -- correct for a cold/standalone shot (e.g. a PIR-triggered
    /// photo, possibly minutes after the last one), but wasteful for a
    /// continuous back-to-back burst; use `capture_jpeg_continuous` for that.
    ///
    /// `jpeg_buf` must be large enough for one frame -- a real VGA JPEG at
    /// quality 10 has been measured at ~12-18KB on this hardware; a 64KB
    /// buffer leaves comfortable margin.
    pub async fn capture_jpeg(&mut self, jpeg_buf: &mut [u8]) -> Result<usize, CameraError> {
        self.capture_jpeg_with_warmup(jpeg_buf, 2).await
    }

    /// Same as `capture_jpeg`, but without the settling-frame skip -- for
    /// calling back-to-back in a tight loop (e.g. building a short video),
    /// where the sensor's auto-exposure/gain are already settled from the
    /// previous call and re-skipping every time would throw away 2 out of
    /// every 3 frames captured for no reason.
    pub async fn capture_jpeg_continuous(
        &mut self,
        jpeg_buf: &mut [u8],
    ) -> Result<usize, CameraError> {
        self.capture_jpeg_with_warmup(jpeg_buf, 0).await
    }

    async fn capture_jpeg_with_warmup(
        &mut self,
        jpeg_buf: &mut [u8],
        warmup_frames: u32,
    ) -> Result<usize, CameraError> {
        let camera = self.camera.take().expect("camera handle already in use");
        let dma_rx_buf = esp_hal::dma_rx_stream_buffer!(20 * 1000, 1000);

        let mut transfer = match camera.receive(dma_rx_buf) {
            Ok(t) => t,
            Err((_, camera, _)) => {
                self.camera = Some(camera);
                return Err(CameraError::DmaSetup);
            }
        };

        // Skip `warmup_frames` partial/settling frames before treating the
        // next one as real.
        for _ in 0..warmup_frames {
            loop {
                let (data, ends_with_eof) = transfer.peek_until_eof();
                if data.is_empty() {
                    if transfer.is_done() {
                        let (camera, _) = transfer.stop();
                        self.camera = Some(camera);
                        return Err(CameraError::DmaFinishedBeforeEof);
                    }
                } else {
                    let bytes_peeked = data.len();
                    transfer.consume(bytes_peeked);
                    if ends_with_eof {
                        break;
                    }
                }
            }
        }

        // No per-byte work during the live DMA loop (confirmed too slow to
        // keep up with the DMA rate) -- only copy into the buffer.
        let mut frame_len: usize = 0;
        let result = loop {
            let (data, ends_with_eof) = transfer.peek_until_eof();
            if data.is_empty() {
                if transfer.is_done() {
                    break Err(CameraError::DmaFinishedBeforeEof);
                }
            } else {
                let bytes_peeked = data.len();
                if frame_len + bytes_peeked > jpeg_buf.len() {
                    break Err(CameraError::FrameTooBigForBuffer);
                }
                jpeg_buf[frame_len..frame_len + bytes_peeked].copy_from_slice(data);
                frame_len += bytes_peeked;
                transfer.consume(bytes_peeked);
                if ends_with_eof {
                    break Ok(());
                }
            }
        };

        let (camera, _) = transfer.stop();
        self.camera = Some(camera);

        result?;
        trim_to_jpeg(jpeg_buf, frame_len)
    }
}

/// Finds the real JPEG bounds (SOI `FF D8` .. EOI `FF D9`) within the
/// captured bytes, discarding any trailing DMA padding/garbage the sensor
/// appends after the real frame. Shifts the found JPEG data to the start of
/// `buf` (so callers can always just use `&jpeg_buf[..returned_len]`) and
/// returns its length. A missing SOI or EOI is a corrupted/incomplete frame,
/// not a real photo -- returns `Err` rather than silently trimming to
/// whatever bytes happen to be there.
fn trim_to_jpeg(buf: &mut [u8], captured_len: usize) -> Result<usize, CameraError> {
    let data = &buf[..captured_len];

    let soi = data
        .windows(2)
        .position(|w| w == [0xFF, 0xD8])
        .ok_or(CameraError::InvalidJpeg)?;

    let eoi = data[soi..]
        .windows(2)
        .position(|w| w == [0xFF, 0xD9])
        .map(|pos| soi + pos + 2)
        .ok_or(CameraError::InvalidJpeg)?;

    let len = eoi - soi;
    buf.copy_within(soi..eoi, 0);
    Ok(len)
}

#[allow(dead_code)]
fn _assert_i2c_blocking(_: I2c<'_, Blocking>) {}

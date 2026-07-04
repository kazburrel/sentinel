//! Reusable OV3660 camera capture: LCD-CAM/DMA peripheral setup + SCCB sensor
//! init + one-shot JPEG frame capture, factored out of the `camera_test`
//! proof-of-concept now that it's verified working on real hardware (see
//! `PROJECT_STATUS.md`, Milestone 4).

use embassy_time::{Duration, Timer};
use esp_hal::{
    dma::DmaRxStreamBuf,
    gpio::interconnect::{PeripheralInput, PeripheralOutput},
    i2c::master::{Config as I2cConfig, I2c},
    lcd_cam::{
        cam::{self, Camera},
        LcdCam,
    },
    peripherals::{DMA_CH0, I2C0, LCD_CAM},
    time::Rate,
};

use crate::ov3660::Ov3660;

#[derive(Debug)]
pub enum CameraError {
    I2c(esp_hal::i2c::master::Error),
    I2cConfig(esp_hal::i2c::master::ConfigError),
    CheckId(crate::ov3660::CheckIdError),
    DmaSetup,
    FrameTooBigForBuffer,
    DmaFinishedBeforeEof,
}

/// Sets up the LCD-CAM/DMA camera peripheral and the OV3660 sensor over
/// SCCB, then captures exactly one VGA JPEG frame into `jpeg_buf`, returning
/// the length of the actual JPEG data (from SOI `FF D8` through EOI `FF D9`,
/// excluding any trailing DMA padding).
///
/// `jpeg_buf` must be large enough for one frame -- a real VGA JPEG at
/// quality 10 has been measured at ~12-13KB on this hardware; a 64KB buffer
/// leaves comfortable margin.
#[allow(clippy::too_many_arguments)]
pub async fn capture_jpeg<'d>(
    lcd_cam_periph: LCD_CAM<'d>,
    dma_ch0: DMA_CH0<'d>,
    i2c0: I2C0<'d>,
    mclk: impl PeripheralOutput<'d>,
    pclk: impl PeripheralInput<'d>,
    vsync: impl PeripheralInput<'d>,
    href: impl PeripheralInput<'d>,
    d0: impl PeripheralInput<'d>,
    d1: impl PeripheralInput<'d>,
    d2: impl PeripheralInput<'d>,
    d3: impl PeripheralInput<'d>,
    d4: impl PeripheralInput<'d>,
    d5: impl PeripheralInput<'d>,
    d6: impl PeripheralInput<'d>,
    d7: impl PeripheralInput<'d>,
    sda: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    scl: impl PeripheralInput<'d> + PeripheralOutput<'d>,
    jpeg_buf: &mut [u8],
) -> Result<usize, CameraError> {
    let dma_rx_buf: DmaRxStreamBuf = esp_hal::dma_rx_stream_buffer!(20 * 1000, 1000);

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

    // SCCB/I2C setup happens BEFORE camera.receive() -- starting the small
    // DMA descriptor chain too early lets it exhaust itself while the ~1s+
    // of I2C setup is still running, before anything ever reads from it.
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

    sensor.init_jpeg(10).await.map_err(CameraError::I2c)?;

    let mut transfer = camera
        .receive(dma_rx_buf)
        .map_err(|_| CameraError::DmaSetup)?;

    // Skip 2 partial/settling frames before treating the next one as real.
    for _ in 0..2 {
        loop {
            let (data, ends_with_eof) = transfer.peek_until_eof();
            if data.is_empty() {
                if transfer.is_done() {
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

    // No per-byte work during the live DMA loop (confirmed too slow to keep
    // up with the DMA rate) -- only copy into the buffer, everything else
    // (dumping, parsing) happens after transfer.stop().
    let mut frame_len: usize = 0;
    loop {
        let (data, ends_with_eof) = transfer.peek_until_eof();
        if data.is_empty() {
            if transfer.is_done() {
                let _ = transfer.stop();
                return Err(CameraError::DmaFinishedBeforeEof);
            }
        } else {
            let bytes_peeked = data.len();
            if frame_len + bytes_peeked > jpeg_buf.len() {
                let _ = transfer.stop();
                return Err(CameraError::FrameTooBigForBuffer);
            }
            jpeg_buf[frame_len..frame_len + bytes_peeked].copy_from_slice(data);
            frame_len += bytes_peeked;
            transfer.consume(bytes_peeked);
            if ends_with_eof {
                break;
            }
        }
    }

    let _ = transfer.stop();

    Ok(trim_to_jpeg(jpeg_buf, frame_len))
}

/// Finds the real JPEG bounds (SOI `FF D8` .. EOI `FF D9`) within the
/// captured bytes, discarding any trailing DMA padding/garbage the sensor
/// appends after the real frame. Shifts the found JPEG data to the start of
/// `buf` (so callers can always just use `&jpeg_buf[..returned_len]`) and
/// returns its length.
fn trim_to_jpeg(buf: &mut [u8], captured_len: usize) -> usize {
    let data = &buf[..captured_len];

    let soi = data
        .windows(2)
        .position(|w| w == [0xFF, 0xD8])
        .unwrap_or(0);

    let eoi = data[soi..]
        .windows(2)
        .position(|w| w == [0xFF, 0xD9])
        .map(|pos| soi + pos + 2)
        .unwrap_or(captured_len);

    let len = eoi - soi;
    buf.copy_within(soi..eoi, 0);
    len
}

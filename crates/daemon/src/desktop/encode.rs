//! Frames into the stream: RGBA capture to I420, the AV1 encoder, and
//! PNG for the agent's screenshots.
//!
//! The encoder is rav1e, embedded — the licence-clean AV1 the stream is
//! built around. One frame in produces at most one packet out under
//! `low_latency`, which is what lets a dropped chunk mean a dropped
//! frame rather than a stalled pipeline.

use std::time::Duration;

use rav1e::data::{FrameType, Rational};
use rav1e::prelude::{
    ChromaSamplePosition, ChromaSampling, Config, Context, EncoderConfig, EncoderStatus,
    FrameParameters, FrameTypeOverride, PixelRange, Tune,
};

use crate::config::ComputerConfig;
use crate::desktop::x11::Image;

/// The AV1 encoder over one display's geometry.
///
/// The sequence header is captured at construction and prepended to
/// every keyframe, so each `keyframe` chunk is a self-sufficient
/// temporal unit a decoder can start from cold — the room replays from
/// the newest one when a watcher joins.
#[derive(Debug)]
pub struct Encoder {
    /// The rav1e context.
    context: Context<u8>,
    /// `container_sequence_header`, cached at construction.
    sequence_header: Vec<u8>,
    /// The cadence, for the supervisor's frame clock.
    fps: u32,
}

/// One encoded temporal unit.
#[derive(Debug)]
pub struct Packet {
    /// Whether a decoder can start from this packet.
    pub keyframe: bool,
    /// The encoded bytes — sequence header included on keyframes.
    pub bytes: Vec<u8>,
}

/// Everything the encode path can fail with.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The encoder rejected the configuration.
    #[error("the encoder refused the configuration: {0}")]
    Config(#[from] rav1e::config::InvalidConfig),
    /// A frame or the drain failed inside rav1e.
    #[error("the encoder failed: {0:?}")]
    Encode(EncoderStatus),
}

impl Encoder {
    /// Builds the encoder for one display.
    ///
    /// Low-latency, speed-preset fastest: the display produces a few
    /// frames a second against a shared VM's cores, and every millisecond
    /// of lookahead is latency the user feels.
    pub fn new(config: &ComputerConfig) -> Result<Self, EncodeError> {
        let mut encoder = EncoderConfig::with_speed_preset(10);
        encoder.width = config.width as usize;
        encoder.height = config.height as usize;
        encoder.time_base = Rational {
            num: 1,
            den: u64::from(config.fps),
        };
        encoder.bit_depth = 8;
        encoder.chroma_sampling = ChromaSampling::Cs420;
        encoder.chroma_sample_position = ChromaSamplePosition::Unknown;
        encoder.pixel_range = PixelRange::Full;
        encoder.low_latency = true;
        // Constant-quality rather than a bitrate target: a text screen is
        // nearly free between keystrokes, and the expensive frames are
        // exactly the ones worth paying for.
        encoder.bitrate = 0;
        encoder.quantizer = 80;
        encoder.tune = Tune::Psychovisual;
        // A GOP long enough to save real egress, short enough that a
        // resync never costs more than a couple of seconds of stream.
        encoder.max_key_frame_interval = u64::from(config.fps) * 8;
        encoder.min_key_frame_interval = u64::from(config.fps);

        let context = Config::new().with_encoder_config(encoder).new_context()?;
        let sequence_header = context.container_sequence_header();
        Ok(Self {
            context,
            sequence_header,
            fps: config.fps,
        })
    }

    /// The configured cadence.
    pub const fn fps(&self) -> u32 {
        self.fps
    }

    /// Encodes one captured frame.
    ///
    /// `force_keyframe` overrides the GOP schedule — a dropped chunk or a
    /// watcher joining mid-stream asks for it. The sequence header rides
    /// in front of every keyframe so a chunk is all a decoder needs.
    ///
    /// Under `low_latency` a frame produces at most one packet, so the
    /// drain is a single receive rather than a queue.
    pub fn encode(
        &mut self,
        image: &Image,
        force_keyframe: bool,
    ) -> Result<Option<Packet>, EncodeError> {
        let mut frame = self.context.new_frame();
        fill_i420(&mut frame, image);
        if force_keyframe {
            self.context
                .send_frame((
                    frame,
                    FrameParameters {
                        frame_type_override: FrameTypeOverride::Key,
                        ..Default::default()
                    },
                ))
                .map_err(EncodeError::Encode)?;
        } else {
            self.context
                .send_frame(frame)
                .map_err(EncodeError::Encode)?;
        }

        match self.context.receive_packet() {
            Ok(packet) => {
                let keyframe = packet.frame_type == FrameType::KEY;
                let mut bytes = Vec::with_capacity(self.sequence_header.len() + packet.data.len());
                if keyframe {
                    bytes.extend_from_slice(&self.sequence_header);
                }
                bytes.extend_from_slice(&packet.data);
                Ok(Some(Packet { keyframe, bytes }))
            }
            // The encoder still holds the frame — under low-latency it is
            // emitted with the next one, so a quiet screen's final packet
            // simply lands a frame late.
            Err(
                EncoderStatus::EnoughData
                | EncoderStatus::NeedMoreData
                | EncoderStatus::LimitReached,
            ) => Ok(None),
            Err(other) => Err(EncodeError::Encode(other)),
        }
    }
}

/// The wall-clock interval between captures.
pub fn frame_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))
}

/// Fills a rav1e frame's I420 planes from RGBA capture.
///
/// BT.601 full-range, chroma box-averaged: a terminal's anti-aliased
/// glyphs survive 4:2:0 better when the sample is the mean of its 2×2
/// rather than its corner.
fn fill_i420(frame: &mut rav1e::prelude::Frame<u8>, image: &Image) {
    let width = image.width as usize;
    let height = image.height as usize;
    let rgba = &image.pixels;

    for (row, plane_row) in frame.planes[0].rows_iter_mut().enumerate() {
        for (col, out) in plane_row.iter_mut().enumerate().take(width) {
            let i = (row * width + col) * 4;
            *out = y_of(rgba[i], rgba[i + 1], rgba[i + 2]);
        }
    }

    let chroma_width = width.div_ceil(2);
    for (plane, is_cb) in [(1, true), (2, false)] {
        let mut rows = frame.planes[plane].rows_iter_mut();
        for crow in 0..height.div_ceil(2) {
            let Some(out_row) = rows.next() else { break };
            for (ccol, out) in out_row.iter_mut().enumerate().take(chroma_width) {
                let mut acc_r = 0u32;
                let mut acc_g = 0u32;
                let mut acc_b = 0u32;
                let mut n = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (y, x) = (crow * 2 + dy, ccol * 2 + dx);
                        if y < height && x < width {
                            let i = (y * width + x) * 4;
                            acc_r += u32::from(rgba[i]);
                            acc_g += u32::from(rgba[i + 1]);
                            acc_b += u32::from(rgba[i + 2]);
                            n += 1;
                        }
                    }
                }
                *out = if is_cb {
                    cb_of(acc_r / n, acc_g / n, acc_b / n)
                } else {
                    cr_of(acc_r / n, acc_g / n, acc_b / n)
                };
            }
        }
    }
}

/// BT.601 luma, full range.
fn y_of(r: u8, g: u8, b: u8) -> u8 {
    let value = (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) + 500) / 1000;
    // The formula's ceiling is 255.5, so the only way this fails is a bug
    // in the constants — saturate rather than wrap, and let the
    // saturating be the type's word for it.
    u8::try_from(value).unwrap_or(u8::MAX)
}

/// A difference channel, bounded to a byte — signed until the last step
/// because chroma dips below zero by design.
fn channel(value: i64) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MIN)
}

/// BT.601 blue-difference chroma, full range.
fn cb_of(r: u32, g: u32, b: u32) -> u8 {
    channel((500 * i64::from(b) - 169 * i64::from(r) - 331 * i64::from(g)) / 1000 + 128)
}

/// BT.601 red-difference chroma, full range.
fn cr_of(r: u32, g: u32, b: u32) -> u8 {
    channel((500 * i64::from(r) - 419 * i64::from(g) - 81 * i64::from(b)) / 1000 + 128)
}

/// Encodes a captured frame as PNG, for the agent's `screenshot`.
///
/// The model sees RGB — the alpha a screen never varies is a channel of
/// weight it cannot use.
pub fn png(image: &Image) -> Result<Vec<u8>, super::ipc::IpcError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|error| {
            super::ipc::IpcError::Encode(format!("the screenshot could not start: {error}"))
        })?;
        let mut rgb = Vec::with_capacity(image.pixels.len() / 4 * 3);
        let (pixels, _rest) = image.pixels.as_chunks::<4>();
        for [r, g, b, _a] in pixels {
            rgb.extend_from_slice(&[*r, *g, *b]);
        }
        writer.write_image_data(&rgb).map_err(|error| {
            super::ipc::IpcError::Encode(format!("the screenshot could not encode: {error}"))
        })?;
    }
    Ok(out)
}

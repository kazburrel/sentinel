#![cfg_attr(not(test), no_std)]

//! Wire format for camera events sent from firmware to the server over the
//! hardened HTTP upload path (see `PROJECT_STATUS.md`, Milestones 13-14).
//!
//! An event body is a small fixed header followed by one or more
//! length-prefixed parts (thumbnail JPEG now, recorded video later, audio
//! optional after that). Hand-rolled rather than pulling in serde/postcard:
//! firmware is `no_std` with only a small on-chip heap (the PSRAM frame
//! buffers themselves are a plain-bytes-only arena -- see `camera.rs` /
//! `main.rs` for why), and this format only ever needs to describe "how
//! many bytes, what kind" around data that already lives in a buffer -- no
//! owned allocation required on either side of the wire. Every multi-byte
//! integer is little-endian, chosen explicitly rather than relying on the
//! target's native layout.
//!
//! Version 2 (bumped from the Milestone 14 v1 that only ever shipped in
//! this session's test binaries -- no deployed consumer to stay compatible
//! with): added an `event_id` so parts/retries can be correlated by
//! something firmware assigns, not just the server's receipt timestamp, and
//! added per-part `encoding`/`timestamp_ms`/`duration_ms` so a future audio
//! part can be aligned against the video part without guessing. Parsing is
//! also strict now -- a zero-part envelope and undeclared trailing bytes
//! after the last part are both rejected, not silently accepted.

pub const MAGIC: [u8; 4] = *b"CAM1";
pub const VERSION: u8 = 2;
/// magic(4) + version(1) + part_count(1) + event_id(8, little-endian)
pub const ENVELOPE_HEADER_LEN: usize = 14;
/// kind(1) + encoding(1) + timestamp_ms(4, LE) + duration_ms(4, LE) + len(4, LE)
pub const PART_HEADER_LEN: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    Thumbnail = 1,
    Video = 2,
    Audio = 3,
}

impl PartKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(PartKind::Thumbnail),
            2 => Some(PartKind::Video),
            3 => Some(PartKind::Audio),
            _ => None,
        }
    }
}

/// What's actually inside a part's payload bytes -- distinct from `PartKind`
/// (which just says "this is the thumbnail slot") because the same kind
/// could plausibly gain a second encoding later (e.g. a raw-frames video
/// today, a real container format down the line) without needing another
/// wire version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A single JPEG image, used by `Thumbnail` parts today.
    Jpeg = 1,
    /// `firmware::recorder::PsramRecorder`'s raw format: `[frame_len: u32
    /// LE][timestamp_ms: u32 LE][JPEG bytes]` repeated -- used by `Video`
    /// parts today.
    RecorderFrames = 2,
    /// Reserved for the future `Audio` part; nothing produces this yet.
    Pcm16Mono = 3,
}

impl Encoding {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Encoding::Jpeg),
            2 => Some(Encoding::RecorderFrames),
            3 => Some(Encoding::Pcm16Mono),
            _ => None,
        }
    }
}

/// Whether `encoding` is a sensible payload format for `kind` -- e.g. a
/// `Thumbnail` claiming to be `Pcm16Mono` is individually well-formed
/// (both are known enum values) but meaningless, so it's rejected rather
/// than stored and trusted downstream. Deliberately a whitelist of allowed
/// pairs, not a strict 1:1 mapping: a kind can gain a second valid encoding
/// later (e.g. a real video container alongside `RecorderFrames`) just by
/// adding a line here, no wire version bump required.
fn kind_allows_encoding(kind: PartKind, encoding: Encoding) -> bool {
    matches!(
        (kind, encoding),
        (PartKind::Thumbnail, Encoding::Jpeg)
            | (PartKind::Video, Encoding::RecorderFrames)
            | (PartKind::Audio, Encoding::Pcm16Mono)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u8),
    /// An envelope declaring zero parts is never valid -- every event has
    /// at least a thumbnail.
    EmptyEvent,
    UnknownPartKind(u8),
    UnknownEncoding(u8),
    /// Both `kind` and `encoding` are individually valid, known values, but
    /// this pairing is nonsensical (e.g. a `Thumbnail` claiming to be
    /// `Pcm16Mono`) -- rejected rather than stored and trusted by whatever
    /// reads `encoding` downstream to pick a decoder.
    InvalidEncodingForKind { kind: PartKind, encoding: Encoding },
    PartLengthExceedsBuffer,
    /// Bytes remained in the body after all declared parts were consumed --
    /// parsing must account for every byte, not just the parts it expected.
    TrailingBytes,
}

/// Encodes the fixed envelope header for an event with `part_count` parts
/// and firmware-assigned `event_id` (a per-boot-session monotonic counter
/// today -- see the test binaries -- good enough to correlate this event's
/// parts/retries without needing a real-time clock firmware doesn't have).
/// The caller writes this first, then each part's own header + payload in
/// turn (see `encode_part_header`) -- there is no single in-memory buffer
/// holding the whole event, since a recorded video part can be megabytes
/// and firmware's on-chip heap is only tens of KB.
pub fn encode_envelope_header(part_count: u8, event_id: u64) -> [u8; ENVELOPE_HEADER_LEN] {
    let mut out = [0u8; ENVELOPE_HEADER_LEN];
    out[..4].copy_from_slice(&MAGIC);
    out[4] = VERSION;
    out[5] = part_count;
    out[6..14].copy_from_slice(&event_id.to_le_bytes());
    out
}

/// Encodes one part's header (kind, encoding, this part's start offset and
/// duration in milliseconds relative to the event, and payload length). The
/// payload bytes are written separately, straight from wherever they
/// already live (capture buffer, PSRAM arena slice) -- never copied through
/// this crate. `duration_ms` is `0` for instantaneous parts (a thumbnail
/// still image).
pub fn encode_part_header(
    kind: PartKind,
    encoding: Encoding,
    timestamp_ms: u32,
    duration_ms: u32,
    len: u32,
) -> [u8; PART_HEADER_LEN] {
    let mut out = [0u8; PART_HEADER_LEN];
    out[0] = kind.as_u8();
    out[1] = encoding.as_u8();
    out[2..6].copy_from_slice(&timestamp_ms.to_le_bytes());
    out[6..10].copy_from_slice(&duration_ms.to_le_bytes());
    out[10..14].copy_from_slice(&len.to_le_bytes());
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvelopeHeader {
    pub version: u8,
    pub part_count: u8,
    pub event_id: u64,
}

/// Parses the fixed envelope header from the start of `buf`, returning it
/// along with the remaining bytes (the parts).
pub fn decode_envelope_header(buf: &[u8]) -> Result<(EnvelopeHeader, &[u8]), EnvelopeError> {
    if buf.len() < ENVELOPE_HEADER_LEN {
        return Err(EnvelopeError::TooShort);
    }
    if buf[..4] != MAGIC {
        return Err(EnvelopeError::BadMagic);
    }
    let version = buf[4];
    if version != VERSION {
        return Err(EnvelopeError::UnsupportedVersion(version));
    }
    let part_count = buf[5];
    if part_count == 0 {
        return Err(EnvelopeError::EmptyEvent);
    }
    let event_id = u64::from_le_bytes(buf[6..14].try_into().unwrap());
    Ok((
        EnvelopeHeader {
            version,
            part_count,
            event_id,
        },
        &buf[ENVELOPE_HEADER_LEN..],
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartHeader {
    pub kind: PartKind,
    pub encoding: Encoding,
    pub timestamp_ms: u32,
    pub duration_ms: u32,
    pub len: u32,
}

/// Iterates over the length-prefixed parts following the envelope header.
/// Borrows from the original buffer throughout -- no allocation, so it
/// works identically whether the caller then copies each part into its own
/// file (the `server` crate, on std) or just inspects it on firmware.
///
/// Once all `part_count` parts have been yielded, any bytes still remaining
/// in the buffer are reported as `EnvelopeError::TrailingBytes` instead of
/// being silently ignored -- parsing must account for the entire body, not
/// just as many parts as it expected.
pub struct PartsIter<'a> {
    remaining: &'a [u8],
    parts_left: u8,
}

impl<'a> PartsIter<'a> {
    pub fn new(after_envelope_header: &'a [u8], part_count: u8) -> Self {
        PartsIter {
            remaining: after_envelope_header,
            parts_left: part_count,
        }
    }
}

impl<'a> Iterator for PartsIter<'a> {
    type Item = Result<(PartHeader, &'a [u8]), EnvelopeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.parts_left == 0 {
            return if self.remaining.is_empty() {
                None
            } else {
                // Force every subsequent call to keep reporting the same
                // error rather than looping forever if a caller ignores
                // the short-circuit and keeps polling.
                self.remaining = &[];
                Some(Err(EnvelopeError::TrailingBytes))
            };
        }
        self.parts_left -= 1;

        if self.remaining.len() < PART_HEADER_LEN {
            return Some(Err(EnvelopeError::TooShort));
        }
        let kind_byte = self.remaining[0];
        let kind = match PartKind::from_u8(kind_byte) {
            Some(k) => k,
            None => return Some(Err(EnvelopeError::UnknownPartKind(kind_byte))),
        };
        let encoding_byte = self.remaining[1];
        let encoding = match Encoding::from_u8(encoding_byte) {
            Some(e) => e,
            None => return Some(Err(EnvelopeError::UnknownEncoding(encoding_byte))),
        };
        if !kind_allows_encoding(kind, encoding) {
            return Some(Err(EnvelopeError::InvalidEncodingForKind { kind, encoding }));
        }
        let timestamp_ms = u32::from_le_bytes(self.remaining[2..6].try_into().unwrap());
        let duration_ms = u32::from_le_bytes(self.remaining[6..10].try_into().unwrap());
        let len = u32::from_le_bytes(self.remaining[10..14].try_into().unwrap());

        let after_header = &self.remaining[PART_HEADER_LEN..];
        if after_header.len() < len as usize {
            return Some(Err(EnvelopeError::PartLengthExceedsBuffer));
        }
        let (payload, rest) = after_header.split_at(len as usize);
        self.remaining = rest;
        Some(Ok((
            PartHeader {
                kind,
                encoding,
                timestamp_ms,
                duration_ms,
                len,
            },
            payload,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: PartKind, encoding: Encoding, payload: &[u8]) -> PartHeader {
        PartHeader {
            kind,
            encoding,
            timestamp_ms: 0,
            duration_ms: 0,
            len: payload.len() as u32,
        }
    }

    #[test]
    fn round_trips_a_single_thumbnail_part() {
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 42));
        body.extend_from_slice(&encode_part_header(
            PartKind::Thumbnail,
            Encoding::Jpeg,
            0,
            0,
            jpeg.len() as u32,
        ));
        body.extend_from_slice(jpeg);

        let (header, rest) = decode_envelope_header(&body).expect("valid header");
        assert_eq!(header.version, VERSION);
        assert_eq!(header.part_count, 1);
        assert_eq!(header.event_id, 42);

        let parts: Vec<_> = PartsIter::new(rest, header.part_count)
            .collect::<Result<_, _>>()
            .expect("valid parts");
        assert_eq!(
            parts,
            vec![(part(PartKind::Thumbnail, Encoding::Jpeg, jpeg), &jpeg[..])]
        );
    }

    #[test]
    fn round_trips_thumbnail_and_video_parts_with_timestamps() {
        let thumb = b"thumb-bytes";
        let video = b"much-longer-video-bytes-here";
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(2, 7));
        body.extend_from_slice(&encode_part_header(
            PartKind::Thumbnail,
            Encoding::Jpeg,
            0,
            0,
            thumb.len() as u32,
        ));
        body.extend_from_slice(thumb);
        body.extend_from_slice(&encode_part_header(
            PartKind::Video,
            Encoding::RecorderFrames,
            120,
            5000,
            video.len() as u32,
        ));
        body.extend_from_slice(video);

        let (header, rest) = decode_envelope_header(&body).expect("valid header");
        let parts: Vec<_> = PartsIter::new(rest, header.part_count)
            .collect::<Result<_, _>>()
            .expect("valid parts");
        assert_eq!(
            parts,
            vec![
                (part(PartKind::Thumbnail, Encoding::Jpeg, thumb), &thumb[..]),
                (
                    PartHeader {
                        kind: PartKind::Video,
                        encoding: Encoding::RecorderFrames,
                        timestamp_ms: 120,
                        duration_ms: 5000,
                        len: video.len() as u32,
                    },
                    &video[..]
                ),
            ]
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut body = vec![b'X', b'X', b'X', b'X', VERSION, 1];
        body.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(decode_envelope_header(&body), Err(EnvelopeError::BadMagic));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut body = encode_envelope_header(1, 0).to_vec();
        body[4] = 99;
        assert_eq!(
            decode_envelope_header(&body),
            Err(EnvelopeError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_truncated_header() {
        let body = [b'C', b'A', b'M', b'1', VERSION];
        assert_eq!(decode_envelope_header(&body), Err(EnvelopeError::TooShort));
    }

    #[test]
    fn rejects_zero_part_envelope() {
        let body = encode_envelope_header(0, 1);
        assert_eq!(decode_envelope_header(&body), Err(EnvelopeError::EmptyEvent));
    }

    #[test]
    fn rejects_trailing_bytes_after_last_part() {
        let jpeg = b"fake-jpeg-bytes";
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 0));
        body.extend_from_slice(&encode_part_header(
            PartKind::Thumbnail,
            Encoding::Jpeg,
            0,
            0,
            jpeg.len() as u32,
        ));
        body.extend_from_slice(jpeg);
        body.extend_from_slice(b"unexpected-trailing-junk"); // not declared by part_count

        let (header, rest) = decode_envelope_header(&body).expect("valid envelope header");
        let err = PartsIter::new(rest, header.part_count)
            .collect::<Result<Vec<_>, _>>()
            .expect_err("trailing bytes should be rejected");
        assert_eq!(err, EnvelopeError::TrailingBytes);
    }

    #[test]
    fn rejects_unknown_part_kind() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 0));
        body.push(99); // unknown kind byte
        body.push(Encoding::Jpeg.as_u8());
        body.extend_from_slice(&0u32.to_le_bytes()); // timestamp_ms
        body.extend_from_slice(&0u32.to_le_bytes()); // duration_ms
        body.extend_from_slice(&5u32.to_le_bytes()); // len
        body.extend_from_slice(b"12345");

        let (header, rest) = decode_envelope_header(&body).expect("valid envelope header");
        let err = PartsIter::new(rest, header.part_count)
            .collect::<Result<Vec<_>, _>>()
            .expect_err("unknown part kind should be rejected");
        assert_eq!(err, EnvelopeError::UnknownPartKind(99));
    }

    #[test]
    fn rejects_unknown_encoding() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 0));
        body.push(PartKind::Thumbnail.as_u8());
        body.push(99); // unknown encoding byte
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&5u32.to_le_bytes());
        body.extend_from_slice(b"12345");

        let (header, rest) = decode_envelope_header(&body).expect("valid envelope header");
        let err = PartsIter::new(rest, header.part_count)
            .collect::<Result<Vec<_>, _>>()
            .expect_err("unknown encoding should be rejected");
        assert_eq!(err, EnvelopeError::UnknownEncoding(99));
    }

    #[test]
    fn rejects_mismatched_kind_and_encoding() {
        // Both Thumbnail and Pcm16Mono are individually valid, known
        // values -- just a nonsensical pairing (a still image claiming to
        // be audio) that must still be rejected.
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 0));
        body.extend_from_slice(&encode_part_header(
            PartKind::Thumbnail,
            Encoding::Pcm16Mono,
            0,
            0,
            5,
        ));
        body.extend_from_slice(b"12345");

        let (header, rest) = decode_envelope_header(&body).expect("valid envelope header");
        let err = PartsIter::new(rest, header.part_count)
            .collect::<Result<Vec<_>, _>>()
            .expect_err("mismatched kind/encoding should be rejected");
        assert_eq!(
            err,
            EnvelopeError::InvalidEncodingForKind {
                kind: PartKind::Thumbnail,
                encoding: Encoding::Pcm16Mono,
            }
        );
    }

    #[test]
    fn rejects_part_length_exceeding_buffer() {
        let mut body = Vec::new();
        body.extend_from_slice(&encode_envelope_header(1, 0));
        body.extend_from_slice(&encode_part_header(PartKind::Thumbnail, Encoding::Jpeg, 0, 0, 100)); // claims 100 bytes
        body.extend_from_slice(b"only 5"); // far fewer actually present

        let (header, rest) = decode_envelope_header(&body).expect("valid envelope header");
        let err = PartsIter::new(rest, header.part_count)
            .collect::<Result<Vec<_>, _>>()
            .expect_err("truncated part should be rejected");
        assert_eq!(err, EnvelopeError::PartLengthExceedsBuffer);
    }
}

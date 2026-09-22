//! A second opinion on SMF bytes: an encoder and decoder written from the
//! spec, sharing no code with `midly`.
//!
//! # Why this exists rather than an oracle crate
//!
//! `tutti-midi-file` wraps `midly`, so `midly` cannot be its own second
//! opinion — it *is* the thing under test at the chunk/varint layer. The
//! alternatives were checked and none qualifies: `nodi` wraps midly, `rimd` is
//! unmaintained, and `midi-file` is a thin reader. So the repo's oracle
//! pattern (`tests/oracle_decode.rs` encodes with hound and decodes with
//! symphonia; `tests/oracle_pitch.rs` checks tutti's YIN against another
//! crate's) has no off-the-shelf partner here, and the honest substitute is to
//! write one.
//!
//! # Why this rather than checked-in `.mid` fixtures
//!
//! This repo has no checked-in test data of any kind, and its stated reason is
//! good: `tests/roundtrip.rs:46` argues that an expected value should be
//! recomputed from first principles rather than pinned to bytes, "the whole
//! point is a second opinion". A committed corpus would also only cover the
//! files someone thought to commit. A builder covers those *and* the malformed
//! cases — a zero division, an SMPTE header, a truncated chunk — which is
//! where a hand-rolled parser actually breaks, and it puts the bytes next to
//! the assertion that cares about them.
//!
//! # What it implements
//!
//! Enough of SMF 1.0 to be a real cross-check: `MThd`/`MTrk` framing, the
//! variable-length quantity, running status, meta events (tempo, time
//! signature, track name, end-of-track), sysex, and both metrical and SMPTE
//! division headers. It is intentionally literal — no cleverness, so that when
//! it and `midly` disagree, the disagreement is informative.

#![allow(dead_code)]
// Each integration-test binary compiles this tree separately, and no single
// binary uses every item. Same reason, and same allow, as the plugin hosts'
// `tests/support/mod.rs`.

// ---------------------------------------------------------------- writing --

/// The variable-length quantity: 7 bits per byte, big-endian, continuation bit
/// set on every byte but the last. Spelled out rather than borrowed, because
/// the encoder under test delegates exactly this to `midly`.
pub fn write_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 4];
    let mut n = 0;
    loop {
        buf[n] = (value & 0x7F) as u8;
        n += 1;
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        // Continuation bit on everything but the final byte.
        out.push(buf[i] | if i == 0 { 0x00 } else { 0x80 });
    }
}

/// Read a VLQ, returning the value and how many bytes it consumed.
pub fn read_vlq(data: &[u8]) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    for (i, &b) in data.iter().take(4).enumerate() {
        value = (value << 7) | u32::from(b & 0x7F);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// One event to encode, as a delta plus raw status/data bytes.
pub struct RawEvent {
    pub delta: u32,
    /// The full event, status byte first. Meta events start `0xFF`, sysex
    /// `0xF0`; channel messages carry their own status.
    pub bytes: Vec<u8>,
}

impl RawEvent {
    pub fn new(delta: u32, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            delta,
            bytes: bytes.into(),
        }
    }

    pub fn note_on(delta: u32, channel: u8, key: u8, vel: u8) -> Self {
        Self::new(delta, vec![0x90 | (channel & 0x0F), key, vel])
    }

    pub fn note_off(delta: u32, channel: u8, key: u8, vel: u8) -> Self {
        Self::new(delta, vec![0x80 | (channel & 0x0F), key, vel])
    }

    /// Set Tempo, microseconds per quarter note.
    pub fn tempo(delta: u32, us_per_quarter: u32) -> Self {
        let b = us_per_quarter.to_be_bytes();
        Self::new(delta, vec![0xFF, 0x51, 0x03, b[1], b[2], b[3]])
    }

    pub fn track_name(delta: u32, name: &str) -> Self {
        let mut v = vec![0xFF, 0x03];
        write_vlq(&mut v, name.len() as u32);
        v.extend_from_slice(name.as_bytes());
        Self::new(delta, v)
    }

    /// `numerator`/`2^denominator_pow`, with the usual 24/8 clock defaults.
    pub fn time_signature(delta: u32, numerator: u8, denominator_pow: u8) -> Self {
        Self::new(
            delta,
            vec![0xFF, 0x58, 0x04, numerator, denominator_pow, 24, 8],
        )
    }

    /// An F0 sysex with a length-prefixed payload, terminated 0xF7.
    pub fn sysex(delta: u32, payload: &[u8]) -> Self {
        let mut v = vec![0xF0];
        write_vlq(&mut v, payload.len() as u32 + 1);
        v.extend_from_slice(payload);
        v.push(0xF7);
        Self::new(delta, v)
    }

    /// A channel message with its status byte omitted — legal when the
    /// previous event carried the same status. `midly` must reinstate it.
    pub fn running_status(delta: u32, data: impl Into<Vec<u8>>) -> Self {
        Self::new(delta, data)
    }
}

/// How the header declares its time division.
#[derive(Clone, Copy)]
pub enum Division {
    /// Ticks per quarter note. Zero is representable and must be rejected by
    /// any reader that divides by it.
    Metrical(u16),
    /// SMPTE: negative frames-per-second byte plus ticks-per-frame. Not a
    /// tempo-relative grid, so `tutti-midi-file` refuses it.
    Smpte { fps: i8, ticks_per_frame: u8 },
}

impl Division {
    fn to_bytes(self) -> [u8; 2] {
        match self {
            Division::Metrical(tpq) => tpq.to_be_bytes(),
            Division::Smpte {
                fps,
                ticks_per_frame,
            } => [fps as u8, ticks_per_frame],
        }
    }
}

/// Build a complete SMF byte stream. `format` is 0, 1 or 2.
pub fn build_smf(format: u16, division: Division, tracks: &[Vec<RawEvent>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&division.to_bytes());

    for events in tracks {
        let mut body = Vec::new();
        for e in events {
            write_vlq(&mut body, e.delta);
            body.extend_from_slice(&e.bytes);
        }
        // End of Track is mandatory and is what tells a reader the chunk is
        // whole; omitting it is a different (also interesting) test.
        write_vlq(&mut body, 0);
        body.extend_from_slice(&[0xFF, 0x2F, 0x00]);

        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
    }
    out
}

// ---------------------------------------------------------------- reading --

/// A decoded event: absolute tick from track start, plus its bytes with any
/// running status made explicit.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedEvent {
    pub tick: u64,
    pub status: u8,
    pub data: Vec<u8>,
}

/// What the header declared, as read back.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSmf {
    pub format: u16,
    /// Raw division word — interpret with [`Division`]'s rules.
    pub division: [u8; 2],
    pub tracks: Vec<Vec<DecodedEvent>>,
}

impl DecodedSmf {
    /// Ticks per quarter, if the division is metrical.
    pub fn ticks_per_quarter(&self) -> Option<u16> {
        if self.division[0] & 0x80 == 0 {
            Some(u16::from_be_bytes(self.division))
        } else {
            None
        }
    }

    /// Absolute ticks of every Note On with non-zero velocity, in order.
    pub fn note_on_ticks(&self) -> Vec<u64> {
        self.tracks
            .iter()
            .flatten()
            .filter(|e| e.status & 0xF0 == 0x90 && e.data.get(1).is_some_and(|&v| v != 0))
            .map(|e| e.tick)
            .collect()
    }

    /// The first Set Tempo payload, as microseconds per quarter.
    pub fn first_tempo_us(&self) -> Option<u32> {
        self.tracks.iter().flatten().find_map(|e| {
            (e.status == 0xFF && e.data.first() == Some(&0x51)).then(|| {
                let p = &e.data[2..];
                u32::from_be_bytes([0, p[0], p[1], p[2]])
            })
        })
    }
}

/// How many data bytes a channel status takes.
fn channel_data_len(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

/// Decode an SMF byte stream. Returns `None` on anything malformed, so a
/// fuzzing-style test can assert "never panics, never accepts".
pub fn decode_smf(data: &[u8]) -> Option<DecodedSmf> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return None;
    }
    let header_len = u32::from_be_bytes(data[4..8].try_into().ok()?) as usize;
    if header_len < 6 || data.len() < 8 + header_len {
        return None;
    }
    let format = u16::from_be_bytes(data[8..10].try_into().ok()?);
    let ntracks = u16::from_be_bytes(data[10..12].try_into().ok()?) as usize;
    let division: [u8; 2] = data[12..14].try_into().ok()?;

    let mut pos = 8 + header_len;
    let mut tracks = Vec::new();
    for _ in 0..ntracks {
        if pos + 8 > data.len() || &data[pos..pos + 4] != b"MTrk" {
            return None;
        }
        let len = u32::from_be_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        pos += 8;
        let end = pos.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        tracks.push(decode_track(&data[pos..end])?);
        pos = end;
    }
    Some(DecodedSmf {
        format,
        division,
        tracks,
    })
}

fn decode_track(mut body: &[u8]) -> Option<Vec<DecodedEvent>> {
    let mut out = Vec::new();
    let mut tick = 0u64;
    // Running status: a channel event may omit its status byte, reusing the
    // previous one. Meta and sysex clear it.
    let mut running: Option<u8> = None;

    while !body.is_empty() {
        let (delta, n) = read_vlq(body)?;
        tick += u64::from(delta);
        body = &body[n..];
        let &first = body.first()?;

        let (status, data): (u8, Vec<u8>) = if first == 0xFF {
            running = None;
            let kind = *body.get(1)?;
            let (len, n) = read_vlq(body.get(2..)?)?;
            let len = len as usize;
            let start = 2 + n;
            let payload = body.get(start..start + len)?;
            let mut d = vec![kind, len as u8];
            d.extend_from_slice(payload);
            body = &body[start + len..];
            if kind == 0x2F {
                out.push(DecodedEvent {
                    tick,
                    status: 0xFF,
                    data: d,
                });
                break;
            }
            (0xFF, d)
        } else if first == 0xF0 || first == 0xF7 {
            running = None;
            let (len, n) = read_vlq(body.get(1..)?)?;
            let len = len as usize;
            let start = 1 + n;
            let payload = body.get(start..start + len)?.to_vec();
            body = &body[start + len..];
            (first, payload)
        } else if first & 0x80 != 0 {
            running = Some(first);
            let need = channel_data_len(first);
            let d = body.get(1..1 + need)?.to_vec();
            body = &body[1 + need..];
            (first, d)
        } else {
            // Running status: no status byte, data only.
            let status = running?;
            let need = channel_data_len(status);
            let d = body.get(0..need)?.to_vec();
            body = &body[need..];
            (status, d)
        };

        out.push(DecodedEvent { tick, status, data });
    }
    Some(out)
}

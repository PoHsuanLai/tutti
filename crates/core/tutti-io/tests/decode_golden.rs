//! Golden decode digests for the committed fixtures in `assets/audio/`.
//!
//! Pins what the engine's decode path produces, bit for bit, so that moving
//! the decoder out of the fundsp fork (design doc 013, Phase 0) can be shown to
//! change nothing. Every sample of every channel is folded into an FNV-1a
//! digest over its `f32` bit pattern — a tolerance would let a changed
//! conversion or a dropped packet through.
//!
//! Not feature-gated: the crate's dev-dependency on itself turns every codec
//! on for its tests, so this fails to compile rather than silently vanishing
//! if that ever stops being true.

use std::path::PathBuf;

use tutti_io::{AudioIn, FileIn, Wave};

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../assets/audio")
        .join(name)
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn f32(&mut self, v: f32) {
        self.u64(u64::from(v.to_bits()));
    }
}

fn wave_digest(w: &Wave) -> u64 {
    let mut h = Fnv::new();
    h.u64(w.channels() as u64);
    h.u64(w.len() as u64);
    h.u64(w.sample_rate().get().to_bits());
    for c in 0..w.channels() {
        for i in 0..w.len() {
            h.f32(w.at(c, i));
        }
    }
    h.0
}

/// Read the whole file through `FileIn` in odd-sized chunks, so reads cross
/// packet boundaries mid-packet, and digest the interleaved stream.
fn stream_digest(name: &str) -> (u64, usize) {
    let mut dec = FileIn::open(asset(name)).expect("open");
    let ch = dec.layout().count() as usize;
    let mut buf = vec![0.0f32; 333 * ch];
    let mut h = Fnv::new();
    let mut frames = 0usize;
    loop {
        let n = dec.poll_into(&mut buf).0;
        if n == 0 {
            break;
        }
        for &s in &buf[..n * ch] {
            h.f32(s);
        }
        frames += n;
    }
    (h.0, frames)
}

/// `(file, load digest, streamed digest)`. The lossy rows are asserted only
/// where they were recorded; see [`bit_portable`].
///
/// Recorded from the fundsp fork's decoder (`fundsp::read` / `fundsp::stream`,
/// reached as `tutti_core::{Wave, FileIn}`) before the decoder moved to this
/// crate; this file was written against that path first and then moved, so
/// these rows are the fork's output and the rehomed decoder reproduces them.
///
/// Mutation: shortening every decoded packet by one frame in the shared
/// packet-decode helper changes every row that has audio (checked).
const GOLDEN: &[(&str, u64, u64)] = &[
    ("stereo_s16.wav", 0x3a5e069030cee9fe, 0x5983063d609be7ed),
    ("stereo_s24.wav", 0x9e907ce1c8607f10, 0xb9f775ef850a7f23),
    ("mono_48k.wav", 0xcdad4db62795bafc, 0x48ca681f1e6cefd5),
    ("surround_51.wav", 0xe130b7339862140b, 0xdf0812ca019020c5),
    // Same digests as the 24-bit WAV: ffmpeg writes FLAC at 24 bits from the
    // same generator, and a lossless decode must land on the same samples.
    ("stereo.flac", 0x9e907ce1c8607f10, 0xb9f775ef850a7f23),
    ("stereo.mp3", 0x83e5be1d21986f6e, 0x03180728f1ec4892),
    // The one row the move did not carry over unchanged. The fork's streamed
    // digest here was the EMPTY digest (0xcbf29ce484222325): `FileIn` read zero
    // frames from this file. The fix is in `FileIn::decode_next_packet`, and
    // `the_stream_reads_every_frame_the_load_does` shows this new digest is the
    // load's samples, bit for bit.
    ("stereo.ogg", 0x40a6b908a599a73c, 0x833652fd27bdc9c9),
];

/// Whether a fixture's golden digest holds on every platform.
///
/// **Lossless decodes are portable; lossy ones are not.** PCM and FLAC decode
/// integers and scale them to `f32` — exact IEEE arithmetic, identical
/// everywhere. The MP3 and Vorbis decoders build their windows, IMDCT twiddles
/// and (Vorbis) floor curves from `sin`/`cos`/`tan`/`exp`/`powf`
/// (`symphonia-bundle-mp3`'s `hybrid_synthesis.rs`/`synthesis.rs`/`stereo.rs`,
/// `symphonia-core`'s `dsp::mdct`, `symphonia-codec-vorbis`'s `window.rs` and
/// `floor.rs`). Those are libm quality-of-implementation, not IEEE
/// correctly-rounded, and differ in the last ulp between the platform libms —
/// the same reason `render_is_bit_identical_to_the_audionode_era` is gated off
/// MSVC. The MP3 digest did differ on macOS CI. The Ogg one happened to match
/// there, which is luck rather than a property, so it is gated too.
fn bit_portable(name: &str) -> bool {
    !(name.ends_with(".mp3") || name.ends_with(".ogg"))
}

fn check_digests(rows: impl Iterator<Item = &'static (&'static str, u64, u64)>) {
    let mut failures = Vec::new();
    for &(name, want_load, want_stream) in rows {
        let w = Wave::load(asset(name)).expect("load");
        let got_load = wave_digest(&w);
        let (got_stream, frames) = stream_digest(name);
        if got_load != want_load || got_stream != want_stream {
            failures.push(format!(
                "(\"{name}\", {got_load:#018x}, {got_stream:#018x}), // {} ch, {} frames, streamed {frames}",
                w.channels(),
                w.len()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "digest mismatch:\n{}",
        failures.join("\n")
    );
}

/// The lossless rows, bit for bit, on every platform.
#[test]
fn decoded_samples_match_the_golden_digests() {
    check_digests(GOLDEN.iter().filter(|r| bit_portable(r.0)));
}

/// The lossy rows, bit for bit, on the target they were recorded on: Linux
/// x86_64 (glibc's libm). Elsewhere the platform libm may round the decoder's
/// trig differently in the last ulp (see [`bit_portable`]), so the portable
/// guard for these files is `decoded_levels_match_on_every_platform`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn lossy_decodes_match_the_golden_digests_where_recorded() {
    check_digests(GOLDEN.iter().filter(|r| !bit_portable(r.0)));
}

/// `(file, channels, frames, per-channel (RMS, peak))` of `Wave::load`,
/// recorded on Linux x86_64.
const LEVELS: &[(&str, usize, usize, &[(f64, f64)])] = &[
    (
        "stereo_s16.wav",
        2,
        11025,
        &[(0.380794707, 0.699768066), (0.333542624, 0.649963379)],
    ),
    (
        "stereo_s24.wav",
        2,
        11025,
        &[(0.380794746, 0.699767232), (0.333542693, 0.649975181)],
    ),
    ("mono_48k.wav", 1, 9600, &[(0.424264137, 0.600006104)]),
    (
        "surround_51.wav",
        6,
        4410,
        &[
            (0.070711370, 0.100006104),
            (0.141422299, 0.200012207),
            (0.212133152, 0.299987793),
            (0.035354544, 0.049987793),
            (0.282842225, 0.399993896),
            (0.353554930, 0.500000000),
        ],
    ),
    (
        "stereo.flac",
        2,
        11025,
        &[(0.380794746, 0.699767232), (0.333542693, 0.649975181)],
    ),
    (
        "stereo.mp3",
        2,
        12672,
        &[(0.337157817, 0.669130385), (0.295668269, 0.633588970)],
    ),
    (
        "stereo.ogg",
        2,
        11840,
        &[(0.370645079, 0.729985654), (0.324173438, 0.660415709)],
    ),
];

/// The portable guard: on **every** platform, each fixture decodes to the
/// recorded width and frame count, and each channel's RMS and peak are within
/// `1e-5` relative of the recorded values. A last-ulp libm difference moves
/// these by ~1e-7; a real regression — a missing packet, a swapped or dropped
/// channel, a gain error of 0.01 dB — moves them far past 1e-5. This is what
/// catches a lossy-decode regression where the bit-exact digest is not
/// asserted.
///
/// Mutations (checked), each in the decode loop every container shares:
/// swapping channels by index fails on RMS; dropping each packet's last frame
/// fails on frame count; scaling every decoded sample by 1.0001 fails on RMS.
#[test]
fn decoded_levels_match_on_every_platform() {
    const REL: f64 = 1e-5;
    let close = |got: f64, want: f64| (got - want).abs() <= REL * want.abs();
    for &(name, channels, frames, levels) in LEVELS {
        let w = Wave::load(asset(name)).expect("load");
        assert_eq!(w.channels(), channels, "{name}: channels");
        assert_eq!(w.len(), frames, "{name}: frames");
        for (c, &(rms, peak)) in levels.iter().enumerate() {
            let ch = w.channel(c);
            let got_rms = (ch.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>()
                / ch.len() as f64)
                .sqrt();
            let got_peak = ch.iter().fold(0.0f64, |m, &s| m.max(f64::from(s).abs()));
            assert!(
                close(got_rms, rms),
                "{name}: channel {c} RMS {got_rms} vs {rms}"
            );
            assert!(
                close(got_peak, peak),
                "{name}: channel {c} peak {got_peak} vs {peak}"
            );
        }
    }
    assert_eq!(LEVELS.len(), GOLDEN.len(), "every fixture has a level row");
}

/// Streaming a file from the start yields every frame the whole-file load
/// does, bit for bit, in every container.
///
/// This is the test that found `FileIn` reading **nothing** from an Ogg file:
/// Vorbis's first packet decodes to zero frames (it only primes the overlap),
/// and the streamer took a zero-frame packet for end-of-stream. A streamed Ogg
/// clip therefore played as silence from its first block, and `probe` called
/// it streamable because the container reports a frame count.
///
/// Mutation: restoring `return Ok(frames)` for a zero-frame packet in
/// `FileIn::decode_next_packet` fails this on `stereo.ogg` (checked).
#[test]
fn the_stream_reads_every_frame_the_load_does() {
    for &(name, _, _) in GOLDEN {
        let w = Wave::load(asset(name)).expect("load");
        let ch = w.channels();
        let mut dec = FileIn::open(asset(name)).expect("open");
        let mut buf = vec![0.0f32; 333 * ch];
        let mut at = 0usize;
        loop {
            let n = dec.poll_into(&mut buf).0;
            if n == 0 {
                break;
            }
            for f in 0..n {
                for c in 0..ch {
                    assert_eq!(
                        buf[f * ch + c].to_bits(),
                        w.at(c, at + f).to_bits(),
                        "{name}: frame {} channel {c}",
                        at + f
                    );
                }
            }
            at += n;
        }
        assert_eq!(at, w.len(), "{name}: streamed frame count");
    }
}

/// The header agrees with the decode: `probe_metadata`'s width, rate **and
/// frame count** are what `Wave::load` actually produces, for every fixture.
///
/// Restores the fork's `probe_metadata_matches_full_load`, which the move
/// deleted along with the fork's WAV writer it depended on.
///
/// Mutation: have `probe_metadata` report `n_frames.map(|n| n + 1)` and every
/// row fails (checked).
#[test]
fn the_probe_agrees_with_the_full_load() {
    for &(name, _, _) in GOLDEN {
        let w = Wave::load(asset(name)).expect("load");
        let meta = Wave::probe_metadata(asset(name)).expect("probe");
        assert_eq!(meta.channels, w.channels(), "{name}: probed width");
        assert_eq!(
            meta.sample_rate as f64,
            w.sample_rate().get(),
            "{name}: probed rate"
        );
        assert_eq!(
            meta.total_frames,
            Some(w.len() as u64),
            "{name}: probed frame count"
        );
    }
}

/// Where a seek landed, as a lag against the whole-file load: the offset `d`
/// at which the frames read after `seek(start)` equal the load's frames at
/// `start + d`, or `None` if no offset in `-4096..=4096` matches. Used only to
/// make a failure say *how far off* a seek was.
fn lag_of(w: &Wave, start: usize, out: &[f32]) -> Option<i64> {
    let ch = w.channels();
    let n = out.len() / ch;
    (-4096i64..=4096).find(|&d| {
        let s = start as i64 + d;
        s >= 0
            && (s as usize + n) <= w.len()
            && (0..n).all(|f| {
                (0..ch).all(|c| out[f * ch + c].to_bits() == w.at(c, s as usize + f).to_bits())
            })
    })
}

/// After a seek, `FileIn` produces exactly the frames a whole-file load has
/// at that position — for **every** container, lossy included. Position is the
/// contract; the fixtures are also sample-identical after a seek, so that is
/// asserted, bit for bit.
///
/// Starts cover frame 1 (inside the first packet), 700 (mid-packet for every
/// codec here), a third of the way in, and 100 frames from the end. Each is
/// seeked on a fresh decoder, then again in reverse order on one decoder, so a
/// seek from a mid-stream state is covered too.
///
/// This pins the fix for an Ogg seek landing 1024 frames late (576 from frame
/// 1): after `decoder.reset()` the first Vorbis packet decodes to zero frames
/// and is skipped, but the preroll discard was counted from the seek's
/// `actual_ts` rather than from the packet that produced audio — and when that
/// primer is the packet containing `start`, no discard can recover it, so the
/// seek has to step back past it.
///
/// Mutations (checked): the pre-fix `seek` fails `stereo.ogg` at lag +576
/// (start 1); disabling only the step back past the primer fails it at lag
/// +575; discarding one extra preroll frame fails `stereo_s16.wav` at lag +1.
#[test]
fn seek_lands_on_the_whole_file_load_frames() {
    for &(name, _, _) in GOLDEN {
        let w = Wave::load(asset(name)).expect("load");
        let ch = w.channels();
        let len = w.len();
        let starts = [1, 700, len / 3, len - 100];

        let check = |dec: &mut FileIn, start: usize, how: &str| {
            dec.seek(start as u64).expect("seek");
            assert_eq!(dec.cursor(), start as u64, "{name}: cursor after seek");
            let want = 257.min(len - start);
            let mut out = vec![0.0f32; want * ch];
            assert_eq!(
                dec.poll_into(&mut out).0,
                want,
                "{name}: read count after {how} seek to {start}"
            );
            let exact = (0..want).all(|f| {
                (0..ch).all(|c| out[f * ch + c].to_bits() == w.at(c, start + f).to_bits())
            });
            assert!(
                exact,
                "{name}: {how} seek to {start} did not land on the load's frames; \
                 it matches the load at lag {:?}",
                lag_of(&w, start, &out)
            );
        };

        for &start in &starts {
            let mut dec = FileIn::open(asset(name)).expect("open");
            assert!(dec.seekable(), "{name}: every fixture must be seekable");
            check(&mut dec, start, "fresh");
        }
        let mut dec = FileIn::open(asset(name)).expect("open");
        for &start in starts.iter().rev() {
            check(&mut dec, start, "repeated");
        }
    }
}

/// `WaveAsset::from_bytes` — the Bevy loader's path, which probes from the
/// bytes alone with no extension hint — decodes to the same samples as the
/// path-based load, for every fixture.
///
/// Compiled only with `bevy` (the CI "dark features" job runs
/// `cargo nextest run -p tutti-io --features bevy`), because `WaveAsset` is
/// the Asset derive and exists only there.
///
/// Mutation: have `from_bytes` decode only the first half of its bytes and
/// every row fails (checked). (A wrong extension hint does not: symphonia
/// probes the bytes regardless.)
#[cfg(feature = "bevy")]
#[test]
fn from_bytes_decodes_what_load_decodes() {
    // Against this platform's own `Wave::load`, not the recorded golden: both
    // sides run the same decoder here, so this is bit-exact everywhere.
    for &(name, _, _) in GOLDEN {
        let want = wave_digest(&Wave::load(asset(name)).expect("load"));
        let bytes = std::fs::read(asset(name)).expect("read fixture");
        let asset = tutti_io::WaveAsset::from_bytes(&bytes).expect("decode bytes");
        assert_eq!(wave_digest(&asset), want, "{name}: from_bytes digest");
    }
}

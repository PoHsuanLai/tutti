//! The streaming tier places a file's channels as the memory tier does.
//!
//! A disk voice reads its ring at the file's own width and order, and the
//! `Place` built from the file's topology when the voice is taken puts the
//! channels where the engine has them: a WAV 7.1 (rears before sides) is
//! permuted, a channel no speaker names is refused, and a narrowing with an
//! unordered end is refused, all as `TakeVoiceError::Place` at the call that
//! takes the voice. A file already in the engine's order is read as it was.
//!
//! The files are written here, as WAVE_FORMAT_EXTENSIBLE float with the
//! channel mask hand-written: channel `c` holds the constant `(c + 1) / 16`
//! (a dyadic value, so the read is exact), which makes a channel in the wrong
//! place a wrong value. The expected figures come from the engine's speaker
//! order (`ChannelTopology::smpte`), not from the placement under test.

use std::path::Path;

use tutti_graph::contract;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{Command, DiskStreamer, DiskVoice, PlaceError, TakeVoiceError};
use tutti_types::{
    Beat, Bpm, ChannelLayout, ChannelTopology, FoldEnd, FoldError, SamplePosition, SampleRate,
};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
const FRAMES: u32 = 8_192;

/// The value of file channel `c`.
fn value(c: usize) -> f32 {
    (c + 1) as f32 / 16.0
}

/// A float32 `WAVE_FORMAT_EXTENSIBLE` file of [`FRAMES`] frames, `channels`
/// wide, declaring `mask`, channel `c` holding [`value`]`(c)`.
fn write_wav(path: &Path, channels: u16, mask: u32) {
    let mut fmt = Vec::new();
    fmt.extend(0xFFFEu16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend((SR as u32).to_le_bytes());
    fmt.extend((SR as u32 * 4 * u32::from(channels)).to_le_bytes());
    fmt.extend((4 * channels).to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(22u16.to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    fmt.extend([
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    let mut data = Vec::new();
    for _ in 0..FRAMES {
        for c in 0..usize::from(channels) {
            data.extend(value(c).to_le_bytes());
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend((fmt.len() as u32).to_le_bytes());
    out.extend(&fmt);
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&data);
    std::fs::write(path, out).expect("writes the file");
}

/// A hand-stepped butler streaming `path` on channel 0, primed.
fn streamer_on(path: &Path) -> DiskStreamer {
    let mut streamer = DiskStreamer::manual(SampleRate(SR), Default::default()).expect("builds");
    streamer
        .commands()
        .send(Command::Stream {
            channel_index: 0,
            file_path: path.to_path_buf(),
            offset: SamplePosition(0.0),
        })
        .expect("the butler is alive");
    assert!(
        streamer.step_until_settled(1_000) < 1_000,
        "the ring primes"
    );
    streamer
}

/// A file written to a temp dir and streamed.
fn stream(channels: u16, mask: u32) -> (tempfile::TempDir, DiskStreamer) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, channels, mask);
    let streamer = streamer_on(&path);
    (dir, streamer)
}

/// The last block of a few rendered from `voice`, planar: the ring is full and
/// the start-up is over.
fn settled_block(voice: DiskVoice) -> Vec<Vec<f32>> {
    let mut voice = contract::prepared(voice, SampleRate(SR), BLOCK);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut last = Vec::new();
    for _ in 0..8 {
        let env = clock.env(BLOCK, SR);
        last = contract::drive_input(&mut voice, &env, &[], &[], &[]).audio;
        clock.advance(BLOCK as i64, SR);
    }
    last
}

/// Every sample of `block`'s channel `c` is exactly `want[c]`.
fn assert_channels(block: &[Vec<f32>], want: &[f32], what: &str) {
    assert_eq!(block.len(), want.len(), "{what}: width");
    for (c, (lane, &w)) in block.iter().zip(want).enumerate() {
        assert!(
            lane.iter().all(|s| s.to_bits() == w.to_bits()),
            "{what}: channel {c} is not {w}: {:?}",
            &lane[..4]
        );
    }
}

const WAV_7_1: u32 = 0x63F; // FL FR FC LFE BL BR SL SR: the rears before the sides.
const WAV_5_1: u32 = 0x3F; // FL FR FC LFE BL BR

/// A WAV-order 7.1 file plays in the engine's order: the sides before the
/// rears. The expectation is derived from the speakers, not the placement:
/// engine channel `i` plays the file channel that feeds the same speaker.
///
/// Mutation (run): `take_streaming_unit` building `Place::native` in place of
/// `Place::for_topology` (the disk tier as it was) -> the voice plays the
/// file's order and fails on channel 4.
#[test]
fn a_wav_order_seven_one_is_permuted_to_the_engines_order() {
    let (_dir, streamer) = stream(8, WAV_7_1);
    let voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("a 7.1 file places at its own width");
    let file = {
        use tutti_types::Speaker::*;
        ChannelTopology::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight,
            SideLeft,
            SideRight,
        ])
    };
    let engine = ChannelTopology::smpte(ChannelLayout::from(8u16)).expect("7.1 has an order");
    let want: Vec<f32> = engine
        .positions()
        .iter()
        .map(|&s| value(file.index_of(s).expect("the same speakers")))
        .collect();
    // The figures differ from the file's order, or this proves nothing.
    assert_ne!(want, (0..8).map(value).collect::<Vec<_>>());
    assert_channels(&settled_block(voice), &want, "7.1 in WAV order");
}

/// A file with a channel no speaker names (bit 25 is a second LFE) is refused
/// when the voice is taken, at any width, and the stream keeps its reader.
///
/// Mutation (run): `take_streaming_unit` falling back to `Place::native` when
/// `Place::for_topology` refuses (`unwrap_or_else`) -> the take succeeds and
/// fails the first assertion. (The `Unknown` check in `for_topology` is also
/// `Fold::to_width`'s, so removing it alone changes nothing for a wide file.)
#[test]
fn a_file_with_a_second_lfe_is_refused() {
    let (_dir, streamer) = stream(2, 0x0200_0001);
    let status = streamer.status();
    let refused = TakeVoiceError::Place(PlaceError::Fold(FoldError::Unnamed {
        end: FoldEnd::Source,
        channel: 1,
    }));
    assert_eq!(
        status.take_disk_voice(0, Beat::new(0.0), None).err(),
        Some(refused)
    );
    assert_eq!(
        status
            .take_disk_voice_into(0, ChannelLayout::STEREO, Beat::new(0.0), None)
            .err(),
        Some(refused)
    );
    // Refused before the reader was taken: it is not `ReaderTaken`.
    assert_ne!(
        status.take_disk_voice(0, Beat::new(0.0), None).err(),
        Some(TakeVoiceError::ReaderTaken)
    );
}

/// A 5.1 file into a 5-wide voice has no order to narrow by: refused, and the
/// stream keeps its reader, so the same file then takes at its own width and
/// at stereo (a fold by position).
///
/// Mutation (run): the width given to `Place::for_topology` replaced by the
/// file's own in `take_streaming_unit` -> the 5-wide take succeeds and fails
/// the first assertion. (Also the `unwrap_or_else` fallback above.)
#[test]
fn a_five_one_file_into_a_five_wide_voice_is_refused() {
    let (_dir, streamer) = stream(6, WAV_5_1);
    let status = streamer.status();
    let refused = status
        .take_disk_voice_into(0, ChannelLayout::from(5u16), Beat::new(0.0), None)
        .err();
    assert!(
        matches!(
            refused,
            Some(TakeVoiceError::Place(PlaceError::Fold(
                FoldError::Unplaced { channel: 5, .. }
            )))
        ),
        "{refused:?}"
    );
    let stereo = status
        .take_disk_voice_into(0, ChannelLayout::STEREO, Beat::new(0.0), None)
        .expect("a refusal leaves the reader, and 5.1 folds to stereo");
    assert_eq!(settled_block(stereo).len(), 2);
}

/// A stereo file plays as it was: the voice is two wide and every sample is the
/// file's, to the bit.
///
/// Mutation (run): `read_ring` reading channel `src - 1 - c` for `c` ->
/// the channels are exchanged and it fails (and the 7.1 case with it).
#[test]
fn a_stereo_file_is_unchanged() {
    let (_dir, streamer) = stream(2, 0x3);
    let voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("stereo places");
    assert_channels(&settled_block(voice), &[value(0), value(1)], "stereo");
}

/// A fork for an offline render, which reads the file itself, places it as the
/// live voice does.
///
/// Mutation (run): `offline_render` passing `&Place::FAN` in place of the
/// voice's placement -> the fork plays the file's order and fails (the live
/// cases, which do not fork, still pass).
#[test]
fn a_fork_places_the_file_as_the_live_voice_does() {
    use tutti_graph::{ForkMode, IntoNode, Prepare};
    use tutti_types::Samples;
    let (_dir, streamer) = stream(8, WAV_7_1);
    let voice = streamer
        .status()
        .take_disk_voice(0, Beat::new(0.0), None)
        .expect("places");
    let forked = voice
        .clone()
        .into_parts()
        .fork
        .expect("forkable")
        .fork(ForkMode::Offline)
        .expect("forks");
    let mut node = forked.node;
    node.prepare(&Prepare::new(SampleRate(SR), Samples(BLOCK)));
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let env = clock.env(BLOCK, SR);
    let out = contract::drive_input(&mut *node, &env, &[], &[], &[]).audio;
    // Engine order: FL FR FC LFE SL SR BL BR from the file's FL FR FC LFE BL BR SL SR.
    let want: Vec<f32> = [0, 1, 2, 3, 6, 7, 4, 5].map(value).to_vec();
    assert_channels(&out, &want, "the fork of a 7.1 in WAV order");
}

/// A file wider than a frame folds into it by position (10 and 12 channels
/// into 8, where the tier cut them to their first 8): a file with no mask is
/// given the first speakers of WAVE's order, all of them named.
///
/// Mutation (run): `take_streaming_unit` building `Place::native(width)` ->
/// the read has no fold (a `debug_assert` in `Place::frame`, a read of the
/// first 8 in release) and the test fails.
#[test]
fn a_file_wider_than_a_frame_folds_into_it() {
    for channels in [10, 12] {
        let (_dir, streamer) = stream(channels, 0);
        let voice = streamer
            .status()
            .take_disk_voice(0, Beat::new(0.0), None)
            .expect("named speakers fold by position");
        let block = settled_block(voice);
        assert_eq!(block.len(), 8, "{channels} channels");
        assert!(
            block.iter().flatten().any(|&s| s != 0.0),
            "{channels} channels: the fold is silent"
        );
    }
}

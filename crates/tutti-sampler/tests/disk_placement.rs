//! The streaming tier places a file's channels as the memory tier does.
//!
//! A disk voice reads its ring at the file's own width and order, and the
//! `Place` built from the file's topology when the stream opens puts the
//! channels where the engine has them: a WAV 7.1 (rears before sides) is
//! permuted, a channel no speaker names is refused, and a narrowing with an
//! unordered end is refused, all as `OpenError::Placement` from the open that
//! would have made the stream. A file already in the engine's order is read as
//! it was.
//!
//! The files are written here, as WAVE_FORMAT_EXTENSIBLE float with the
//! channel mask hand-written: channel `c` holds the constant `(c + 1) / 16`
//! (a dyadic value, so the read is exact), which makes a channel in the wrong
//! place a wrong value. The expected figures come from the engine's speaker
//! order (`ChannelTopology::smpte`), not from the placement under test.

use std::path::Path;
use tutti_types::MaxBlock;

use tutti_graph::contract;
use tutti_sampler::testing::{MockTransport, settle};
use tutti_sampler::{
    ByHand, Direction, DiskStreamer, DiskVoice, LoopSetting, OpenError, PlaceError, Placement,
    Stream, StreamReader,
};
use tutti_types::{
    Beat, Bpm, ChannelLayout, ChannelTopology, FoldEnd, FoldError, SamplePosition, SampleRate,
};

/// Only inside an `assert_no_alloc` region does this abort.
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

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

/// A hand-stepped disk streamer, and the answer to opening `path` on it (at
/// `width`, or the file's own), primed.
fn open(
    path: &Path,
    width: Option<ChannelLayout>,
) -> (
    DiskStreamer<ByHand>,
    Result<(Stream, StreamReader), OpenError>,
) {
    let mut streamer = DiskStreamer::manual(SampleRate(SR), Default::default()).expect("builds");
    let opening = match width {
        Some(width) => streamer.open_into(path, Placement::HEAD, width),
        None => streamer.open(path, Placement::HEAD),
    };
    let reader = settle(&mut streamer, opening);
    if reader.is_ok() {
        assert!(
            streamer.step_until_settled(1_000) < 1_000,
            "the ring primes"
        );
    }
    (streamer, reader)
}

/// A hand-stepped streamer and the handle of the stream it streams: both are
/// held while the voice plays (dropping the handle ends the stream).
struct Rig {
    streamer: DiskStreamer<ByHand>,
    stream: Stream,
}

/// A file written to a temp dir and streamed at its own width.
fn stream(channels: u16, mask: u32) -> (tempfile::TempDir, Rig, StreamReader) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, channels, mask);
    let (streamer, answer) = open(&path, None);
    let (stream, reader) = answer.expect("a placeable file opens");
    (dir, Rig { streamer, stream }, reader)
}

/// The voice that plays `reader`, placed at beat 0 for the file's length.
fn voice_of(reader: StreamReader) -> DiskVoice {
    DiskVoice::from_reader(reader, Beat::new(0.0), None)
}

/// The last block of a few rendered from `voice`, planar: the ring is full and
/// the start-up is over.
fn settled_block(voice: DiskVoice) -> Vec<Vec<f32>> {
    let mut voice = contract::prepared(voice, SampleRate(SR), BLOCK);
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut last = Vec::new();
    for _ in 0..8 {
        let env = clock.env(BLOCK, SampleRate::new(SR));
        last = contract::drive_input(&mut voice, &env, &[], &[], &[]).audio;
        clock.advance(BLOCK as i64, SampleRate::new(SR));
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
/// Mutation (run): `open_stream_region` building `Place::native` in place of
/// `Place::for_topology` (the disk tier as it was) -> the voice plays the
/// file's order and fails on channel 4.
#[test]
fn a_wav_order_seven_one_is_permuted_to_the_engines_order() {
    let (_dir, _streamer, reader) = stream(8, WAV_7_1);
    let voice = voice_of(reader);
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
/// when the stream is opened, at any width: the open fails with the placement's
/// refusal, where it used to open and fail at the voice.
///
/// Mutation (run): `open_stream_region` falling back to `Place::native` when
/// `Place::for_topology` refuses (`unwrap_or_else`) -> the open succeeds and
/// fails the first assertion. (The `Unknown` check in `for_topology` is also
/// `Fold::to_width`'s, so removing it alone changes nothing for a wide file.)
#[test]
fn a_file_with_a_second_lfe_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, 2, 0x0200_0001);
    // The fixture is what it says: bit 25 is the decoder's second LFE
    // (`tutti-codec`'s `a_flag_with_no_speaker_keeps_its_slot_as_unknown`).
    assert_eq!(
        tutti_codec::FileIn::open(&path)
            .expect("opens")
            .topology()
            .positions(),
        [
            tutti_types::Speaker::FrontLeft,
            tutti_types::Speaker::Unknown(25)
        ]
    );
    let refused = OpenError::Placement(PlaceError::Fold(FoldError::Unnamed {
        end: FoldEnd::Source,
        channel: 1,
    }));
    for width in [None, Some(ChannelLayout::STEREO)] {
        let (_streamer, answer) = open(&path, width);
        assert_eq!(answer.err(), Some(refused.clone()), "width {width:?}");
    }
}

/// A 5.1 file into a 5-wide voice has no order to narrow by: the open is
/// refused, and a refusal leaves nothing behind, so the same file then opens at
/// stereo (a fold by position) and at its own width.
///
/// Mutation (run): the width given to `Place::for_topology` replaced by the
/// file's own in `open_stream_region` -> the 5-wide open succeeds and fails
/// the first assertion. (Also the `unwrap_or_else` fallback above.)
#[test]
fn a_five_one_file_into_a_five_wide_voice_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("placed.wav");
    write_wav(&path, 6, WAV_5_1);
    let (mut streamer, refused) = open(&path, Some(ChannelLayout::from(5u16)));
    let refused = refused.err();
    assert!(
        matches!(
            refused,
            Some(OpenError::Placement(PlaceError::Fold(
                FoldError::Unplaced { channel: 5, .. }
            )))
        ),
        "{refused:?}"
    );
    // Refused again, the same way: nothing the first refusal did changed it.
    let opening = streamer.open_into(&path, Placement::HEAD, ChannelLayout::from(5u16));
    assert_eq!(settle(&mut streamer, opening).err(), refused);
    let opening = streamer.open_into(&path, Placement::HEAD, ChannelLayout::STEREO);
    let (_stereo_stream, stereo) = settle(&mut streamer, opening).expect("5.1 folds to stereo");
    assert_eq!(settled_block(voice_of(stereo)).len(), 2);
    let opening = streamer.open(&path, Placement::HEAD);
    let (_own_stream, own) = settle(&mut streamer, opening).expect("and opens at its own width");
    assert_eq!(settled_block(voice_of(own)).len(), 6);
}

/// A stereo file plays as it was: the voice is two wide and every sample is the
/// file's, to the bit.
///
/// Mutation (run): `read_ring` reading channel `src - 1 - c` for `c` ->
/// the channels are exchanged and it fails (and the 7.1 case with it).
#[test]
fn a_stereo_file_is_unchanged() {
    let (_dir, _streamer, reader) = stream(2, 0x3);
    let voice = voice_of(reader);
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
    let (_dir, _streamer, reader) = stream(8, WAV_7_1);
    let voice = voice_of(reader);
    let forked = voice
        .clone()
        .into_parts()
        .fork
        .expect("forkable")
        .fork(ForkMode::Offline)
        .expect("forks");
    let mut node = forked.node;
    node.prepare(&Prepare::new(SampleRate(SR), MaxBlock::of(Samples(BLOCK))));
    let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let env = clock.env(BLOCK, SampleRate::new(SR));
    let out = contract::drive_input(&mut *node, &env, &[], &[], &[]).audio;
    // Engine order: FL FR FC LFE SL SR BL BR from the file's FL FR FC LFE BL BR SL SR.
    let want: Vec<f32> = [0, 1, 2, 3, 6, 7, 4, 5].map(value).to_vec();
    assert_channels(&out, &want, "the fork of a 7.1 in WAV order");
}

/// What a placed file plays in the engine's 7.1 order (FL FR FC LFE SL SR BL
/// BR), channel `c` of the file holding [`value`]`(c)`: the figures `Fold`'s
/// rules give (a speaker the destination has goes straight across; one it
/// lacks folds into its nearest at -3 dB, summed first where the file has
/// overhead speakers), written out here and not asked of `Place`.
struct Fixture {
    channels: u16,
    mask: u32,
    /// The decoder's names for the file's channels, pinned so a change in how
    /// a mask-less file is named fails here and not as a wrong figure.
    speakers: &'static [tutti_types::Speaker],
    want: [f32; 8],
}

fn fixtures() -> [Fixture; 3] {
    use tutti_types::Speaker::*;
    let m = tutti_types::M3DB;
    let v = value;
    [
        // The rears before the sides.
        Fixture {
            channels: 8,
            mask: WAV_7_1,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                SideLeft,
                SideRight,
            ],
            want: [v(0), v(1), v(2), v(3), v(6), v(7), v(4), v(5)],
        },
        // No mask: WAVE's first ten speakers. The left and right of centre
        // fold into the centre, the back centre into both backs, and the right
        // side has no source.
        Fixture {
            channels: 10,
            mask: 0,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                FrontLeftOfCenter,
                FrontRightOfCenter,
                BackCenter,
                SideLeft,
            ],
            want: [
                v(0),
                v(1),
                v(2) + m * v(6) + m * v(7),
                v(3),
                v(9),
                0.0,
                v(4) + m * v(8),
                v(5) + m * v(8),
            ],
        },
        // Twelve: the right side and a top centre join. The top centre (an
        // overhead speaker) folds into the front pair.
        Fixture {
            channels: 12,
            mask: 0,
            speakers: &[
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
                FrontLeftOfCenter,
                FrontRightOfCenter,
                BackCenter,
                SideLeft,
                SideRight,
                TopCenter,
            ],
            want: [
                v(0) + m * v(11),
                v(1) + m * v(11),
                v(2) + m * (v(6) + v(7)),
                v(3),
                v(9),
                v(10),
                v(4) + m * v(8),
                v(5) + m * v(8),
            ],
        },
    ]
}

/// A file with a Fixture's shape, streamed.
fn stream_fixture(f: &Fixture) -> (tempfile::TempDir, Rig, StreamReader) {
    let (dir, rig, reader) = stream(f.channels, f.mask);
    assert_eq!(
        tutti_codec::FileIn::open(dir.path().join("placed.wav"))
            .expect("opens")
            .topology()
            .positions(),
        f.speakers
    );
    (dir, rig, reader)
}

/// Every sample of `block` is within rounding of `want` (a fold sums in `f32`).
fn assert_near(block: &[Vec<f32>], want: &[f32; 8], what: &str) {
    assert_eq!(block.len(), 8, "{what}: width");
    for (c, (lane, &w)) in block.iter().zip(want).enumerate() {
        assert!(
            lane.iter().all(|s| (s - w).abs() < 1e-6),
            "{what}: channel {c} is not {w}: {:?}",
            &lane[..4]
        );
    }
}

/// A file wider than a frame folds into it by position (10 and 12 channels into
/// 8, where the tier cut them to their first 8), to the figures `Fold`'s rules
/// give, and a WAV-order 7.1 is permuted.
///
/// Mutation (run): `open_stream_region` building `Place::native(width)` ->
/// the read has no fold (a `debug_assert` in `Place::frame`, a read of the
/// first 8 in release) and the figures are the file's first eight channels.
#[test]
fn a_file_wider_than_a_frame_folds_into_it() {
    for f in fixtures() {
        let (_dir, _streamer, reader) = stream_fixture(&f);
        let voice = voice_of(reader);
        assert_near(
            &settled_block(voice),
            &f.want,
            &format!("{} channels", f.channels),
        );
    }
}

/// `n` blocks of `node` from a transport rolling from beat 0, planar.
fn render(node: &mut dyn tutti_graph::Node, clock: &MockTransport, n: usize) -> Vec<Vec<f32>> {
    let mut out: Vec<Vec<f32>> = Vec::new();
    for _ in 0..n {
        let env = clock.env(BLOCK, SampleRate::new(SR));
        let b = contract::drive_input(node, &env, &[], &[], &[]).audio;
        if out.is_empty() {
            out = vec![Vec::new(); b.len()];
        }
        for (o, c) in out.iter_mut().zip(b) {
            o.extend(c);
        }
        clock.advance(BLOCK as i64, SampleRate::new(SR));
    }
    out
}

/// Fork `voice` as an export does, not yet prepared.
fn fork_unprepared(voice: &DiskVoice) -> Box<dyn tutti_graph::Node> {
    use tutti_graph::{ForkMode, IntoNode};
    voice
        .clone()
        .into_parts()
        .fork
        .expect("forkable")
        .fork(ForkMode::Offline)
        .expect("forks")
        .node
}

/// A boxed node, as a node (for `Direct`).
struct Boxed(Box<dyn tutti_graph::Node>);

impl tutti_graph::Node for Boxed {
    fn shape(&self) -> tutti_graph::Shape {
        self.0.shape()
    }
    fn prepare(&mut self, p: &tutti_graph::Prepare) {
        self.0.prepare(p);
    }
    fn process(
        &mut self,
        cx: &tutti_graph::Cx<'_>,
        io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        self.0.process(cx, io)
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}

/// [`fork_unprepared`], prepared.
fn fork(voice: &DiskVoice) -> Box<dyn tutti_graph::Node> {
    let mut node = fork_unprepared(voice);
    node.prepare(&tutti_graph::Prepare::new(
        SampleRate(SR),
        tutti_types::MaxBlock::of(tutti_types::Samples(BLOCK)),
    ));
    node
}

/// Channel `c` of `direct`'s last block, all channels, as owned lanes
/// (allocating: call it outside a guard).
fn lanes<N: tutti_graph::Node>(direct: &contract::Direct<N>) -> Vec<Vec<f32>> {
    (0..8).map(|c| direct.output(c).to_vec()).collect()
}

fn loop_1000_3000() -> LoopSetting {
    LoopSetting::On {
        start: SamplePosition(1_000.0),
        end: SamplePosition(3_000.0),
        crossfade_frames: 64,
    }
}

/// A fork reads a placed file through every one of its paths: forward, looped
/// (the loop wrapping three times, its crossfade included), reversed, and the
/// paged read of a file wider than a frame, to the same figures as the live
/// voice.
///
/// Mutation (run): `Pages::read_looped_into` handed `&Place::FAN` -> the looped
/// rows (and the looped no-alloc rows) fail, the forward and reversed ones not. Mutation (run): the reverse arm of `OfflineRead::read_into`
/// handed `&Place::FAN` -> the reversed row alone fails. Mutation (run): the
/// page's `MAX_WAVE_CHANNELS` back to `MAX_SAMPLER_CHANNELS` (the tier's old
/// cut to 8) -> the 10- and 12-channel rows fail.
#[test]
fn a_fork_places_every_read_path() {
    for f in fixtures() {
        for path in ["forward", "looped", "reversed"] {
            let (_dir, mut rig, reader) = stream_fixture(&f);
            if path == "looped" {
                rig.stream.set_loop(loop_1000_3000()).expect("alive");
                assert!(rig.streamer.step_until_settled(1_000) < 1_000);
            }
            let voice = voice_of(reader);
            if path == "reversed" {
                rig.stream.set_direction(Direction::Reverse).expect("alive");
                assert!(rig.streamer.step_until_settled(1_000) < 1_000);
            }
            let mut node = fork(&voice);
            let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
            // 6 000 frames: three trips round the 2 000-frame loop, and well
            // inside the 8 192-frame file when it is not looped.
            let out = render(&mut *node, &clock, 6_000 / BLOCK);
            let what = format!("{} channels, {path}", f.channels);
            for chunk in 0..out[0].len() / BLOCK {
                let block: Vec<Vec<f32>> = out
                    .iter()
                    .map(|l| l[chunk * BLOCK..(chunk + 1) * BLOCK].to_vec())
                    .collect();
                assert_near(&block, &f.want, &format!("{what}, block {chunk}"));
            }
        }
    }
}

/// A fork's placed read allocates nothing once its page is in (forward and
/// looped, the permuting and the folding file).
///
/// Mutation (run): `Place::frame` collecting the fold's source into a `Vec`
/// -> the folding rows abort in the guarded region.
#[test]
fn a_fork_placed_read_is_allocation_free() {
    for f in fixtures() {
        for looped in [false, true] {
            let (_dir, mut rig, reader) = stream_fixture(&f);
            if looped {
                rig.stream.set_loop(loop_1000_3000()).expect("alive");
                assert!(rig.streamer.step_until_settled(1_000) < 1_000);
            }
            let voice = voice_of(reader);
            let mut direct =
                contract::Direct::new(Boxed(fork_unprepared(&voice)), SampleRate(SR), BLOCK);
            let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
            // The first block opens the file and pages it in.
            direct.block_in(&clock.env(BLOCK, SampleRate::new(SR)));
            clock.advance(BLOCK as i64, SampleRate::new(SR));
            assert_no_alloc::assert_no_alloc(|| {
                for _ in 0..80 {
                    direct.block_in(&clock.env(BLOCK, SampleRate::new(SR)));
                    clock.advance(BLOCK as i64, SampleRate::new(SR));
                }
            });
            assert_near(
                &lanes(&direct),
                &f.want,
                &format!("{} channels", f.channels),
            );
        }
    }
}

/// A live placed voice allocates nothing in a block that crosses a loop edit
/// (the reader's crossfade from the old mapping, `cross_switch`) or a loop's
/// wrap, and plays its figures throughout.
///
/// Mutation (run): `cross_switch`'s read passing `&Place::FAN` -> the 7.1 and
/// the fold's blocks in the crossfade are in the file's order and fail.
/// Mutation (run): `Place::frame` collecting into a `Vec` -> aborts.
#[test]
fn a_loop_edit_and_wrap_on_a_placed_stream_allocate_nothing() {
    for f in fixtures() {
        let (_dir, mut rig, reader) = stream_fixture(&f);
        let voice = voice_of(reader);
        let mut direct = contract::Direct::new(voice, SampleRate(SR), BLOCK);
        let clock = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        for b in 0..90 {
            // Frame 2 816: the loop's end (3 000) is within the 256 frames a
            // change near the playhead is crossfaded over, so the reader
            // crosses a switch (`cross_switch`).
            if b == 44 {
                rig.stream.set_loop(loop_1000_3000()).expect("alive");
            }
            assert_no_alloc::assert_no_alloc(|| {
                direct.block_in(&clock.env(BLOCK, SampleRate::new(SR)))
            });
            clock.advance(BLOCK as i64, SampleRate::new(SR));
            let _ = rig.streamer.step_once();
            if b >= 4 {
                assert_near(
                    &lanes(&direct),
                    &f.want,
                    &format!("{} channels, block {b}", f.channels),
                );
            }
        }
    }
}

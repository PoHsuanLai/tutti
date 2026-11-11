//! A live 16-bit take and an export of the same signal agree when dithered
//! alike (doc 025 stage 7, decision A): both go through `tutti_dsp`'s
//! `DitherSink`, so the property is one path, not two renders compared.
//!
//! The signal is a quiet sine, where quantization without dither is far from
//! quantization with it; the take is cut into 100-frame pulls and the export
//! writes the planes whole, so only the noise sequence's continuity holds the
//! two together.

#![cfg(all(feature = "record", feature = "export"))]

use tutti_codec::{FileOut, Format, IntDepth, Pcm, Spec};
use tutti_export::{EventLossPolicy, FileTarget, Rendered, write_buffers};
use tutti_session::{DitherShape, DitherSink, DitherSpec, IDLE_PARK, Recorder};
use tutti_types::{ChannelLayout, Pull, SampleRate, Samples, Source};

const RATE: SampleRate = SampleRate(48_000.0);
const FRAMES: usize = 5_000;

fn tone(k: usize, channel: usize) -> f32 {
    0.3 * (k as f32 * 0.05 * (channel + 1) as f32).sin()
}

/// The planes of the signal, for the export.
fn planes() -> Vec<Vec<f32>> {
    (0..2)
        .map(|c| (0..FRAMES).map(|k| tone(k, c)).collect())
        .collect()
}

/// The same frames, as a live source that ends.
struct Take {
    pos: usize,
}

impl Source for Take {
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }
    fn pull(&mut self, out: &mut [f32]) -> Pull {
        let room = Samples::from_interleaved_len(out.len(), ChannelLayout::STEREO).get();
        let n = (FRAMES - self.pos).min(room);
        if n == 0 {
            return Pull::Ended;
        }
        for i in 0..n {
            out[i * 2] = tone(self.pos + i, 0);
            out[i * 2 + 1] = tone(self.pos + i, 1);
        }
        self.pos += n;
        Pull::Frames(Samples(n))
    }
}

fn samples(path: &std::path::Path) -> Vec<i16> {
    hound::WavReader::open(path)
        .expect("readable")
        .samples::<i16>()
        .map(|s| s.expect("a sample"))
        .collect()
}

fn record(path: &std::path::Path, dither: Option<DitherShape>) {
    let spec = Spec::new(Format::Wav(Pcm::Int16), ChannelLayout::STEREO, RATE);
    let file = FileOut::create(path, &spec).expect("creates");
    let spec = dither.map(|shape| DitherSpec {
        shape,
        lsb: IntDepth::Int16.lsb(),
    });
    let sink = DitherSink::new(file, spec);
    let rec = Recorder::start_with_capacity(Take { pos: 0 }, sink, Samples(100), IDLE_PARK)
        .expect("widths");
    rec.wait().expect("the take ends and finalizes");
}

fn export(path: &std::path::Path, dither: Option<DitherShape>) {
    let rendered = Rendered::new(planes(), RATE, 0).expect("planes");
    let target =
        FileTarget::new(path, Format::Wav(Pcm::Int16), ChannelLayout::STEREO).with_dither(dither);
    write_buffers(&rendered, &target, EventLossPolicy::Fail).expect("writes");
}

/// The 16-bit samples of a dithered take and of an export are the same, for
/// both shapes; and the dither is really there (an undithered take differs).
///
/// Mutations (each run, each fails this test): skip the `noise.apply` in
/// `DitherSink::write` (the take loses its dither); restart the noise at the
/// top of `write` (the take's 100-frame pulls each draw the sequence's head,
/// the export's one block does not). The seed is shared by both sides, so a
/// reseed cannot show here: `the_seeded_sequence_is_pinned` in `tutti-dsp`
/// holds it.
#[test]
fn a_live_take_and_an_export_agree_when_dithered_alike() {
    let dir = tempfile::tempdir().expect("dir");
    for shape in [DitherShape::Triangular, DitherShape::Rectangular] {
        let take = dir.path().join("take.wav");
        let exported = dir.path().join("export.wav");
        record(&take, Some(shape));
        export(&exported, Some(shape));
        let (take, exported) = (samples(&take), samples(&exported));
        assert_eq!(take.len(), FRAMES * 2);
        assert_eq!(take, exported, "{shape:?}: take and export differ");
    }

    let plain = dir.path().join("plain.wav");
    let dithered = dir.path().join("dithered.wav");
    record(&plain, None);
    record(&dithered, Some(DitherShape::Triangular));
    assert_ne!(samples(&plain), samples(&dithered), "the dither must show");
}

/// With no dither the take is what it always was: the plain quantization of
/// the frames, and the export with no dither writes the same.
///
/// Mutation (run): make `DitherSink::new` default a `None` spec to a
/// triangular one and the first assertion fails.
#[test]
fn without_dither_a_take_is_the_plain_quantization() {
    let dir = tempfile::tempdir().expect("dir");
    let take = dir.path().join("take.wav");
    record(&take, None);
    let expected: Vec<i16> = (0..FRAMES)
        .flat_map(|k| [tone(k, 0), tone(k, 1)])
        .map(|s| IntDepth::Int16.quantize(s) as i16)
        .collect();
    assert_eq!(samples(&take), expected);
    let exported = dir.path().join("off.wav");
    export(&exported, None);
    assert_eq!(samples(&exported), expected);
}

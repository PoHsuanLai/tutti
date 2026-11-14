use super::*;

const LSB16: Amplitude = Amplitude(1.0 / 32768.0);

fn triangular() -> Option<DitherSpec> {
    Some(DitherSpec {
        shape: DitherShape::Triangular,
        lsb: LSB16,
    })
}

/// Collects what reaches it, and refuses on demand.
#[derive(Default)]
struct Collect {
    got: Vec<f32>,
    writes: usize,
    refuse_after: Option<usize>,
}

impl Sink for Collect {
    type Sample = f32;
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }
    fn write(&mut self, block: Interleaved<'_, f32>) -> Result<(), SinkFailed> {
        if self.refuse_after.is_some_and(|n| self.writes >= n) {
            return Err(SinkFailed);
        }
        self.writes += 1;
        self.got.extend_from_slice(block.samples());
        Ok(())
    }
    fn finalize(self) -> std::io::Result<()> {
        Err(std::io::Error::other("inner finalize"))
    }
}

fn stereo(samples: &[f32]) -> Interleaved<'_, f32> {
    Interleaved::new(samples, ChannelLayout::STEREO).unwrap()
}

fn signal(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 97) as f32 / 97.0) - 0.5).collect()
}

/// TPDF is `r1 - r2` with both draws in `[0, 1]`: at most one LSB either way,
/// whatever the signal. One f32 rounding of the sum is allowed on top.
#[test]
fn the_noise_added_is_within_one_lsb() {
    let input = signal(4096 * 2);
    let mut sink = DitherSink::new(Collect::default(), triangular());
    sink.write(stereo(&input)).unwrap();
    let out = sink.into_inner().got;
    let mut widest = 0.0f32;
    for (o, i) in out.iter().zip(&input) {
        let d = (o - i).abs();
        assert!(d <= LSB16.0 + 1e-7, "|{o} - {i}| = {d} exceeds one LSB");
        widest = widest.max(d);
    }
    assert!(widest > 0.5 * LSB16.0, "the range is used, widest {widest}");
}

/// `None` hands the block on untouched, in one write.
#[test]
fn without_a_spec_it_is_the_identity() {
    let input = signal(3000 * 2);
    let mut sink = DitherSink::new(Collect::default(), None);
    assert!(sink.is_identity());
    sink.write(stereo(&input)).unwrap();
    let inner = sink.into_inner();
    assert_eq!(inner.got, input);
    assert_eq!(inner.writes, 1, "uncopied and unsplit");
}

/// Frames in are frames out: a block longer than the scratch is split but
/// loses nothing.
#[test]
fn frames_in_are_frames_out() {
    for frames in [0usize, 1, 1023, 1024, 1025, 5000] {
        let input = signal(frames * 2);
        let mut sink = DitherSink::new(Collect::default(), triangular());
        sink.write(stereo(&input)).unwrap();
        assert_eq!(sink.into_inner().got.len(), frames * 2, "frames {frames}");
    }
}

/// The noise does not depend on how the writes are cut: one block of N equals
/// pieces of any size, across chunk boundaries too.
#[test]
fn the_noise_continues_across_writes() {
    let input = signal(3000 * 2);
    let mut whole = DitherSink::new(Collect::default(), triangular());
    whole.write(stereo(&input)).unwrap();
    let mut cut = DitherSink::new(Collect::default(), triangular());
    for piece in input.chunks(2 * 700 + 2) {
        cut.write(stereo(piece)).unwrap();
    }
    assert_eq!(whole.into_inner().got, cut.into_inner().got);
}

/// The inner sink's refusal passes through, and so does its finalize.
#[test]
fn the_inner_sinks_errors_pass_through() {
    let inner = Collect {
        refuse_after: Some(0),
        ..Collect::default()
    };
    let mut sink = DitherSink::new(inner, triangular());
    assert_eq!(sink.write(stereo(&signal(8))), Err(SinkFailed));
    let mut open = DitherSink::new(Collect::default(), None);
    assert_eq!(open.write(stereo(&signal(8))), Ok(()));
    let err = open.finalize().unwrap_err();
    assert_eq!(err.to_string(), "inner finalize");
}

/// A view of another width than the inner sink's fails the write, dithered or
/// not, and nothing reaches the inner sink.
#[test]
fn a_view_of_the_wrong_width_fails_the_write() {
    let mono = Interleaved::new(&[0.0f32; 8], ChannelLayout::MONO).unwrap();
    for spec in [triangular(), None] {
        let mut sink = DitherSink::new(Collect::default(), spec);
        assert_eq!(sink.write(mono), Err(SinkFailed));
        assert_eq!(sink.into_inner().writes, 0);
    }
}

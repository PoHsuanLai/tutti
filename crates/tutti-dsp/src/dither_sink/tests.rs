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
    fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }
    fn write(&mut self, block: &[f32]) -> Result<(), SinkFailed> {
        if self.refuse_after.is_some_and(|n| self.writes >= n) {
            return Err(SinkFailed);
        }
        self.writes += 1;
        self.got.extend_from_slice(block);
        Ok(())
    }
    fn finalize(self) -> std::io::Result<()> {
        Err(std::io::Error::other("inner finalize"))
    }
}

fn signal(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 97) as f32 / 97.0) - 0.5).collect()
}

/// TPDF is `r1 - r2` with both draws in `[0, 1]`: at most one LSB either way,
/// whatever the signal. One f32 rounding of the sum is allowed on top.
///
/// Mutation (run): add instead of subtract the second draw in
/// `DitherNoise::triangular` and the bound is exceeded.
#[test]
fn the_noise_added_is_within_one_lsb() {
    let input = signal(4096 * 2);
    let mut sink = DitherSink::new(Collect::default(), triangular());
    sink.write(&input).unwrap();
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
///
/// Mutation (run): drop the `let Some(spec) ... else` early return so `None`
/// falls to a default spec and the samples change.
#[test]
fn without_a_spec_it_is_the_identity() {
    let input = signal(3000 * 2);
    let mut sink = DitherSink::new(Collect::default(), None);
    assert!(sink.is_identity());
    sink.write(&input).unwrap();
    let inner = sink.into_inner();
    assert_eq!(inner.got, input);
    assert_eq!(inner.writes, 1, "uncopied and unsplit");
}

/// Frames in are frames out: a block longer than the scratch is split but
/// loses nothing, and a trailing partial frame is dropped as every sink does.
///
/// Mutation (run): chunk `interleaved` instead of `interleaved[..whole]` and
/// the odd sample reaches the inner sink.
#[test]
fn frames_in_are_frames_out() {
    for frames in [0usize, 1, 1023, 1024, 1025, 5000] {
        let mut input = signal(frames * 2);
        input.push(0.9); // a partial frame
        let mut sink = DitherSink::new(Collect::default(), triangular());
        sink.write(&input).unwrap();
        assert_eq!(sink.into_inner().got.len(), frames * 2, "frames {frames}");
    }
}

/// The noise does not depend on how the writes are cut: one block of N equals
/// pieces of any size, across chunk boundaries too.
///
/// Mutation (run): build a fresh `DitherNoise` per chunk in `write`.
#[test]
fn the_noise_continues_across_writes() {
    let input = signal(3000 * 2);
    let mut whole = DitherSink::new(Collect::default(), triangular());
    whole.write(&input).unwrap();
    let mut cut = DitherSink::new(Collect::default(), triangular());
    for piece in input.chunks(2 * 700 + 2) {
        cut.write(piece).unwrap();
    }
    assert_eq!(whole.into_inner().got, cut.into_inner().got);
}

/// The inner sink's refusal passes through, and so does its finalize.
///
/// Mutation (run): `let _ = self.inner.write(..)` in `write` and the first
/// assertion fails.
#[test]
fn the_inner_sinks_errors_pass_through() {
    let inner = Collect {
        refuse_after: Some(0),
        ..Collect::default()
    };
    let mut sink = DitherSink::new(inner, triangular());
    assert_eq!(sink.write(&signal(8)), Err(SinkFailed));
    let mut open = DitherSink::new(Collect::default(), None);
    assert_eq!(open.write(&signal(8)), Ok(()));
    let err = open.finalize().unwrap_err();
    assert_eq!(err.to_string(), "inner finalize");
}

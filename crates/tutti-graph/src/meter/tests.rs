use super::*;

/// Folds `block` into `writer` as the op would.
fn fold(writer: &LevelWriter, block: &[f32]) {
    let stats = block_stats(block);
    writer.fold(stats.peak, f64::from(stats.sum_sq), block.len() as u32);
}

/// Constant `v` over `frames`.
fn feed(writer: &LevelWriter, v: f32, frames: usize) {
    fold(writer, &vec![v; frames]);
}

fn take(reader: &mut LevelReader) -> Level {
    reader.take().into()
}

fn read(samples: &[f32]) -> Level {
    let (w, mut r) = level_cell();
    fold(&w, samples);
    take(&mut r)
}

/// An amplitude-`a` sine, 64 frames per period.
fn sine(a: f32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| a * (std::f32::consts::TAU * i as f32 / 64.0).sin())
        .collect()
}

/// A constant block reads its analytic peak and RMS, exactly.
#[test]
fn a_constant_reads_exactly() {
    let (w, mut r) = level_cell();
    feed(&w, -0.5, 100);
    let l = take(&mut r);
    assert_eq!(l.peak.get(), 0.5);
    assert_eq!(l.rms.get(), 0.5);
    assert_eq!(l.frames, Samples(100));
}

/// The statistics of a block of any length agree with a plain loop: the
/// vector shape handles every tail.
#[test]
fn every_length_is_measured_whole() {
    for len in 0..40usize {
        let block: Vec<f32> = (0..len).map(|i| (i as f32 - 30.0) * 0.25).collect();
        let got = block_stats(&block);
        let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let sum: f32 = block.iter().map(|s| s * s).sum();
        assert_eq!(got.peak, peak, "len {len}");
        assert_eq!(got.sum_sq, sum, "len {len}");
    }
}

/// A read takes what was held and leaves nothing: the reads after it, with no
/// block between, are empty, whichever slot the cell reads (it alternates).
#[test]
fn a_read_takes_what_was_held() {
    let (w, mut r) = level_cell();
    feed(&w, 0.25, 64);
    assert_eq!(take(&mut r).peak.get(), 0.25);
    for _ in 0..3 {
        assert_eq!(take(&mut r), Level::default());
    }
}

/// Blocks between reads fold: the peak is the largest, the RMS covers all
/// the frames, whatever the block size.
#[test]
fn blocks_between_reads_fold() {
    let (w, mut r) = level_cell();
    feed(&w, 1.0, 64);
    feed(&w, 0.5, 192);
    let l = take(&mut r);
    assert_eq!(l.peak.get(), 1.0);
    assert_eq!(l.frames, Samples(256));
    // sqrt((64 * 1 + 192 * 0.25) / 256)
    assert_eq!(l.rms.get(), (112.0f64 / 256.0).sqrt() as f32);
}

/// A read with no frames has no peak and no energy: a block lands whole or
/// not at all. (The cell guarantees it; this pins the `Level` built on it.)
#[test]
fn an_empty_read_is_silence() {
    let (_w, mut r) = level_cell();
    let l = take(&mut r);
    assert_eq!(
        (l.peak.get(), l.rms.get(), l.frames),
        (0.0, 0.0, Samples(0))
    );
}

/// A NaN sample does not set the peak, whether it falls in a vector pass
/// or the tail.
#[test]
fn nan_does_not_set_the_peak() {
    assert_eq!(block_stats(&[0.5, f32::NAN, 0.25]).peak, 0.5);
    let mut long = vec![0.25f32; 16];
    long[3] = f32::NAN;
    long[9] = 0.75;
    assert_eq!(block_stats(&long).peak, 0.75);
}

/// A block measured in pieces reads as the block measured whole, whatever
/// the cut: the peak exactly, the frames exactly, the RMS to rounding (the
/// cut changes which partial sums are added first).
#[test]
fn a_block_in_pieces_reads_as_the_whole_block() {
    let block = sine(0.8, 300);
    let whole = read(&block);
    let (w, mut r) = level_cell();
    for _ in 0..2 {
        for piece in block.chunks(77) {
            fold(&w, piece);
        }
        let l = take(&mut r);
        assert_eq!(l.peak, whole.peak);
        assert_eq!(l.frames, whole.frames);
        assert!(
            (l.rms.get() - whole.rms.get()).abs() < 1e-6,
            "{l:?} against {whole:?}"
        );
    }
}

/// Whole periods of a sine: peak `a`, RMS `a / sqrt(2)`.
#[test]
fn a_known_sine_reads_its_peak_and_rms() {
    let l = read(&sine(0.5, 256));
    assert!((l.peak.get() - 0.5).abs() < 1e-5, "peak {}", l.peak.get());
    assert!(
        (l.rms.get() - 0.5 / 2f32.sqrt()).abs() < 1e-5,
        "rms {}",
        l.rms.get()
    );
}

/// The loud part sits past frame 8192, which the old stereo scratch dropped
/// in a release build, so a meter of the head read silence.
#[test]
fn a_block_past_8192_frames_is_metered_whole() {
    let frames = 8192 + 1024;
    let mut buf = vec![0.0f32; frames];
    buf[8192..].fill(0.5);
    let l = read(&buf);
    assert_eq!(l.peak.get(), 0.5);
    // sqrt(1024 * 0.25 / 9216) = 0.5 / 3
    assert!((l.rms.get() - 0.5 / 3.0).abs() < 1e-6);
}

/// Silence reads zero (over the frames measured).
#[test]
fn silence_reads_zero() {
    let l = read(&[0.0; 512]);
    assert_eq!((l.peak.get(), l.rms.get()), (0.0, 0.0));
    assert_eq!(l.frames, Samples(512));
}

/// Lanes are channels: a signal on one reads on its own lane and nothing
/// leaks into its neighbour.
#[test]
fn a_one_sided_signal_reads_on_its_own_lane() {
    let ((quiet, mut quiet_r), (loud, mut loud_r)) = (level_cell(), level_cell());
    fold(&quiet, &[0.0; 16]);
    fold(&loud, &[-0.75; 16]);
    let (q, l) = (take(&mut quiet_r), take(&mut loud_r));
    assert_eq!((q.peak.get(), q.rms.get()), (0.0, 0.0));
    assert_eq!((l.peak.get(), l.rms.get()), (0.75, 0.75));
}

/// An empty block adds nothing: what was held reads as it was.
#[test]
fn an_empty_block_changes_nothing() {
    let (w, mut r) = level_cell();
    fold(&w, &[0.5, 0.5]);
    fold(&w, &[]);
    assert_eq!(take(&mut r), read(&[0.5, 0.5]));
}

/// A long unread span reads exactly: an hour of 48 kHz in 512-frame blocks,
/// taken once, has the analytic RMS (an `f32` energy sum stalls past 2^24).
#[test]
fn an_hour_unread_reads_the_analytic_rms() {
    let (w, mut r) = level_cell();
    // 0.25 for 512 frames sums to 128 per block, 0.3 to a figure no
    // power-of-two sum hides.
    let block = vec![0.3f32; 512];
    let stats = block_stats(&block);
    let blocks = 337_500u64;
    for _ in 0..blocks {
        w.fold(stats.peak, f64::from(stats.sum_sq), 512);
    }
    let l = take(&mut r);
    let want = (f64::from(stats.sum_sq) / 512.0).sqrt() as f32;
    assert_eq!(l.rms.get(), want);
    assert_eq!(l.frames, Samples(337_500 * 512));
}

/// The switch starts off and both ends see it.
#[test]
fn the_switch_starts_off_and_both_ends_see_it() {
    let (reader, writer) = MeterReader::pair(MeterId(0), 1, 1);
    let on = || writer.shared.enabled.load(Ordering::Relaxed);
    assert!(!reader.is_enabled() && !on());
    reader.enable();
    assert!(on());
    reader.disable();
    assert!(!on());
}

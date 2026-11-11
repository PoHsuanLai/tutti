use super::*;

fn geo(window: usize, hop: usize) -> StftGeometry {
    StftGeometry::new(SampleRate::new(44100.0), Samples(window), Samples(hop)).unwrap()
}

#[test]
fn rejects_a_zero_window_a_zero_hop_and_a_non_positive_rate() {
    assert_eq!(
        StftGeometry::new(SampleRate::new(44100.0), Samples(0), Samples(512)),
        Err(Error::ZeroWindow)
    );
    assert_eq!(
        StftGeometry::new(SampleRate::new(44100.0), Samples(2048), Samples(0)),
        Err(Error::ZeroHop)
    );
    assert_eq!(
        StftGeometry::new(SampleRate::new(0.0), Samples(2048), Samples(512)),
        Err(Error::NonPositiveSampleRate)
    );
}

/// A hop wider than the window is legal for analysis and illegal for
/// reconstruction — decimated display grids sample sparsely on purpose.
#[test]
fn a_hop_wider_than_the_window_analyses_but_does_not_invert() {
    let sparse = StftGeometry::new(SampleRate::new(44100.0), Samples(2048), Samples(8192)).unwrap();
    assert!(!sparse.frames_overlap());

    assert_eq!(
        StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), Samples(8192)),
        Err(Error::HopExceedsWindow {
            window: Samples(2048),
            hop: Samples(8192),
        })
    );
}

/// The bug this split exists to prevent: a decimated hop is accepted for
/// analysis but refused for anything that has to invert.
#[test]
fn cola_refuses_what_new_accepts() {
    // 4x overlap: fine for both.
    assert!(StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), Samples(512)).is_ok());

    // 2x overlap: analysable, not invertible.
    assert!(StftGeometry::new(SampleRate::new(44100.0), Samples(2048), Samples(1024)).is_ok());
    assert_eq!(
        StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), Samples(1024)),
        Err(Error::NotColaCompliant {
            window: Samples(2048),
            hop: Samples(1024),
            window_fn: CosineWindow::HANN,
        })
    );

    // Hop does not divide the window: analysable, not invertible.
    assert!(StftGeometry::new(SampleRate::new(44100.0), Samples(2048), Samples(500)).is_ok());
    assert!(StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), Samples(500)).is_err());
}

/// **A grid that is COLA for one window is not automatically COLA for
/// another**, and this is the case that makes `cola_with` necessary.
///
/// 2048/512 is 4x overlap: exact for Hann, and a 2.1% amplitude ripple for
/// Blackman, which needs 8x. Nothing about the grid changed — only the
/// shape laid over it.
#[test]
fn a_hann_cola_grid_may_not_be_cola_for_another_window() {
    let hop = Samples(512);
    assert!(StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), hop).is_ok());

    assert_eq!(
        StftGeometry::cola_with(
            SampleRate::new(44100.0),
            Samples(2048),
            hop,
            CosineWindow::BLACKMAN
        ),
        Err(Error::NotColaCompliant {
            window: Samples(2048),
            hop,
            window_fn: CosineWindow::BLACKMAN,
        }),
        "Blackman needs 8x overlap; 2048/512 is 4x"
    );

    // And it is accepted at the overlap it actually asks for.
    assert!(
        StftGeometry::cola_with(
            SampleRate::new(44100.0),
            Samples(2048),
            Samples(256),
            CosineWindow::BLACKMAN
        )
        .is_ok()
    );
}

/// The builder does **not** re-check COLA, and the doc says so — this pins
/// that, because a silently-invalidated grid is the hazard `cola_with`
/// exists to give callers a way to avoid.
#[test]
fn the_builder_can_produce_a_non_cola_grid() {
    let hann =
        StftGeometry::cola(SampleRate::new(44100.0), Samples(2048), Samples(512)).expect("hann 4x");
    assert!(hann.is_cola());

    let blackman = hann.with_window_fn(CosineWindow::BLACKMAN);
    assert!(
        !blackman.is_cola(),
        "with_window_fn is infallible and does not re-validate"
    );
}

/// The two window nouns do not collide: one is a length, one is a shape.
#[test]
fn a_grid_carries_both_a_window_length_and_a_window_shape() {
    let g = geo(2048, 512);
    assert_eq!(g.window(), Samples(2048));
    assert_eq!(g.window_fn(), CosineWindow::HANN);
    assert_eq!(g.window_coefficients().len(), 2048);
}

/// **The magnitude gain follows the window shape, not just its length.**
///
/// This is what lets a display divide out the forward transform's window
/// gain without knowing which window produced it.
#[test]
fn magnitude_gain_tracks_the_window_shape() {
    let hann = geo(1024, 256);
    assert!((hann.magnitude_gain() - 512.0).abs() < 1e-3, "0.5 * 1024");

    let blackman = hann.with_window_fn(CosineWindow::BLACKMAN);
    assert!(
        (blackman.magnitude_gain() - 430.08).abs() < 1e-2,
        "0.42 * 1024, got {}",
        blackman.magnitude_gain()
    );

    let rect = hann.with_window_fn(CosineWindow::RECTANGULAR);
    assert!((rect.magnitude_gain() - 1024.0).abs() < 1e-3, "1.0 * 1024");
}

#[test]
fn bin_counts_differ_by_one_and_both_derive_from_the_window() {
    let g = geo(2048, 512);
    assert_eq!(g.freq_bins(), BinCount(1024));
    assert_eq!(g.bins_per_frame(), BinCount(1025));
}

#[test]
fn bin_frequency_spans_dc_to_nyquist() {
    let g = geo(2048, 512);
    assert_eq!(g.bin_frequency(BinIndex(0)), Hz(0.0));
    // Bin `window/2` is Nyquist.
    assert!((g.bin_frequency(BinIndex(1024)).get() - 22050.0).abs() < 1e-3);
    // Resolution is rate / window.
    assert!((g.bin_frequency(BinIndex(1)).get() - 44100.0 / 2048.0).abs() < 1e-3);
}

#[test]
fn frames_and_output_length_round_trip() {
    let g = geo(2048, 512);

    // Shorter than one window yields no frames at all.
    assert_eq!(g.frames_for(Samples(2047)), FrameCount(0));
    assert_eq!(g.output_len(FrameCount(0)), Samples(0));

    assert_eq!(g.frames_for(Samples(2048)), FrameCount(1));
    assert_eq!(g.output_len(FrameCount(1)), Samples(2048));

    // Each additional frame adds exactly one hop.
    assert_eq!(g.frames_for(Samples(2048 + 512)), FrameCount(2));
    assert_eq!(g.output_len(FrameCount(2)), Samples(2048 + 512));
}

#[test]
fn latency_is_one_window() {
    let g = geo(4410, 1102);
    assert!((g.latency().get() - 0.1).abs() < 1e-6);
}

/// The latency is the window over the rate in `f64`, with no `f32` on the
/// way: 2048 / 44100 = 0.046439909297052155 s, where an `f32` round trip
/// reads 0.04643991.
#[test]
fn latency_is_exact() {
    let g = geo(2048, 512);
    assert_eq!(g.latency(), Seconds(2048.0 / 44_100.0));
}

use super::*;

const RATE: f64 = 48_000.0;

fn instant() -> GateKernel {
    GateKernel::new(
        Seconds(0.0),
        Seconds(0.0),
        Seconds(0.0),
        SampleRate::new(RATE),
    )
}

fn range(db: f32) -> f32 {
    Db(db).to_amplitude().get()
}

/// A level on either side of the threshold's linear value opens or closes
/// the gate; closed, the gain is the range.
#[test]
fn the_block_threshold_decides_open_and_closed() {
    let b = GateBlock::new(Db(-20.0), Db(-60.0), Db(-60.0));
    let threshold = range(-20.0);
    let mut k = instant();
    assert_eq!(k.gain(&b, 0, 1, threshold * 1.01, None), 1.0);
    assert!(k.is_open());
    let mut k = instant();
    let closed = k.gain(&b, 0, 1, threshold * 0.99, None);
    assert!(!k.is_open());
    assert!((closed - range(-60.0)).abs() < 1e-9, "{closed}");
}

/// Silence opens the gate only for a threshold at or under `Db::FLOOR`.
#[test]
fn silence_opens_only_at_or_under_the_floor() {
    for (threshold, opens) in [(-20.0f32, false), (Db::FLOOR.get(), true), (-200.0, true)] {
        let b = GateBlock::new(Db(threshold), Db(-60.0), Db(-60.0));
        let mut k = instant();
        let g = k.gain(&b, 0, 1, 0.0, None);
        assert_eq!(g == 1.0, opens, "threshold {threshold}: gain {g}");
    }
}

/// A closed gate's gain follows the range ramp, and the block's last frame
/// is the target range exactly (`ramp_is_at_end`), not a computed value
/// that could round past it.
///
/// Not pinned: the `i + 1 >= n` shortcut itself, since the unshortcut sum rounds to
/// `to` for these figures; the last-frame assertion pins the value, not the branch.
#[test]
fn the_range_ramps_and_ends_exactly_on_the_target() {
    let b = GateBlock::new(Db(-20.0), Db(-80.0), Db(-20.0));
    let mut k = instant();
    let n = 64;
    let first = k.gain(&b, 0, n, 0.0, None);
    let want_first = range(-80.0 + 60.0 / n as f32);
    assert!((first - want_first).abs() < 1e-6, "{first} vs {want_first}");
    let mut last = 0.0;
    for i in 1..n {
        last = k.gain(&b, i, n, 0.0, None);
    }
    assert_eq!(last.to_bits(), range(-20.0).to_bits());
    // A held range is the target at every frame.
    let held = GateBlock::new(Db(-20.0), Db(-40.0), Db(-40.0));
    let mut k = instant();
    for i in 0..n {
        assert_eq!(
            k.gain(&held, i, n, 0.0, None).to_bits(),
            range(-40.0).to_bits()
        );
    }
}

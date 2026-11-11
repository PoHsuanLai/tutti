use super::*;

/// Every reading a control delivers while `run` drives it.
fn delivered(weights: Weights, run: impl FnOnce(&mut Control<'_>)) -> Vec<Progress> {
    let mut seen = Vec::new();
    let mut record = |p: &Progress| seen.push(*p);
    let token = CancelToken::new();
    let mut control = Control::new(&token).on_progress(&mut record);
    control.weigh(weights);
    run(&mut control);
    seen
}

/// Doc 019 section 6.1's tables tile `[0, 1]`: each band starts where the
/// last ended, the first at 0 and the last at exactly 1, so the fraction is
/// monotonic across stages and reaches 1.0 only in the last.
///
/// Mutations (each run, each failed): `Rendering` of `STREAMING` ending at
/// 0.90 -> the bands no longer meet; `Finalizing` of `STREAMING` ending at
/// 0.99 -> the last band no longer reaches 1; `Measuring` of `NORMALIZED`
/// ending at 0.70 -> the bands overlap.
#[test]
fn the_bands_tile_the_unit_interval() {
    for table in [&STREAMING[..], &NORMALIZED[..]] {
        assert_eq!(table.first().unwrap().from, 0.0);
        assert_eq!(table.last().unwrap().to, 1.0);
        assert_eq!(table.last().unwrap().stage, Stage::Finalizing);
        for b in table {
            assert!(b.from < b.to, "{:?} is empty", b.stage);
        }
        for pair in table.windows(2) {
            assert_eq!(
                pair[0].to, pair[1].from,
                "{:?} to {:?}",
                pair[0].stage, pair[1].stage
            );
            assert!(pair[0].stage < pair[1].stage, "stages in order");
        }
    }
}

/// The figures of the doc's tables, as written: a change to a weight is a
/// change to this test, in review.
///
/// Mutation (run): `Measuring` of `NORMALIZED` ending at 0.70 -> fails.
#[test]
fn the_bands_are_the_documented_ones() {
    let streaming = [
        (Stage::Rendering, 0.05, 0.95),
        (Stage::Finalizing, 0.95, 1.00),
    ];
    for (stage, from, to) in streaming {
        assert_eq!(Weights::Streaming.band(stage), Some((from, to)));
    }
    let normalized = [
        (Stage::Rendering, 0.03, 0.55),
        (Stage::Measuring, 0.55, 0.62),
        (Stage::Writing, 0.62, 0.95),
        (Stage::Finalizing, 0.95, 1.00),
    ];
    for (stage, from, to) in normalized {
        assert_eq!(Weights::Normalized.band(stage), Some((from, to)));
    }
    assert_eq!(Weights::Streaming.band(Stage::Measuring), None);
}

/// A stage is delivered when it is entered and when it completes, and while
/// it counts frames no later than every `DELIVERY_FRAMES`, however finely it
/// is advanced, and not every frame.
///
/// Mutations (each run, each failed): `advance` delivering only on
/// completion -> the gap check fails; `advance` delivering on every call ->
/// the "not one per frame" check fails. (`DELIVERY_FRAMES` itself is pinned
/// as the literal 4096 by `tests/cancel.rs`.)
#[test]
fn frames_are_delivered_on_the_cadence_and_at_every_stage_change() {
    let total = Samples(3 * DELIVERY_FRAMES + 100);
    let seen = delivered(Weights::Normalized, |c| {
        c.begin(Stage::Rendering, Some(total));
        for done in 1..=total.get() {
            c.advance(Stage::Rendering, Samples(done), total);
        }
        c.begin(Stage::Measuring, Some(total));
        c.advance(Stage::Measuring, total, total);
    });
    let rendering: Vec<usize> = seen
        .iter()
        .filter(|p| p.stage() == Stage::Rendering)
        .map(|p| p.frames().unwrap().0.get())
        .collect();
    assert_eq!(
        rendering.first(),
        Some(&0),
        "the stage's entry is delivered"
    );
    assert_eq!(rendering.last(), Some(&total.get()), "and its completion");
    assert!(
        rendering.windows(2).all(|w| w[1] - w[0] <= DELIVERY_FRAMES),
        "{rendering:?}"
    );
    assert!(rendering.len() < 10, "not one per frame: {rendering:?}");
    let stages: Vec<Stage> = seen.iter().map(Progress::stage).collect();
    assert_eq!(stages.last(), Some(&Stage::Measuring));
    assert_eq!(
        stages.iter().filter(|s| **s == Stage::Measuring).count(),
        2,
        "the entry and the completion of the second stage"
    );
}

/// The fraction never falls, even when a stage is reported out of order or
/// behind, and a stage's end is its band's end exactly.
///
/// Mutation (run): `deliver` without `.max(floor)` -> the late reading
/// drops back; fails.
#[test]
fn the_fraction_never_falls() {
    let total = Samples(1000);
    let seen = delivered(Weights::Streaming, |c| {
        c.begin(Stage::Rendering, Some(total));
        c.advance(Stage::Rendering, total, total);
        c.at(Stage::Rendering, 0.1);
        c.at(Stage::Finalizing, 0.0);
        c.at(Stage::Finalizing, 1.0);
    });
    assert!(
        seen.windows(2).all(|w| w[0].fraction() <= w[1].fraction()),
        "{seen:?}"
    );
    assert_eq!(seen[0].fraction(), 0.05);
    assert_eq!(
        seen[1].fraction(),
        0.95,
        "the end of rendering is its band's"
    );
    assert_eq!(seen.last().unwrap().fraction(), 1.0);
}

/// A reading's output is the one being measured, written or finalized, and
/// none while rendering.
///
/// Mutation (run): `deliver` giving `Some(0)` for `Rendering` -> fails.
#[test]
fn the_output_is_none_only_while_rendering() {
    let seen = delivered(Weights::Normalized, |c| {
        for stage in [
            Stage::Rendering,
            Stage::Measuring,
            Stage::Writing,
            Stage::Finalizing,
        ] {
            c.begin(stage, None);
        }
    });
    let outputs: Vec<_> = seen.iter().map(Progress::output).collect();
    assert_eq!(outputs, [None, Some(0), Some(0), Some(0)]);
}

/// A token and its clones are one request; a control with no token is never
/// cancelled.
///
/// Mutation (run): `CancelToken::cancel` storing into a fresh flag -> the
/// clone does not see it; fails.
#[test]
fn a_token_is_shared_and_none_never_cancels() {
    let token = CancelToken::new();
    let control = Control::new(&token);
    assert!(!control.is_cancelled());
    token.clone().cancel();
    assert!(token.is_cancelled());
    assert!(control.is_cancelled());
    assert!(!Control::none().is_cancelled());
}

/// **`preparing` reports the session's steps inside the band the render
/// leaves, by the table the outputs render under.** Two of four steps are half
/// of the band: 0.025 of a streaming render's 0 to 0.05, 0.015 of a
/// normalized one's 0 to 0.03; the last step is the end of the band, which the
/// render's first reading does not fall below. Each reading is `Preparing`,
/// names no output and counts no frames.
///
/// Checked to fail by making `Weights::of` answer `Streaming` always: the
/// normalized figures read 0.025 and 0.05.
#[test]
fn preparing_reports_steps_in_the_band_the_render_leaves() {
    use crate::{BufferTarget, FileTarget, Normalize, NormalizeSpec, Output};
    use tutti_codec::{Format, Pcm};
    use tutti_types::ChannelLayout;
    use tutti_types::Db;
    let streaming = [Output::master(BufferTarget::new(ChannelLayout::STEREO))];
    let normalized = [Output::master(
        FileTarget::new("n.wav", Format::Wav(Pcm::Int16), ChannelLayout::STEREO)
            .with_normalize(Some(NormalizeSpec::own(Normalize::peak(Db(-1.0))))),
    )];
    for (outputs, steps, end) in [
        (&streaming, [(2u32, 0.025f32), (4, 0.05)], 0.05f32),
        (&normalized, [(2, 0.015), (4, 0.03)], 0.03),
    ] {
        let mut seen = Vec::new();
        let mut record = |p: &Progress| seen.push(*p);
        let token = CancelToken::new();
        let mut control = Control::new(&token).on_progress(&mut record);
        for (done, _) in steps {
            control.preparing(outputs.iter().map(Output::target), done, 4);
        }
        control.begin(Stage::Rendering, Some(Samples(10)));
        for (p, (_, want)) in seen.iter().zip(steps) {
            assert!((p.fraction() - want).abs() < 1e-6, "{p:?} want {want}");
            assert_eq!(
                (p.stage(), p.output(), p.frames()),
                (Stage::Preparing, None, None)
            );
        }
        let first = seen.last().unwrap();
        assert_eq!(first.stage(), Stage::Rendering);
        assert!(
            first.fraction() >= end,
            "the render starts at the band's end"
        );
    }
}

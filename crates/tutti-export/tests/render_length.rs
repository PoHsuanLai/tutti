//! What a render's length may be: a number of frames, held or streamed, never
//! an empty file reported as success and never a process abort.

mod support;

use crate::support::{render_normalized_to_file, render_to_buffers, render_to_file};
use tutti_export::{
    ConfigError, Error, ExportConfig, FrozenClock, Normalize, RenderConfig, RenderGraph, Setup,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::Const;
use tutti_types::Seconds;
use tutti_types::{ChannelLayout, Db, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);

fn graph() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.5, 0.5]));
    g.pipe_output(id);
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::assemble(editor, executor, Setup::new()).expect("paired")
}

fn config(duration_seconds: f64) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(duration_seconds),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A duration that is not a length is `InvalidDuration` on every entry point,
/// and the file at the path is left alone.
///
/// Mutation (run): `RenderPlan::new` mapping a non-finite or negative duration
/// to 0 frames again (dropping its guard) → `EmptyRender`, or for NaN an
/// empty file → fails.
#[test]
fn a_duration_that_is_not_a_length_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    std::fs::write(&path, b"old").unwrap();
    for seconds in [f64::NAN, f64::INFINITY, -1.0] {
        let cfg = config(seconds);
        let want = |e: Error| matches!(e, Error::InvalidConfig(ConfigError::InvalidDuration));
        assert!(want(
            render_to_buffers(graph(), &cfg, &FrozenClock).unwrap_err()
        ));
        assert!(want(
            render_to_file(graph(), &cfg, &FrozenClock, &path).unwrap_err()
        ));
        assert!(want(
            render_normalized_to_file(
                graph(),
                &cfg,
                &FrozenClock,
                Normalize::peak(Db(-1.0)),
                &path
            )
            .unwrap_err()
        ));
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
}

/// The default config, which has no duration, is `EmptyRender`, not a
/// zero-frame file.
///
/// Mutation (run): drop the `output_length == 0` check in `RenderPlan::new` →
/// `Ok` with an empty buffer → fails.
#[test]
fn a_render_with_no_duration_is_a_config_error() {
    let err = render_to_buffers(graph(), &config(0.0), &FrozenClock).unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig(ConfigError::EmptyRender)),
        "{err:?}"
    );
    let dir = tempfile::tempdir().unwrap();
    let err = render_to_file(
        graph(),
        &config(0.0),
        &FrozenClock,
        dir.path().join("a.wav"),
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::InvalidConfig(ConfigError::EmptyRender)),
        "{err:?}"
    );
    assert!(!dir.path().join("a.wav").exists());
}

/// A render too long to hold in memory is an error, not an abort: this
/// duration asks for more bytes than an address space has, which
/// `Vec::with_capacity` turns into the process's death.
///
/// Mutation (run): `Vec::with_capacity(plan.output_length.get())` back in
/// `render_pass` → the test process aborts → fails.
#[test]
fn a_render_too_long_to_hold_is_an_error_not_an_abort() {
    let err = render_to_buffers(graph(), &config(1.0e12), &FrozenClock).unwrap_err();
    assert!(matches!(err, Error::RenderTooLarge { .. }), "{err:?}");
}

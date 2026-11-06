use super::*;
use crate::{AudioEngine, ManualStreamDriver, OutputSpec};
use tutti_core::{AudioTap, ChannelLayout, Engine, MasterMeter, SampleRate, Samples, Transport};
use tutti_graph::{Editor, Prepare};

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate(rate),
        ChannelLayout::STEREO,
        cpal::SampleFormat::F32,
    )
}

/// **A plain restart refuses a device at another rate, and leaves the
/// stream stopped** — its hook, run through the device-free restart
/// (`restart` itself needs a device to resolve). At the same rate it
/// starts.
///
/// Mutations (run): `refuse_rate_change` answering `Ok` regardless → the
/// 48 kHz restart starts the stream over the 44.1 kHz graph → fails;
/// `rerate_or_restore` adopting the rate (or keeping the new spec) on
/// failure → the second attempt compares 48 kHz with 48 kHz and starts
/// → fails. (Checking against `spec().sample_rate` instead of
/// `graph_rate` is equivalent while a failure restores the spec; it
/// fails here only together with the restore removed.)
#[test]
fn a_plain_restart_refuses_a_new_rate_and_stays_stopped() {
    let transport = Transport::new(44_100.0);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(44_100.0), Samples(512)));
    let engine = Engine::new(&transport, &mut ed, exec).expect("an empty graph");
    let state = Arc::new(AudioCallbackState::new(
        engine,
        MasterMeter::new(),
        AudioTap::new(),
    ));
    let mut driver = TuttiDriver::new(AudioEngine::from_spec(spec_at(44_100.0)), state);
    let graph = SampleRate(44_100.0);

    // Twice onto the same 48 kHz device: the second attempt must not
    // find the first one's resolved spec and call it "no change".
    for attempt in 0..2 {
        let (d, stream) = ManualStreamDriver::new();
        let err = driver
            .restart_on(
                spec_at(48_000.0),
                d,
                refuse_rate_change(driver.graph_rate()),
            )
            .expect_err("a new rate is refused");
        assert!(
            matches!(
                err,
                crate::Error::RateChanged { device, graph: g }
                    if device == SampleRate(48_000.0) && g == graph
            ),
            "attempt {attempt}: {err}"
        );
        assert!(
            !driver.engine().is_running(),
            "nothing plays at the wrong rate"
        );
        assert!(!stream.is_open());
        assert_eq!(driver.graph_rate(), graph, "the graph did not move");
        assert_eq!(
            driver.engine().spec().sample_rate,
            graph,
            "the spec does not claim the refused rate"
        );
    }

    let (d, stream) = ManualStreamDriver::new();
    driver
        .restart_on(spec_at(44_100.0), d, refuse_rate_change(graph))
        .expect("the same rate restarts");
    assert!(driver.engine().is_running() && stream.is_open());
    drop(ed);
}

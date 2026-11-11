//! `Graph::set_outputs_from`: a claim on the master takes every output, and a
//! second claim replaces the first.
//!
//! The session half of `bevy-tutti`'s `composition` `master_bus` module
//! (`a_second_claim_on_the_master_silently_replaces_the_first`,
//! `a_master_claim_takes_every_channel_even_from_a_mono_source` and
//! `the_sequence_this_crate_used_to_produce_lost_the_metronome`, the last the
//! first's sequence with a click and a soundfont for names, so one test here).
//! Nothing warns, nothing logs, and the first node is simply gone from the
//! output: that is the documented behaviour, pinned so it stays the
//! behaviour a host wires `MasterSources`-style declarations around.

use tutti_graph::ForkByClone;
use tutti_nodes::testing::Osc;
use tutti_session::{GraphSource, Session};
use tutti_types::{ChannelLayout, Hz, SampleRate};

fn session() -> Session {
    Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate::DEFAULT)
        .expect("builds")
}

/// Two "this is the master" calls do not sum. The second wins outright, on
/// every channel: the click a build piped to the output is disconnected by
/// the soundfont that finishes loading a few frames later.
#[test]
fn a_second_claim_on_the_master_silently_replaces_the_first() {
    let mut s = session();
    let (first, _) = s.graph().add(ForkByClone(Osc::sine(Hz(440.0))));
    let (second, _) = s.graph().add(ForkByClone(Osc::sine(Hz(880.0))));

    s.graph().set_outputs_from(first);
    assert_eq!(
        s.graph_ref().output_sources(0),
        [GraphSource::Node(first, 0)],
        "the first claim lands"
    );

    s.graph().set_outputs_from(second);
    assert_eq!(
        s.graph_ref().output_sources(0),
        [GraphSource::Node(second, 0)],
        "and the second overwrites it: not a mix"
    );
    assert_eq!(
        s.graph_ref().output_sources(1),
        // A mono `Osc` has one output, so `channel % node_outputs` wraps both
        // global channels onto port 0.
        [GraphSource::Node(second, 0)],
        "on every channel, not just channel 0"
    );
}

/// The claim reaches *all* global channels regardless of the source's width,
/// which is why a mono node claiming the bus silences a stereo one on both
/// sides rather than just the left.
#[test]
fn a_master_claim_takes_every_channel_even_from_a_mono_source() {
    let mut s = session();
    let (stereo, _) = s.graph().add(ForkByClone(
        Osc::sine(Hz(440.0)).with_layout(ChannelLayout::STEREO),
    ));
    let (mono, _) = s.graph().add(ForkByClone(Osc::sine(Hz(880.0))));

    s.graph().set_outputs_from(stereo);
    assert_eq!(
        s.graph_ref().output_sources(1),
        [GraphSource::Node(stereo, 1)]
    );
    s.graph().set_outputs_from(mono);

    // `set_outputs_from` wraps with `channel % node_outputs`, so a 1-output
    // node feeds both channels from its single port.
    assert_eq!(
        s.graph_ref().output_sources(0),
        [GraphSource::Node(mono, 0)]
    );
    assert_eq!(
        s.graph_ref().output_sources(1),
        [GraphSource::Node(mono, 0)]
    );
}

//! **What a steady-state update costs with modulation declared**: the
//! control thread's `Session::update` on an offline session whose
//! declarations and graph do not change between updates.
//!
//! The scene, for each `n`: a chain of `n` `SvfFilterNode`s into the
//! output, one LFO per filter routed into its cutoff (a range on each), and
//! one follower of the first filter routed into the last one's cutoff (so
//! the reconcile's cycle check has an edge to walk). Each size settles (its
//! lanes and the follower placed), then `UPDATES` updates are timed. The
//! `bare` row is the same chain with nothing declared: the floor the rest
//! of the update costs. The `changing` row is the modulated scene with one
//! range edit on the first filter's cutoff before every update (the edit is
//! outside the timing; the update that follows it is timed), so the apply is
//! never skipped: what the memo costs when it never pays.
//!
//! A percentile harness, not criterion: the figure is a mean and a p99 per
//! update, printed. Run it with
//! `cargo bench -p tutti-session --features modulation --bench mod_update`.

use std::time::{Duration, Instant};

use tutti_core::AudioNode;
use tutti_mod::{Follower, Lfo};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_session::{GraphSource, ParamRef, Session};
use tutti_types::{BeatDuration, Depth, Hz, Q, UnitParam};
use tutti_types::{ChannelLayout, SampleRate, Samples};

const UPDATES: usize = 2_000;

fn scene(n: usize, modulated: bool) -> (Session, Vec<AudioNode>) {
    let mut s = Session::builder()
        .build_offline(SampleRate(48_000.0), ChannelLayout::STEREO, Samples(512))
        .expect("builds");
    let mut filters = Vec::with_capacity(n);
    for _ in 0..n {
        let (f, _) = s.graph().add(SvfFilterNode::<f32>::new(
            SvfType::LowPass,
            Hz(1_000.0),
            Q(0.7),
        ));
        if let Some(&prev) = filters.last() {
            s.graph().set_source(f, 0, GraphSource::Node(prev, 0));
        }
        filters.push(f);
    }
    let last = *filters.last().expect("n > 0");
    s.graph().set_output_source(0, GraphSource::Node(last, 0));
    if modulated {
        let mut m = s.mods();
        for (i, &f) in filters.iter().enumerate() {
            let cutoff = ParamRef::unit(f, UnitParam::Cutoff);
            m.range(cutoff, 20.0, 20_000.0);
            let lfo = m.lfo(Lfo::sine().rate(BeatDuration(0.25 + i as f64 / 64.0)));
            m.route(lfo, cutoff, Depth(0.1));
        }
        let follower = m.follower(Follower::new(), filters[0]);
        m.route(
            follower,
            ParamRef::unit(last, UnitParam::Cutoff),
            Depth(-0.2),
        );
    }
    for _ in 0..8 {
        s.update();
    }
    if modulated {
        assert_eq!(s.mod_state().refusal(), None, "the declarations apply");
        assert!(
            s.mod_state().unresolved_targets().is_empty(),
            "every target resolved"
        );
    }
    (s, filters)
}

fn measure(label: &str, n: usize, modulated: bool, edit: bool) {
    let (mut s, filters) = scene(n, modulated);
    let commits = s.graph().commits_sent();
    let mut times = Vec::with_capacity(UPDATES);
    for i in 0..UPDATES {
        if edit {
            let cutoff = ParamRef::unit(filters[0], UnitParam::Cutoff);
            s.mods().range(cutoff, 20.0, 20_000.0 + (i % 2) as f32);
        }
        let t = Instant::now();
        let events = s.update();
        times.push(t.elapsed());
        drop(events);
    }
    assert_eq!(
        s.graph().commits_sent(),
        commits,
        "a steady state commits nothing"
    );
    times.sort();
    let mean = times.iter().sum::<Duration>() / UPDATES as u32;
    let p99 = times[UPDATES * 99 / 100];
    println!(
        "{label:>10} n = {n:>4}: mean {:>9.2} us, p99 {:>9.2} us",
        mean.as_secs_f64() * 1e6,
        p99.as_secs_f64() * 1e6,
    );
}

fn main() {
    for n in [16, 64, 256] {
        measure("bare", n, false, false);
        measure("modulated", n, true, false);
        measure("changing", n, true, true);
    }
}

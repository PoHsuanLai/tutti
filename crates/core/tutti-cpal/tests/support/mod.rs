//! Shared fixtures for this crate's integration tests.
//!
//! The rolling-graph fixture now exists in three places — `output.rs`'s
//! `build_callback_state`, `tests/rt_no_alloc.rs`'s `rolling_state`, and
//! whatever the next test binary needs. Two copies were already acknowledged
//! in `rt_no_alloc.rs`'s header ("duplicated rather than shared because that
//! one is `#[cfg(test)]`-private"); three is the point at which the repo's own
//! escalation applies.
//!
//! A support module, not a dev-dependency crate: `tutti-fixture-resolve`
//! exists because *three separate crates* each carried a copy of the same
//! probe-path logic. Three copies inside one crate is a `tests/support/`.
//!
//! `rt_no_alloc.rs` still keeps its own copy, deliberately — it declares a
//! `#[global_allocator]`, and every line it runs before the gate has to be
//! auditable in one file.

#![allow(dead_code)]
// Each integration-test binary compiles this tree separately, and no single
// binary uses every item. Same reason, and same allow, as the plugin hosts'
// `tests/support/mod.rs`.

use parking_lot::Mutex;
use std::sync::Arc;
use tutti_core::dsp::Net;
use tutti_core::{AudioTap, ChannelLayout, Engine, Hz, MasterMeter, SampleRate, Q};
use tutti_core::{MotionEvent, Transport, TransportClock};
use tutti_cpal::{AudioCallbackState, OutputSpec};
use tutti_nodes::testing::{Const, Osc};
use tutti_nodes::{SvfFilterNode, SvfType};

pub const SAMPLE_RATE: f64 = 48_000.0;

/// Leak a net so its backend stays valid.
///
/// The backend borrows through the net, so the net must outlive it. Leaked
/// deliberately: a test process is the whole lifetime, and it keeps the
/// fixture free of a self-referential handle. Same reasoning, verbatim, as
/// `tests/rt_no_alloc.rs`.
fn keep(net: Net) {
    let _: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));
}

/// A rolling transport driving a sine through a filter, at `outputs` width.
///
/// The graph has to actually render: a `Net` with nothing wired leaves every
/// output edge on `Port::Zero`, so the callback folds silence without walking
/// a vertex, and any assertion over it is vacuous.
pub fn rolling_state(outputs: usize) -> (Transport, Arc<AudioCallbackState>) {
    let transport = Transport::new(SAMPLE_RATE);

    let mut net = Net::new(0, outputs);
    net.push(Box::new(TransportClock::new(
        transport.clock_links(),
        SAMPLE_RATE,
    )));
    let source = net.push(Box::new(Osc::sine(Hz(220.0))));
    let filter = net.push(Box::new(SvfFilterNode::<f64>::new(
        SvfType::LowPass,
        Hz(2_000.0),
        Q(0.7),
    )));
    net.connect(source, 0, filter, 0);
    net.pipe_output(filter);

    let backend = net.backend();
    keep(net);

    let engine = Engine::new(transport.motion.clone(), backend);
    let state = AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new());

    transport.settings.set_tempo(120.0);
    let _ = transport.motion.try_send(MotionEvent::Play);
    transport.motion.drain();

    (transport, Arc::new(state))
}

/// A graph emitting a constant `level` on **every** output channel.
///
/// DC rather than a tone, for the reason `tutti-export`'s `dc_net` gives: every
/// frame carries the same known value, so an expected result can be computed
/// in closed form instead of sampled. That is what makes the sample-format
/// matrix assertable.
pub fn dc_state(level: f32, outputs: usize) -> Arc<AudioCallbackState> {
    let mut net = Net::new(0, outputs);
    let node = net.push(Box::new(Const::mono(level)));
    for ch in 0..outputs {
        net.pipe_output(node);
        let _ = ch;
    }
    let backend = net.backend();
    keep(net);

    Arc::new(AudioCallbackState::new(
        Engine::new(
            tutti_core::MotionFsm::new(tutti_core::TransportSettings::new()),
            backend,
        ),
        MasterMeter::new(),
        AudioTap::new(),
    ))
}

/// A graph emitting `level` on exactly one output channel, silence on the
/// rest, with the master tap already open.
///
/// This is what distinguishes a *fold* from a truncation: put the signal on a
/// channel outside the front pair and a `[..2]` metering shortcut sees
/// silence while a real fold does not. A uniform-DC graph cannot tell the two
/// apart, because every channel carries the same value.
pub fn dc_on_channel(
    level: f32,
    channel: usize,
    outputs: usize,
) -> (Arc<AudioCallbackState>, tutti_core::TapCons) {
    let mut net = Net::new(0, outputs);
    let node = net.push(Box::new(Const::mono(level)));
    net.connect_output(node, 0, channel);
    let backend = net.backend();
    keep(net);

    let tap = AudioTap::new();
    let cons = tap.open().expect("a fresh tap opens");
    let state = AudioCallbackState::new(
        Engine::new(
            tutti_core::MotionFsm::new(tutti_core::TransportSettings::new()),
            backend,
        ),
        MasterMeter::new(),
        tap,
    );
    (Arc::new(state), cons)
}

/// A spec with no device behind it, at `channels` width and `format`.
pub fn spec(channels: usize, format: cpal::SampleFormat) -> OutputSpec {
    OutputSpec::new(
        SampleRate(SAMPLE_RATE),
        ChannelLayout::from(channels),
        format,
    )
}

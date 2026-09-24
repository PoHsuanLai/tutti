//! The **whole** output callback, not just the graph render.
//!
//! `tutti-nodes`' `engine_render` bench measures `Engine::process`. That is
//! not what a sound card calls. The real callback also clamps to
//! `MAX_FRAMES`, zero-fills the mix, folds the device buffer to stereo, calls
//! `meter_output`, and converts to the device's sample format — and until
//! [`OutputBlock`] existed none of that was reachable outside a live stream,
//! so none of it had ever been measured.
//!
//! **The case that justifies this file is `callback/vs_render`.** Its delta
//! against `engine_render` is the metering-and-conversion tax the engine
//! bench cannot see. If that tax is a large fraction of the block budget,
//! optimising DSP is the wrong place to look.
//!
//! Read the numbers the way `engine_render`'s header describes: elem/s ÷
//! 48 000 is the realtime multiple, and at 64 frames the budget is 1.333 ms.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use parking_lot::Mutex;
use tutti_core::dsp::Net;
use tutti_core::{AudioTap, AudioUnit, ChannelLayout, Engine, Hz, InterleavedMut};
use tutti_core::{MasterMeter, MotionFsm, SampleRate, TransportSettings};
use tutti_cpal::{process_audio, AudioCallbackState, OutputBlock};
use tutti_nodes::testing::Osc;

const SR: f64 = 48_000.0;

fn state(outputs: usize, tap_open: bool) -> (Arc<AudioCallbackState>, Option<tutti_core::TapCons>) {
    let mut net = Net::new(0, outputs);
    let src = net.push(Box::new(Osc::sine(Hz(440.0))));
    net.pipe_output(src);
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend borrows through the net; built once per case in setup, so
    // the leak is bounded by case count rather than iteration count.
    let _: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

    let tap = AudioTap::new();
    let cons = tap_open.then(|| tap.open().expect("a fresh tap opens"));
    let engine = Engine::new(MotionFsm::new(TransportSettings::new()), backend);
    (
        Arc::new(AudioCallbackState::new(engine, MasterMeter::new(), tap)),
        cons,
    )
}

/// The full callback across block sizes and device widths.
fn bench_callback(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback");
    for outputs in [2usize, 6] {
        let (st, _cons) = state(outputs, false);
        let mut block = OutputBlock::new(st, ChannelLayout::from(outputs));
        for frames in [64usize, 256, 512] {
            let mut data = vec![0.0f32; frames * outputs];
            group.throughput(Throughput::Elements(frames as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{outputs}ch-f32"), frames),
                &frames,
                |b, _| b.iter(|| block.render(black_box(&mut data))),
            );
        }
    }
    group.finish();
}

/// **The payoff case.** `process_audio` alone against the full `render`, at
/// the same width and block size. The difference is the fold, the metering
/// and the format conversion — everything a graph-only benchmark omits.
fn bench_vs_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback/vs_render");
    const FRAMES: usize = 256;

    let (st, _c1) = state(2, false);
    let mut mix = vec![0.0f32; FRAMES * 2];
    group.throughput(Throughput::Elements(FRAMES as u64));
    group.bench_function("process_audio_only", |b| {
        b.iter(|| {
            process_audio(
                &st,
                &mut InterleavedMut::new(black_box(&mut mix), ChannelLayout::STEREO),
            )
        });
    });

    let (st2, _c2) = state(2, false);
    let mut block = OutputBlock::new(st2, ChannelLayout::STEREO);
    let mut data = vec![0.0f32; FRAMES * 2];
    group.bench_function("full_callback_f32", |b| {
        b.iter(|| block.render(black_box(&mut data)));
    });

    // The conversion half, isolated: same work, an integer destination.
    let (st3, _c3) = state(2, false);
    let mut block16 = OutputBlock::new(st3, ChannelLayout::STEREO);
    let mut data16 = vec![0i16; FRAMES * 2];
    group.bench_function("full_callback_i16", |b| {
        b.iter(|| block16.render(black_box(&mut data16)));
    });

    group.finish();
}

/// What an open tap costs the audio thread.
///
/// `AudioTap` is opt-in precisely so a closed one is nearly free — the claim
/// is "one atomic load". This prices it, and prices the push when it is open,
/// which is what a host recording the master bus pays.
fn bench_tap(c: &mut Criterion) {
    let mut group = c.benchmark_group("callback/tap");
    const FRAMES: usize = 256;
    group.throughput(Throughput::Elements(FRAMES as u64));

    for (label, open) in [("closed", false), ("open", true)] {
        let (st, cons) = state(2, open);
        let mut block = OutputBlock::new(st, ChannelLayout::STEREO);
        let mut data = vec![0.0f32; FRAMES * 2];
        group.bench_function(label, |b| {
            b.iter(|| block.render(black_box(&mut data)));
        });
        // Held so an open tap stays open for the whole measurement; a dropped
        // consumer would close it and quietly measure the `closed` case twice.
        drop(cons);
    }
    group.finish();
}

criterion_group!(benches, bench_callback, bench_vs_render, bench_tap);
criterion_main!(benches);

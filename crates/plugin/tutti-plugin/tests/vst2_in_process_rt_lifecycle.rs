//! What the in-process VST2 node dispatches from the audio thread.
//!
//! `Node::reset` and `Node::prepare` may run while the plugin is rendering
//! (a graph resets and re-prepares a node between blocks, on another thread
//! from the one that renders it), so neither may reach
//! `Vst2Instance::set_sample_rate`: that brackets `effSetSampleRate`(10) in
//! `effMainsChanged`(12) — the opcode plugins allocate and free their
//! rate-dependent buffers in. Routing there dispatches two main-thread-only
//! opcodes, plus `effStopProcess`(71) / `effStartProcess`(72), racing the
//! render.
//!
//! Asserting a *negative* — that no opcode was dispatched — needs the plugin's
//! own view, not the host's. The reference probe counts `effMainsChanged`
//! separately per direction (`resume_count` / `suspend_count`) and records the
//! last `effSetSampleRate` it saw, so what crossed the AEffect seam is directly
//! readable. Every test below reads those counters rather than any host-side
//! flag, and each pairs its negative with a positive that moves the same
//! counter — an assertion that a number stayed at zero passes just as well
//! against a probe that was never loaded.
//!
//! The last two tests read the render itself: what the node writes is what the
//! plugin wrote, and silence past the channels it hands the plugin.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_core::SampleRate;
use tutti_plugin::handles::PluginHandle;
use tutti_plugin::{in_process_vst2_client, InProcessVst2Client};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// Serializes the whole load→drive→read sequence against the probe's one
/// process-global capture, and against other tests loading the same image.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Re-open the image the host loaded and call one of the probe's exports.
///
/// Must go through `dlopen`: the linked rlib is a separate image with separate
/// statics, and only the cdylib's globals see the host's calls. The rlib is
/// still linked, for the shared `#[repr(C)]` type definitions.
fn probe_call<F, R>(path: &PathBuf, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    let r = f(sym);

    // Leak the handle, deliberately. The switches these helpers touch are
    // `static`s inside the probe's image, and they only survive while that
    // image stays mapped — dropping `lib` decrements the refcount that keeps it
    // mapped. With no `Vst2Instance` holding the probe open at that moment, the
    // write is discarded with the unload and the next load maps a fresh image
    // reading the default.
    //
    // Measured on this bug in `vst2_latency.rs`: 2 of 6 runs failed without
    // this, 0 of 6 with it. It reads as flakiness because it passes whenever
    // another test's instance happens to keep the image resident.
    std::mem::forget(lib);
    r
}

fn reset_probe(path: &PathBuf) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

/// The graph node plus the handle that keeps it alive, loaded against the
/// reference probe with its capture zeroed.
struct Loaded {
    unit: InProcessVst2Client,
    handle: PluginHandle,
    path: PathBuf,
}

fn load() -> Loaded {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    let (unit, handle) = in_process_vst2_client(&path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process load of reference plugin at {path:?}: {e:?}"));
    Loaded { unit, handle, path }
}

/// `unit` prepared at `rate`, for blocks of up to [`BLOCK`].
fn prepare_at(unit: &mut InProcessVst2Client, rate: SampleRate) {
    tutti_graph::Node::prepare(
        unit,
        &tutti_graph::Prepare::new(rate, tutti_core::Samples(BLOCK)),
    );
}

/// Render one block through the node, prepared as a graph prepares it
/// (`Node::prepare`, which sizes its scratch and parks its rate), then
/// `Node::process` by hand (`tutti_graph::contract::drive_in`).
fn drive_block(unit: &mut InProcessVst2Client) {
    drive_blocks(unit, BLOCK, BLOCK);
}

/// One block of `frames` through `unit` prepared for blocks of up to `max`.
fn drive_blocks(unit: &mut InProcessVst2Client, max: usize, frames: usize) {
    use tutti_graph::{Env, Node, Prepare, Transport, TransportChanges};
    let rate = SampleRate(SAMPLE_RATE);
    Node::prepare(unit, &Prepare::new(rate, tutti_core::Samples(max)));
    let silence = vec![0.0f32; frames];
    let inputs: Vec<&[f32]> = (0..Node::shape(unit).audio_in.count())
        .map(|_| &silence[..])
        .collect();
    let env = Env {
        frame: tutti_core::Frame(0),
        sample_rate: rate,
        block_len: tutti_core::Samples(frames),
        transport: Transport::default(),
        changes: TransportChanges::NONE,
        play: tutti_graph::PlayClock::ZERO,
    };
    let _ = tutti_graph::contract::drive_in(unit, &env, &inputs, &[], &[]);
}

/// `Node::reset` must dispatch nothing.
///
/// It ran `Vst2Instance::set_sample_rate`, so pre-fix this saw the full
/// `effStopProcess` → `effMainsChanged(0)` → `effSetSampleRate` →
/// `effMainsChanged(1)` → `effStartProcess` cycle: `suspend_count` and
/// `resume_count` each up by one, from the audio thread, with an allocation
/// inside each `effMainsChanged`.
///
/// The two counters are read *before* as well as after, so this asserts they
/// did not move rather than that they hold some absolute value — `load` itself
/// resumes once, and hard-coding that constant would make the test a statement
/// about the loader instead of about `reset`.
#[test]
fn reset_dispatches_no_opcode_to_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(
        before.valid,
        "the probe observed no render — a reset that dispatches nothing is \
         trivially true against a plugin that was never driven"
    );

    tutti_graph::Node::reset(&mut loaded.unit);

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "reset dispatched effMainsChanged(0) from the audio thread"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "reset dispatched effMainsChanged(1) from the audio thread"
    );

    drop(loaded.handle);
}

/// Blocks of any length up to the prepared maximum reach the plugin, and a
/// re-prepare for a longer maximum dispatches nothing to it.
///
/// The node sizes its scratch in `prepare`, to the graph's `MaxBlock`, and
/// never grows it on the audio thread.
///
/// Mutation (run): `ensure_scratch_size` not resizing the render scratch
/// (`RenderScratch` left at its load-time size) → the 1 000-frame block
/// renders past it → fails.
#[test]
fn blocks_past_64_frames_reach_the_plugin_after_a_prepare() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(before.valid, "the probe observed no render");

    drive_blocks(&mut loaded.unit, 1_024, 1_000);
    drive_blocks(&mut loaded.unit, 1_024, 1);
    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "a re-prepare dispatched effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "a re-prepare dispatched effMainsChanged(1)"
    );

    drop(loaded.handle);
}

/// `Node::prepare` at a new rate (a device change re-prepares a running
/// graph) must park the rate rather than dispatch it.
///
/// The rate is *also* asserted not to have reached the plugin, not only the
/// mains counters: a host that skipped the bracket but still dispatched
/// `effSetSampleRate` would leave the counters clean while reconfiguring a
/// running plugin, which is the same class of bug one opcode narrower.
#[test]
fn set_sample_rate_parks_the_rate_instead_of_dispatching_it() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert!(before.valid, "the probe observed no render");
    // Load told the plugin 48k; the new rate must be distinguishable from it.
    assert_eq!(
        before.sample_rate, SAMPLE_RATE as f32,
        "load must have told the plugin the rate it was loaded at"
    );

    prepare_at(&mut loaded.unit, SampleRate(96_000.0));

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.suspend_count, before.suspend_count,
        "prepare dispatched effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "prepare dispatched effMainsChanged(1)"
    );
    assert_eq!(
        after.sample_rate, SAMPLE_RATE as f32,
        "prepare dispatched effSetSampleRate"
    );

    drop(loaded.handle);
}

/// The parked rate is not dropped: the main-thread drain delivers it.
///
/// This is the positive half of the test above. Without it, "nothing was
/// dispatched" is satisfied by a node that discards the rate entirely, which
/// would be the worse bug — the plugin keeps rendering at its previous rate
/// forever, with no diagnostic anywhere.
#[test]
fn the_parked_rate_reaches_the_plugin_on_the_main_thread_drain() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let before = read_capture(&loaded.path);
    assert_eq!(before.sample_rate, SAMPLE_RATE as f32);

    prepare_at(&mut loaded.unit, SampleRate(96_000.0));
    // The drain point: `editor_idle` is the per-frame main-thread call, and it
    // is reached through the handle rather than the node — the two ends of the
    // deferral are deliberately on opposite sides of the audio/control split.
    loaded.handle.editor_idle();

    let after = read_capture(&loaded.path);
    assert_eq!(
        after.sample_rate, 96_000.0,
        "the rate parked by prepare never reached the plugin"
    );
    // And it arrived through the bracket, which is what makes it safe to
    // deliver at all: one suspend and one resume, on the main thread.
    assert_eq!(
        after.suspend_count,
        before.suspend_count + 1,
        "the deferred rate change must be bracketed in effMainsChanged(0)"
    );
    assert_eq!(
        after.resume_count,
        before.resume_count + 1,
        "the deferred rate change must be bracketed in effMainsChanged(1)"
    );

    drop(loaded.handle);
}

/// A second drain with nothing parked must dispatch nothing.
///
/// `effMainsChanged` is not documented as idempotent and the probe counts each
/// direction, so a drain that fired unconditionally would suspend and resume a
/// running plugin on every editor frame.
#[test]
fn a_drain_with_nothing_parked_dispatches_nothing() {
    let _guard = lock_probe();
    let loaded = load();

    loaded.handle.editor_idle();
    let before = read_capture(&loaded.path);
    loaded.handle.editor_idle();
    let after = read_capture(&loaded.path);

    assert_eq!(
        after.suspend_count, before.suspend_count,
        "an empty drain suspended the plugin"
    );
    assert_eq!(
        after.resume_count, before.resume_count,
        "an empty drain resumed the plugin"
    );

    drop(loaded.handle);
}

/// What the node writes is what the plugin wrote, channel by channel and
/// frame by frame: the graph's input reaches the plugin unshifted, and each
/// output channel is the plugin's own (the probe's oracle, `in + tag(ch)`).
///
/// Two blocks with different inputs, the second shorter than the first, so
/// a channel table left pointing at an earlier block's staging, an offset
/// slice, or a swapped channel each shows as a wrong sample. Every value is
/// a small integer, so the comparison is exact on every platform.
///
/// Mutations (run): the input table built from channel 0 for every channel
/// (`inputs.get(0)`) → channel 1 reads channel 0's ramp → fails; the input
/// slice offset by one frame (`&inputs.get(ch)[1..size]`) → fails; no
/// output table handed to the plugin (`outs.clear()` before `drive_f32`) →
/// the outputs stay silent → fails; the output table reversed
/// (`outs.reverse()`) → channel 0 holds channel 1's tag → fails.
#[test]
fn the_output_is_what_the_plugin_wrote() {
    use tutti_graph::{Env, Node, Prepare, Transport, TransportChanges};
    use tutti_vst2_test_plugin::channel_tag;

    let _guard = lock_probe();
    let mut loaded = load();
    let rate = SampleRate(SAMPLE_RATE);
    Node::prepare(
        &mut loaded.unit,
        &Prepare::new(rate, tutti_core::Samples(BLOCK)),
    );
    let channels = usize::from(Node::shape(&loaded.unit).audio_in.count());
    assert_eq!(channels, 2, "the probe's default layout is stereo");

    for (block, frames) in [(0usize, BLOCK), (1, BLOCK - 17)] {
        // Channel `c`, frame `i`: `1000·c + 10·i + block`, distinct across
        // channels, frames and blocks.
        let input: Vec<Vec<f32>> = (0..channels)
            .map(|c| {
                (0..frames)
                    .map(|i| (1_000 * c + 10 * i + block) as f32)
                    .collect()
            })
            .collect();
        let inputs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let env = Env {
            frame: tutti_core::Frame(0),
            sample_rate: rate,
            block_len: tutti_core::Samples(frames),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        let out = tutti_graph::contract::drive_in(&mut loaded.unit, &env, &inputs, &[], &[]);
        assert_eq!(out.audio.len(), channels);
        for (c, ch) in out.audio.iter().enumerate() {
            let want: Vec<f32> = input[c].iter().map(|s| s + channel_tag(c)).collect();
            assert_eq!(ch, &want, "block {block}, channel {c}");
        }
    }

    drop(loaded.handle);
}

/// A node of `WIDE` channels in and out writing `value` to every output,
/// whatever its input: what leaves a non-zero buffer behind for a later node
/// to be handed.
struct Fill {
    width: tutti_core::ChannelLayout,
    inputs: tutti_core::ChannelLayout,
    value: f32,
}

impl tutti_graph::Node for Fill {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(self.inputs, self.width)
    }
    fn prepare(&mut self, _: &tutti_graph::Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        for c in 0..io.output_count() {
            io.output(c).fill(self.value);
        }
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// A plugin wider than the node's channel tables (sixteen) has the outputs
/// past them silenced, not left holding whatever the graph's buffer held.
///
/// The probe is loaded with eighteen inputs and outputs, behind two fill
/// nodes (the first writes a stale value, the second, fed by it, the
/// plugin's input), so the slots the first one wrote are free by the time
/// the plugin's outputs are placed, and an output the node leaves unwritten
/// shows what the first one left there. Channels within the tables are the
/// plugin's (`in + tag(ch)`); the two past them are zero.
///
/// Mutation (run): the silencing loop deleted → channels 16 and 17 hold
/// the stale value → fails.
#[test]
fn outputs_past_the_tables_are_silent() {
    use tutti_core::{ChannelLayout, Samples};
    use tutti_graph::{GraphBuilder, Prepare, Transport, Unforkable};
    use tutti_vst2_test_plugin::channel_tag;

    const WIDE: usize = 18;
    const TABLE: usize = 16;
    const STALE: f32 = -7.0;
    const INPUT: f32 = 3.0;

    let _guard = lock_probe();
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    // SAFETY: `PROBE_LOCK` is held, so no other test thread touches the
    // environment; the variables are cleared once the AEffect is built.
    unsafe {
        std::env::set_var("TUTTI_VST2_PROBE_INPUTS", WIDE.to_string());
        std::env::set_var("TUTTI_VST2_PROBE_OUTPUTS", WIDE.to_string());
    }
    let loaded = in_process_vst2_client(&path, SAMPLE_RATE);
    unsafe {
        std::env::remove_var("TUTTI_VST2_PROBE_INPUTS");
        std::env::remove_var("TUTTI_VST2_PROBE_OUTPUTS");
    }
    let (unit, handle) = loaded.unwrap_or_else(|e| panic!("in-process load: {e:?}"));

    let wide = ChannelLayout::from_count(WIDE as u16);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, wide);
    let stale = g.add(Unforkable(Fill {
        width: wide,
        inputs: ChannelLayout::EMPTY,
        value: STALE,
    }));
    let feed = g.add(Unforkable(Fill {
        width: wide,
        inputs: wide,
        value: INPUT,
    }));
    let key = g.add(Unforkable(unit));
    g.pipe(stale, feed);
    g.pipe(feed, key);
    g.pipe_output(key);
    let mut r = g
        .renderer(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)))
        .expect("the graph compiles");

    for block in 0..3usize {
        let mut out = vec![vec![0.0f32; BLOCK]; WIDE];
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        r.executor_mut()
            .process(BLOCK, &Transport::default(), &[], &mut outs);
        for (c, ch) in out.iter().enumerate() {
            let want = if c < TABLE {
                INPUT + channel_tag(c)
            } else {
                0.0
            };
            assert!(
                ch.iter().all(|&s| s == want),
                "block {block}, channel {c}: want {want}, got {:?}",
                &ch[..4]
            );
        }
    }

    drop(r);
    drop(handle);
}

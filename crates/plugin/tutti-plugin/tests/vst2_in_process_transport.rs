//! **The live in-process VST2 node is told the transport its input was
//! rendered at**, read back out of the reference probe
//! (`tutti-vst2-test-plugin`), which records what `audioMasterGetTime`
//! answered on its last `process` call.
//!
//! The node reads `Cx::transport_at` at its block's first frame, as the
//! out-of-process plugin node and the VST2 fork do: behind a latent path
//! (a silent source declaring 40 frames of latency feeds both its inputs, as
//! in `clap_transport.rs`), timeline frame `F − 40`, through the executor's
//! transport history; with no latency, the block's own first frame. The
//! scenes cycle through a plain roll, a seek at the block boundary, a tempo
//! change inside the block, a loop that wraps before the frame the next
//! block hears, recording, and a stop.

#![cfg(feature = "vst2")]

use std::sync::{Arc, Mutex, MutexGuard};

use tutti_core::{
    Beat, BeatsPerBar, Bpm, ChannelLayout, Frame, MeterChange, MeterMap, NoteValue, RtPublish,
    SampleRate, Samples, TimeSignature,
};
use tutti_graph::{
    Cx, Env, GraphBuilder, IntoNode, Io, LoopRange, Node, Offset, Prepare, Renderer, Shape, Status,
    Transport, TransportChanges, Unforkable,
};
use tutti_plugin::handles::PluginHandle;
use tutti_plugin::{in_process_vst2_client, InProcessVst2Client};
use tutti_types::Latency;
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;
/// The latent path's latency: the node's compiled arrival.
const ARRIVAL: usize = 40;

/// `kVstTransportPlaying`.
const PLAYING: i32 = 1 << 1;
/// `kVstTransportCycleActive`.
const CYCLE_ACTIVE: i32 = 1 << 2;
/// `kVstTransportRecording`.
const RECORDING: i32 = 1 << 3;

/// Serializes against the probe's process-global capture.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Call one of the probe's exports on the image the host loaded, leaking the
/// handle (see `vst2_in_process_fork.rs`'s `probe_call`).
fn probe_call<R>(symbol: &[u8], f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
    let path = probe_path::probe_path();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> = unsafe { lib.get(symbol) }
        .unwrap_or_else(|e| panic!("probe missing {}: {e}", String::from_utf8_lossy(symbol)));
    let r = f(*sym);
    std::mem::forget(lib);
    r
}

fn reset_probe() {
    for symbol in [
        &b"tutti_vst2_probe_reset_capture\0"[..],
        b"tutti_vst2_probe_reset_switches\0",
    ] {
        probe_call(symbol, |sym| {
            // SAFETY: both are `extern "C" fn()`.
            let f: extern "C" fn() = unsafe { std::mem::transmute(sym) };
            f();
        });
    }
}

fn read_capture() -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(b"tutti_vst2_probe_capture\0", |sym| {
        // SAFETY: the probe exports this as
        // `unsafe extern "C" fn(*mut ProcessCapture) -> bool`.
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

fn load() -> (InProcessVst2Client, PluginHandle) {
    let path = probe_path::probe_path();
    in_process_vst2_client(path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process load of the probe at {path:?}: {e:?}"))
}

/// A silent source declaring `latency`: what puts the plugin behind a latent
/// path.
struct Latent {
    latency: Latency,
    width: ChannelLayout,
}

impl Node for Latent {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.width).with_latency(self.latency)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..io.output_count() {
            io.output(c).fill(0.0);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// The live node fed by a silent source of `arrival` frames' latency on both
/// its inputs, its outputs global.
fn rig<N: IntoNode>(node: N, arrival: usize) -> (Renderer, N::Controls) {
    let stereo = ChannelLayout::from_count(2);
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, stereo);
    let latent = g.add(Unforkable(Latent {
        latency: Latency::new(Samples(arrival)),
        width: stereo,
    }));
    let (key, controls) = g.add_with_controls(node);
    for port in 0..2 {
        g.connect(latent, port, key, port);
    }
    g.pipe_output(key);
    let r = g
        .renderer(Prepare::new(SampleRate(SAMPLE_RATE), Samples(BLOCK)))
        .expect("the graph compiles");
    (r, controls)
}

/// One scene per block: the transport at its first frame, and the changes
/// inside it.
fn scene(b: usize) -> (Transport, TransportChanges) {
    let base = 4.0 + b as f64;
    let mut changes = TransportChanges::NONE;
    let rolling = Transport::new(true, Bpm(120.0), Beat(base), None);
    let t = match b % 6 {
        0 => rolling,
        // A seek at the block boundary, far from where the last block left
        // off.
        1 => Transport::new(true, Bpm(120.0), Beat(64.0 + base), None),
        // A tempo change inside the block, after the frame the next block
        // hears (frame 24 of this one) and before it.
        2 => {
            let at = Offset::new(10, Samples(BLOCK)).expect("inside");
            changes
                .push(at, Transport::new(true, Bpm(90.0), Beat(base + 0.5), None))
                .expect("a change inside the block");
            rolling
        }
        // A loop 16 frames long from here: it wraps at frame 16, before
        // frame 24, which the next block's first frame hears.
        3 => Transport::new(
            true,
            Bpm(120.0),
            Beat(base),
            Some(LoopRange {
                start: Beat(base),
                end: Beat(base + 16.0 / 24_000.0),
            }),
        ),
        4 => rolling.with_recording(true),
        _ => Transport::new(false, Bpm(100.0), Beat(base), None),
    };
    (t, changes)
}

/// What the plugin was told on one block.
#[derive(Debug)]
struct Told {
    ppq: f64,
    tempo: f64,
    playing: bool,
    recording: bool,
    cycling: bool,
    numerator: i32,
}

impl Told {
    fn of(cap: &ProcessCapture) -> Self {
        assert!(cap.time_info_present, "the plugin was answered");
        Self {
            ppq: cap.time_ppq_pos,
            tempo: cap.time_tempo,
            playing: cap.time_flags & PLAYING != 0,
            recording: cap.time_flags & RECORDING != 0,
            cycling: cap.time_flags & CYCLE_ACTIVE != 0,
            numerator: cap.time_sig_numerator,
        }
    }

    /// Whether this is `t` (the beat to well under a frame).
    fn is(&self, t: &Transport) -> bool {
        (self.ppq - t.beat().get()).abs() < 1e-9
            && self.tempo == t.tempo.get()
            && self.playing == t.playing
            && self.recording == t.recording()
            && self.cycling == t.looping.is_some()
    }
}

/// Render `blocks` scenes through `r`, one block each; return each block's
/// `Env` and what the plugin was told on it.
fn render_scenes(r: &mut Renderer, blocks: usize) -> Vec<(Env, Told)> {
    let mut out = vec![vec![0.0f32; BLOCK]; 2];
    let mut seen = Vec::with_capacity(blocks);
    for b in 0..blocks {
        let (transport, changes) = scene(b);
        let env = Env {
            frame: r.executor().frame(),
            sample_rate: SampleRate(SAMPLE_RATE),
            block_len: Samples(BLOCK),
            transport,
            changes,
        };
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        r.executor_mut()
            .process_with_changes(BLOCK, &transport, &changes, &[], &mut outs);
        seen.push((env, Told::of(&read_capture())));
    }
    seen
}

/// What the blocks in `seen` said for timeline frame `f`: `Env::transport_at`
/// in the block that held it, or, for a frame before the first block, the
/// first block's transport, unmoved (`TransportHistory`'s rule).
fn said_at(seen: &[(Env, Told)], f: Option<u64>) -> Transport {
    let Some(f) = f else {
        return seen[0].0.transport;
    };
    let (env, _) = seen
        .iter()
        .find(|(e, _)| e.offset_of(Frame(f)).is_some())
        .expect("a block held the frame");
    env.transport_at(env.offset_of(Frame(f)).expect("inside"))
}

/// **Behind a latent path**, each block the plugin is told the transport 40
/// frames before its first frame: the previous block's, 24 frames into it,
/// across a seek at the boundary, a tempo change, a loop that wrapped before
/// that frame, a record toggle and a stop — not the block's own.
///
/// Mutation (run): read `cx.env.transport_at(at)` in `Snapshot::at` (the
/// block's own time, the old polled behaviour) → every block reports its own
/// scene → fails. Mutation (run): build the snapshot in
/// `TransportFeed::block` from a `Cx` with `arrival: Latency::ZERO` → fails.
#[test]
fn a_live_node_behind_a_latent_path_is_told_its_inputs_transport() {
    let _lock = lock_probe();
    reset_probe();
    let (client, handle) = load();
    let (mut r, _) = rig(client, ARRIVAL);

    const BLOCKS: usize = 24;
    let seen = render_scenes(&mut r, BLOCKS);

    let mut differs = 0;
    for (b, (env, told)) in seen.iter().enumerate() {
        let heard = (b * BLOCK).checked_sub(ARRIVAL).map(|f| f as u64);
        let want = said_at(&seen, heard);
        assert!(told.is(&want), "block {b}: told {told:?}, heard {want:?}");
        if !told.is(&env.transport) {
            differs += 1;
        }
    }
    // The previous block's transport is not this one's: the test can tell.
    assert!(differs >= BLOCKS - 1, "{differs} blocks could tell");
    // The loop scene's wrap and the seek after it are inside the window:
    // block 4 hears block 3's loop wrapped (8 frames past its start), and
    // block 2 hears block 1's seek.
    let wrapped = said_at(&seen, Some(4 * BLOCK as u64 - ARRIVAL as u64));
    assert!(wrapped.looping.is_some());
    assert!((wrapped.beat().get() - (7.0 + 8.0 / 24_000.0)).abs() < 1e-12);
    assert!(seen[2].1.ppq > 64.0, "block 2 hears block 1's seek");
    drop(handle);
}

/// **With no latency** the plugin is told the block's own first frame, as
/// before.
///
/// Mutation (run): read the transport `ARRIVAL` frames before the block (a
/// `Cx` whose arrival is 40 here) → fails.
#[test]
fn a_live_node_with_no_latency_is_told_the_blocks_own_time() {
    let _lock = lock_probe();
    reset_probe();
    let (client, handle) = load();
    let (mut r, _) = rig(client, 0);

    let seen = render_scenes(&mut r, 12);
    for (b, (env, told)) in seen.iter().enumerate() {
        assert!(
            told.is(&env.transport),
            "block {b}: told {told:?}, the block says {:?}",
            env.transport
        );
    }
    drop(handle);
}

fn seven_eight() -> Arc<RtPublish<MeterMap>> {
    Arc::new(RtPublish::new(MeterMap::new([MeterChange::new(
        Beat(0.0),
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
    )])))
}

/// **The live node's meter** reaches the plugin: a 7/8 meter installed with
/// `set_meter` on the client before it went in; cleared, after it went in,
/// through the controls inserting it handed back (4/4); and installed
/// again through them.
///
/// Mutation (run): drop the meter source (`TransportFeed::block` reading
/// `self.default_meter` always) → 4 → fails. Mutation (run): hand back a
/// fresh `PluginMeter::default()` as the controls' meter in the node's
/// `IntoNode` (`InProcessVst2Client::controls`) → the handle is not the
/// node's cell → fails.
#[test]
fn the_live_node_tells_the_plugin_its_meter() {
    let _lock = lock_probe();
    reset_probe();
    let (client, handle) = load();
    assert!(!client.has_meter());
    client.set_meter(seven_eight());
    assert!(client.has_meter());
    let (mut r, meter) = rig(client, ARRIVAL);
    let seen = render_scenes(&mut r, 2);
    assert_eq!(seen[1].1.numerator, 7);
    let cap = read_capture();
    assert_eq!(cap.time_sig_denominator, 8);

    meter.clear_meter();
    let seen = render_scenes(&mut r, 1);
    assert_eq!(seen[0].1.numerator, 4, "no meter is 4/4");

    meter.set_meter(seven_eight());
    let seen = render_scenes(&mut r, 1);
    assert_eq!(seen[0].1.numerator, 7, "installed after insertion");
    drop(handle);
}

/// **`Plugin::set_meter` reaches an in-process VST2** (its VST2 arm): the
/// catalog's `Plugin` for the probe is given 7/8 before it goes in, and the
/// plugin is told 7/8.
///
/// Mutation (run): make the VST2 arm of `Plugin::meter` (which
/// `Plugin::set_meter` writes through) a fresh `PluginMeter::default()`, so
/// the install reaches no node → 4 → fails.
#[test]
fn plugin_set_meter_reaches_an_in_process_vst2() {
    let _lock = lock_probe();
    reset_probe();
    let plugin = tutti_plugin::catalog::Plugin::open(probe_path::probe_path(), SAMPLE_RATE)
        .expect("the probe opens");
    let handle = plugin.handle().clone();
    assert!(plugin.set_meter(seven_eight()), "a VST2 takes the meter");
    let (mut r, _) = rig(plugin, 0);
    let seen = render_scenes(&mut r, 1);
    assert_eq!(seen[0].1.numerator, 7);
    drop(handle);
}

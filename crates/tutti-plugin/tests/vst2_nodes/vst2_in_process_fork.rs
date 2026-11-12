//! Forking an in-process VST2 plugin **by state transfer**, against the
//! reference probe (`tutti-vst2-test-plugin`).
//!
//! What a fork promises (`format::in_process`'s `fork` module docs): a
//! second instance of the plugin from the same library, carrying the live
//! instance's saved state (its program, and its chunk or its parameter
//! values), rendering on the fork's own transport and rate, sharing nothing
//! with the live instance afterwards — and a named refusal when any step
//! fails.
//!
//! The probe runs with `TUTTI_VST2_PROBE_STATE_DC`: every output sample is the
//! input plus the channel tag (`1`, `101`) plus the instance's *state* as a DC
//! (`tutti_vst2_test_plugin::config::state_dc`: the chunk's first byte with
//! chunks; `1000 · param0 + 10 000 · program` without). The inputs here are
//! unwired (silence), so each forked sample is an exact small integer: state a
//! host restored is visible in the samples, state it only remembers is not.
//!
//! The env vars stay set for the whole test: the probe reads them in its
//! constructor, and the fork's second instance is constructed during the fork.

#![cfg(feature = "vst2")]

use std::sync::{Mutex, MutexGuard};
use tutti_fixtures::main_thread;
use tutti_plugin_types::Load;
use tutti_types::MaxBlock;
use tutti_vst2_host::Vst2Loaded;

use tutti_graph::NodeKey;
use tutti_graph::{Editor, ForkMode, ForkTarget, IntoNode, Prepare, Renderer, Transport};
use tutti_graph::{OutPort, Source};
use tutti_plugin::{PresetId, RenderMode};
use tutti_plugin_host::PluginHandle;
use tutti_plugin_host::{PluginNode, load_client};
use tutti_plugin_types::PluginForkError;
use tutti_plugin_types::{Normalized, ParamId};
use tutti_types::{
    Beat, BeatsPerBar, Bpm, MeterChange, MeterMap, NoteValue, SampleRate, Samples, TimeSignature,
};
use tutti_vst2_test_plugin::ProcessCapture;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// The probe's channel tags, as literals (`channel_tag(0)`, `channel_tag(1)`):
/// written as the function they would move with it.
const TAGS: [f32; 2] = [1.0, 101.0];

/// `kVstProcessLevelOffline`.
const LEVEL_OFFLINE: i32 = 4;

/// `kVstProcessLevelRealtime`.
const LEVEL_REALTIME: i32 = 2;

/// Serializes against the probe's process-global switches, capture and
/// environment.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Call one of the probe's exports on the image the host loaded, leaking the
/// handle (see `vst2_in_process_rt_lifecycle.rs`'s `probe_call`: dropping it
/// can unmap the image and lose the write).
fn probe_call<R>(symbol: &[u8], f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
    let path = tutti_fixtures::vst2_probe();
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

/// The probe's environment for one test, cleared when it drops.
struct ProbeEnv(Vec<&'static str>);

impl ProbeEnv {
    /// `TUTTI_VST2_PROBE_STATE_DC`, plus `TUTTI_VST2_PROBE_NO_CHUNKS` when
    /// `chunks` is false.
    fn new(chunks: bool) -> Self {
        let mut keys = vec!["TUTTI_VST2_PROBE_STATE_DC"];
        if !chunks {
            keys.push("TUTTI_VST2_PROBE_NO_CHUNKS");
        }
        for key in &keys {
            // SAFETY: callers hold `PROBE_LOCK`; no other thread here touches
            // the environment.
            unsafe { std::env::set_var(key, "1") };
        }
        Self(keys)
    }

    /// Also set `key` to `value`, for the instances constructed from now on
    /// (the probe reads its environment in its constructor), cleared on drop.
    fn set(&mut self, key: &'static str, value: &str) {
        // SAFETY: as in `new`.
        unsafe { std::env::set_var(key, value) };
        self.0.push(key);
    }
}

impl Drop for ProbeEnv {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: as in `new`.
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn load() -> (PluginNode<Vst2Loaded>, PluginHandle) {
    load_with(tutti_transport::TempoReader::constant(Bpm::of(120.0)).expect("a tempo"))
}

/// The probe loaded in process and bound to `tempo`, the project's tempo map.
fn load_with(tempo: tutti_transport::TempoReader) -> (PluginNode<Vst2Loaded>, PluginHandle) {
    let path = tutti_fixtures::vst2_probe();
    let (client, handle) = load_client::<Vst2Loaded>(
        &main_thread(),
        path,
        &Load::new(),
        tutti_plugin_types::SampleRate::new(SAMPLE_RATE),
    )
    .unwrap_or_else(|e| panic!("in-process load of the probe at {path:?}: {e:?}"));
    (client.bind(tempo), handle)
}

fn prepare_at(rate: f64) -> Prepare {
    Prepare::new(SampleRate(rate), MaxBlock::of(Samples(BLOCK)))
}

/// A live editor holding `node` at `key`, both its outputs global.
fn live_graph(key: NodeKey, node: impl IntoNode) -> Editor {
    let (mut editor, _exec) = Editor::new(prepare_at(SAMPLE_RATE));
    let _ = editor.insert(key, "vst2", node);
    editor
        .spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: key, port })));
    editor
}

/// Assert every frame of both channels is `tag + dc`.
fn assert_renders(out: &[Vec<f32>], dc: f32, what: &str) {
    for (ch, samples) in out.iter().enumerate() {
        let want = TAGS[ch] + dc;
        assert!(
            samples.iter().all(|&s| s == want),
            "{what}: channel {ch} must read {want} (tag {} + state {dc}) everywhere; got {:?}",
            TAGS[ch],
            &samples[..4]
        );
    }
}

/// A chunk blob as `Control::save_state` frames it.
fn chunk_state(payload: &[u8]) -> Vec<u8> {
    let mut blob = b"CHK\0".to_vec();
    blob.extend_from_slice(payload);
    blob
}

/// **A chunk plugin forks with the live instance's chunk**, through the graph
/// (`Editor::fork`, the path an export takes), and the fork tells the plugin
/// it renders offline.
///
/// The probe's default chunk starts `t` (116); the live one is handed one that
/// starts 7, so only a restored chunk renders `tag + 7`.
#[test]
fn a_chunk_plugin_forks_with_the_live_chunk_and_renders_offline() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    let (client, handle) = load();
    handle
        .load_state(&main_thread(), &chunk_state(&[7, 1, 2]))
        .expect("the probe takes a chunk");

    let key = NodeKey(1);
    let editor = live_graph(key, client);
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            ForkMode::Offline,
            prepare_at(SAMPLE_RATE),
        )
        .expect("a graph holding an in-process VST2 plugin forks");
    let out = Renderer::new(fork_ed, fork_exec).render(BLOCK * 4);
    assert_renders(&out, 7.0, "the fork");

    let cap = read_capture();
    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_OFFLINE,
        "an offline fork answers kVstProcessLevelOffline"
    );
    drop(handle);
}

/// **A plugin without chunks forks with its program and parameter values**,
/// and **nothing set on the live instance afterwards reaches the fork**.
///
/// The live instance is moved to program 1 and `param0 = 0.75`: the DC is
/// `1000 · 0.75 + 10 000 · 1 = 10 750`. The probe's program change moves no
/// parameter, so each term is restored or not on its own.
#[test]
fn a_plugin_without_chunks_forks_with_its_program_and_parameters() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(false);
    let (client, handle) = load();
    let program_1 = handle
        .presets(&main_thread())
        .expect("the VST2 path carries presets")
        .into_iter()
        .map(|p| p.id)
        .find(|id| *id == PresetId::Number(1))
        .expect("the probe declares two programs");
    assert_eq!(handle.load_preset(&main_thread(), &program_1), Some(true));
    handle
        .any_thread()
        .set_parameter_value(ParamId::new(0), Normalized::of(0.75));

    let fork = client
        .fork_instance(&main_thread(), ForkMode::Live)
        .expect("the probe forks");
    let key = NodeKey(2);
    let (mut editor, exec) = Editor::new(prepare_at(SAMPLE_RATE));
    editor.insert(key, "fork", tutti_graph::Unforkable(fork));
    editor
        .spec_mut()
        .topology
        .set_outputs((0..2).map(|port| Source::Node(OutPort { node: key, port })));
    editor.commit().expect("commits");
    let mut renderer = Renderer::new(editor, exec);
    assert_renders(&renderer.render(BLOCK * 2), 10_750.0, "the fork");

    handle
        .any_thread()
        .set_parameter_value(ParamId::new(0), Normalized::of(0.25));
    assert_eq!(
        handle.any_thread().parameter_value(ParamId::new(0)),
        Some(Normalized::of(0.25)),
        "the live write landed"
    );
    assert_renders(
        &renderer.render(BLOCK * 2),
        10_750.0,
        "the fork after a live write",
    );
    drop(client);
    drop(handle);
}

/// **A fork prepared at another rate tells the plugin that rate**, from its
/// own `prepare` — nothing pumps a fork's editor, so a rate parked for the
/// main thread (the live node's route) would never reach it.
#[test]
fn a_fork_at_another_rate_tells_the_plugin_its_rate() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    let (client, handle) = load();
    assert_eq!(read_capture().sample_rate, 48_000.0, "loaded at 48 kHz");

    let key = NodeKey(3);
    let editor = live_graph(key, client);
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            ForkMode::Offline,
            prepare_at(96_000.0),
        )
        .expect("forks");
    assert_eq!(
        read_capture().sample_rate,
        96_000.0,
        "the fork's prepare dispatched its rate"
    );
    // And it still renders, with its state (the default chunk, `t` = 116).
    assert_renders(
        &Renderer::new(fork_ed, fork_exec).render(BLOCK),
        116.0,
        "the 96 kHz fork",
    );
    drop(handle);
}

/// **A fork source whose live instance is gone says so**, without loading a
/// second instance: `PluginForkError::LiveGone`. The source holds the live
/// instance weakly, so it cannot keep a dropped plugin alive either.
#[test]
fn a_fork_of_a_dropped_plugin_is_live_gone() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    let (client, handle) = load();
    let parts = client.into_parts();
    let source = parts.fork.expect("an in-process VST2 node is forkable");
    drop(parts.node);
    drop(handle);

    let cause = match source.fork(ForkMode::Live) {
        Ok(_) => panic!("forked a plugin that is gone"),
        Err(cause) => cause,
    };
    assert!(
        matches!(
            cause.downcast_ref::<PluginForkError>(),
            Some(PluginForkError::LiveGone)
        ),
        "{cause:?}"
    );
}

/// **`Plugin::into_client` hands an in-process VST2 plugin back whole**, and
/// inserted through its own `IntoNode` (bevy-tutti's load path) it forks.
///
/// Not on macOS: there the probe builds as a `.dylib`, which `Plugin::open`
/// does not read as VST2 (a macOS VST2 is a `.vst` bundle).
#[cfg(not(target_os = "macos"))]
#[test]
fn a_plugin_handed_back_by_into_client_forks() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    let plugin = tutti_plugin::catalog::Plugin::open(
        Some(&main_thread()),
        &tutti_plugin::catalog::PluginRef::file(tutti_fixtures::vst2_probe())
            .expect("a plugin file"),
        tutti_plugin_types::SampleRate::new(SAMPLE_RATE),
    )
    .expect("the probe opens");
    let handle = plugin.handle().clone();
    handle
        .load_state(&main_thread(), &chunk_state(&[5]))
        .expect("the probe takes a chunk");
    let Err(plugin) = plugin.into_client() else {
        panic!("an in-process VST2 plugin is not a PluginClient");
    };

    let key = NodeKey(5);
    let editor = live_graph(
        key,
        plugin.bind(tutti_transport::TempoReader::constant(Bpm::of(120.0)).expect("a tempo")),
    );
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            ForkMode::Offline,
            prepare_at(SAMPLE_RATE),
        )
        .expect("forks");
    assert_renders(
        &Renderer::new(fork_ed, fork_exec).render(BLOCK),
        5.0,
        "the fork",
    );
    drop(handle);
}

/// **A fork reads its transport from the render, with the live node's
/// meter.** The live node has a 7/8 meter installed; the render hands the
/// fork's graph 93 BPM at beat 5. The probe records what
/// `audioMasterGetTime` answered on its last block — the fork's — so it must
/// read 93 BPM at quarter 5, in 7/8 (whose bar 2 starts at quarter 3.5).
#[test]
fn a_fork_reads_its_transport_from_the_render() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    // The tempo the plugin is told is the project map's: a constant 93 BPM,
    // which the render's transport carries too.
    let (client, handle) =
        load_with(tutti_transport::TempoReader::constant(Bpm::of(93.0)).expect("a tempo"));
    let seven_eight = TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH);
    client
        .controls()
        .set_meter(tutti_transport::ProjectMeter::new(MeterMap::new([
            MeterChange::new(Beat(0.0), seven_eight),
        ])));

    let key = NodeKey(6);
    let editor = live_graph(key, client);
    let (fork_ed, fork_exec) = editor
        .fork(
            ForkTarget::Node(key),
            ForkMode::Offline,
            prepare_at(SAMPLE_RATE),
        )
        .expect("forks");
    let mut renderer = Renderer::new(fork_ed, fork_exec);
    renderer.set_transport(Transport::new(true, Bpm::of(93.0), Beat(5.0), None));
    let _ = renderer.render(BLOCK);

    let cap = read_capture();
    assert!(
        cap.time_info_present,
        "the fork answered audioMasterGetTime"
    );
    assert_eq!(cap.time_tempo, 93.0, "the map's tempo");
    assert_eq!(cap.time_ppq_pos, 5.0, "the render's beat");
    assert_eq!(
        (cap.time_sig_numerator, cap.time_sig_denominator),
        (7, 8),
        "the live node's meter"
    );
    assert!((cap.time_bar_start_pos - 3.5).abs() < 1e-9);
    // Project time is the render's beat at its tempo: 5 × 60 / 93 s.
    assert_eq!(
        cap.time_sample_pos,
        (5.0 * 60.0 / 93.0 * SAMPLE_RATE).round()
    );
    // Quarter 5 is on a MIDI clock.
    assert_ne!(cap.time_flags & (1 << 15), 0, "kVstClockValid");
    assert_eq!(cap.time_samples_to_next_clock, 0);
    // No host clock relates to an offline render.
    assert_eq!(
        cap.time_flags & (1 << 8),
        0,
        "kVstNanosValid is clear offline"
    );
    drop(handle);
}

/// **A live fork keeps the live instance's render mode**: a host that set the
/// live plugin offline gets an offline fork from `ForkMode::Live` too (and a
/// realtime one otherwise).
#[test]
fn a_live_fork_keeps_the_live_render_mode() {
    let _lock = lock_probe();
    reset_probe();
    let _env = ProbeEnv::new(true);
    let (client, handle) = load();
    for (mode, level) in [
        (RenderMode::Realtime, LEVEL_REALTIME),
        (RenderMode::Offline, LEVEL_OFFLINE),
    ] {
        assert!(client.set_render_mode(mode));
        let fork = client
            .fork_instance(&main_thread(), ForkMode::Live)
            .expect("forks");
        let key = NodeKey(7);
        let (mut editor, exec) = Editor::new(prepare_at(SAMPLE_RATE));
        editor.insert(key, "fork", tutti_graph::Unforkable(fork));
        editor
            .spec_mut()
            .topology
            .set_outputs(vec![Source::Node(OutPort { node: key, port: 0 })]);
        editor.commit().expect("commits");
        let _ = Renderer::new(editor, exec).render(BLOCK);
        assert_eq!(read_capture().process_level, level, "{mode:?}");
    }
    drop(client);
    drop(handle);
}

/// **A file that now loads as another plugin is `Mismatch`**: the second
/// instance's id is not the live one's, so the live state is not loaded into
/// it. The probe's `uniqueId` is changed between the two loads (as a plugin
/// replaced on disk would be).
#[test]
fn a_second_instance_of_another_plugin_is_a_mismatch() {
    let _lock = lock_probe();
    reset_probe();
    let mut env = ProbeEnv::new(true);
    let (client, handle) = load();
    env.set("TUTTI_VST2_PROBE_UNIQUE_ID", "4242");
    let err = client
        .fork_instance(&main_thread(), ForkMode::Live)
        .expect_err("a different plugin is not a fork");
    match err {
        PluginForkError::Mismatch {
            expected, found, ..
        } => {
            assert_eq!(expected, client_id(&handle));
            assert_eq!(found, "vst2.4242");
        }
        other => panic!("expected Mismatch, got {other:?}"),
    }
    drop(client);
    drop(handle);
}

/// **A live program the second instance does not have is a named
/// `LoadState`**, not a fork at program 0: the live instance is at program 1
/// of 2, and the second instance (constructed with one program) has no
/// program 1.
#[test]
fn a_live_program_out_of_the_second_instances_range_is_a_named_error() {
    let _lock = lock_probe();
    reset_probe();
    let mut env = ProbeEnv::new(true);
    let (client, handle) = load();
    assert_eq!(
        handle.load_preset(&main_thread(), &PresetId::Number(1)),
        Some(true)
    );
    env.set("TUTTI_VST2_PROBE_PROGRAMS", "1");
    let err = client
        .fork_instance(&main_thread(), ForkMode::Live)
        .expect_err("program 1 does not exist in the second instance");
    assert!(
        matches!(err, PluginForkError::LoadState(_)),
        "expected LoadState, got {err:?}"
    );
    drop(client);
    drop(handle);
}

/// The live plugin's id, as the host reports it.
fn client_id(handle: &PluginHandle) -> String {
    handle.descriptor().id.clone()
}

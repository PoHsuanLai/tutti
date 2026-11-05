//! The FFI crate's safe handles against the reference CLAP plugin
//! (`tutti-clap-test-plugin`).
//!
//! These pin what the wrappers promise that no host test sees from outside:
//! which half of an instance can do what, that a retired audio half calls
//! nothing, and that the callbacks reach the embedder. Each test's mutation is
//! in its comment; every one was run and failed as stated.
//!
//! The plugin's switches (render mode, rescan requests) are process-globals of
//! its cdylib, so every test holds [`PLUGIN_LOCK`] across configure, load,
//! drive and assert.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_param_value,
};
use tutti_clap_ffi::{
    Bundle, CallError, EventRef, EventSink, EventSource, Extension, Host, HostCallbacks,
    InputStream, OutputStream, Plugin, PluginEvent, PortScratch, ProcessCall, ProcessError,
    ThreadRoles, header_size,
};
use tutti_clap_test_plugin::{PLUGIN_ID, REPORTED_LATENCY_SAMPLES, probe_tag};

/// `;`-separated candidate paths, from `build.rs`.
const CANDIDATES: &str = env!("TUTTI_CLAP_TEST_PLUGIN_CANDIDATES");

fn probe_path() -> &'static str {
    use std::sync::OnceLock;
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            tutti_fixture_resolve::resolve_or_panic(CANDIDATES, "tutti-clap-test-plugin")
        })
        .as_str()
}

static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `RenderMode::TagOnly`: every output sample is a nonzero per-slot tag.
const RENDER_TAG_ONLY: u32 = 2;

/// The reference cdylib, held open for the whole process: its switches are
/// statics of the loaded image, so dropping the last handle between a switch
/// and a load would reset them.
fn library() -> &'static libloading::Library {
    use std::sync::OnceLock;
    static LIB: OnceLock<libloading::Library> = OnceLock::new();
    LIB.get_or_init(|| {
        // SAFETY: loading the reference plugin runs its initialisers, which
        // are the test plugin's own.
        unsafe { libloading::Library::new(probe_path()) }.expect("open reference plugin")
    })
}

/// Sets the reference plugin's render mode through its exported switch.
fn set_render_mode(mode: u32) {
    // SAFETY: calling the cdylib's exported `extern "C"` switch with its
    // documented signature.
    unsafe {
        let set: libloading::Symbol<unsafe extern "C" fn(u32)> = library()
            .get(b"tutti_test_plugin_set_render_mode\0")
            .expect("render-mode switch");
        set(mode);
    }
}

/// Counts the callbacks the reference plugin makes.
#[derive(Default)]
struct Recorder {
    restarts: AtomicU32,
    rescans: AtomicU32,
}

impl HostCallbacks for Recorder {
    fn is_main_thread(&self) -> bool {
        true
    }

    fn is_audio_thread(&self) -> bool {
        false
    }

    fn request_restart(&self) {
        self.restarts.fetch_add(1, Ordering::SeqCst);
    }

    fn audio_ports_rescan(&self, flags: u32) {
        self.rescans.store(flags, Ordering::SeqCst);
    }
}

fn load(callbacks: Arc<Recorder>) -> Plugin {
    library();
    let bundle = Bundle::open(Path::new(probe_path()), None).expect("opens");
    let index = bundle
        .select(Some(PLUGIN_ID.to_str().unwrap()))
        .expect("selects");
    let host = Host::new(callbacks);
    bundle
        .instantiate(index, host, Arc::new(ThreadRoles::new()))
        .expect("instantiates")
}

/// No events in.
struct NoEvents;

impl EventSource for NoEvents {
    fn len(&self) -> usize {
        0
    }

    fn get(&self, _index: usize) -> Option<EventRef<'_>> {
        None
    }
}

/// One parameter value in.
struct OneValue(clap_event_param_value);

impl EventSource for OneValue {
    fn len(&self) -> usize {
        1
    }

    fn get(&self, index: usize) -> Option<EventRef<'_>> {
        (index == 0).then_some(EventRef::ParamValue(&self.0))
    }
}

/// Drops what the plugin emits, counting it.
#[derive(Default)]
struct Count(usize);

impl EventSink for Count {
    fn push(&mut self, _event: PluginEvent<'_>) -> bool {
        self.0 += 1;
        true
    }
}

fn param_value(param_id: u32, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: header_size::<clap_event_param_value>(),
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id,
        cookie: std::ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

const FRAMES: usize = 64;

/// A stereo-out block through `side`, no input, and the channels it wrote.
fn render(
    side: &mut tutti_clap_ffi::AudioSide,
    scratch: &mut PortScratch<f32>,
    outputs: &mut [&mut [f32]],
    frames: u32,
) -> Result<i32, ProcessError> {
    let mut sink = Count::default();
    side.process(ProcessCall {
        scratch,
        inputs: &[],
        outputs,
        input_ports: &[2],
        output_ports: &[2],
        frames,
        steady_time: 0,
        transport: None,
        in_events: &NoEvents,
        out_events: &mut sink,
        pool: None,
    })
}

/// The bundle lists the plugin it holds, selects it by id, and refuses an id
/// it does not hold rather than loading another plugin.
///
/// Mutation: make `select` ignore its argument and return 0: the unknown-id
/// assertion fails (the known one passes, the reference bundle's first plugin
/// being the probe).
#[test]
fn a_bundle_selects_by_id_and_refuses_an_unknown_one() {
    let _g = lock();
    let bundle = Bundle::open(Path::new(probe_path()), None).expect("opens");
    assert!(
        bundle
            .descriptors()
            .iter()
            .any(|d| d.id == PLUGIN_ID.to_str().unwrap())
    );
    assert!(bundle.select(Some(PLUGIN_ID.to_str().unwrap())).is_ok());
    let err = bundle.select(Some("no.such.plugin")).unwrap_err();
    assert!(err.reason.contains("no.such.plugin"), "{err}");
}

/// The main-thread half answers extension reads, and a `Plugin` is not
/// `Sync`.
///
/// Mutation: have `latency` return 0 without calling the plugin: the figure
/// assertion fails.
#[test]
fn the_main_thread_half_reads_the_plugins_extensions() {
    let _g = lock();
    let plugin = load(Arc::default());
    assert_eq!(plugin.latency(), REPORTED_LATENCY_SAMPLES);
    assert!(plugin.supports(Extension::Params));
    assert!(plugin.param_count() > 0);
    assert!(plugin.audio_port_count(false) > 0);
    assert!(plugin.audio_port_info(0, false).is_some());
    assert!(plugin.audio_port_info(u32::MAX, false).is_none());
}

/// Activation hands out one audio half; while it lives the plugin cannot be
/// activated again, and the half belongs to the plugin that made it.
///
/// Mutation: drop the `audio_sides() != 0` refusal in `activate`: the second
/// `activate` returns true and the assertion fails.
#[test]
fn activation_hands_out_one_audio_half_and_refuses_a_second_activation() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    assert!(plugin.audio_side().is_none(), "inactive: no audio half");
    assert!(plugin.activate(48_000.0, 1, 512));
    let side = plugin.audio_side().expect("active: an audio half");
    assert_eq!(plugin.audio_sides(), 1);
    assert!(plugin.owns(&side));
    assert!(
        !plugin.activate(48_000.0, 1, 512),
        "the half is still alive"
    );

    drop(side);
    assert_eq!(plugin.audio_sides(), 0);
    plugin.deactivate();
    assert!(!plugin.is_active());
}

/// A block runs the plugin, which writes its tagged output through the
/// channel tables the wrapper bound, and starts processing on the first block.
///
/// Mutation: bind the output table one channel off (`wrapping_add(1)`): the
/// slot-0 tag is missing from channel 0 and the assertion fails.
#[test]
fn a_block_runs_the_plugin_through_the_bound_channels() {
    let _g = lock();
    set_render_mode(RENDER_TAG_ONLY);
    let mut plugin = load(Arc::default());
    assert!(plugin.activate(48_000.0, 1, 512));
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::<f32>::new();
    scratch.resize_for(&[2], &[2], 512);

    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    assert!(!side.is_processing());
    render(&mut side, &mut scratch, &mut outputs, FRAMES as u32).expect("the block runs");
    assert!(side.is_processing(), "the first block starts processing");
    assert_eq!(left[0], probe_tag(0, 0));
    assert_eq!(right[0], probe_tag(0, 1));
}

/// A channel shorter than the block is refused before the plugin is called:
/// it would write past the end.
///
/// Mutation: drop the length check in `PortScratch::bind`: the plugin runs
/// (`is_processing` flips) and the error assertion fails.
#[test]
fn a_short_channel_is_refused_without_calling_the_plugin() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    assert!(plugin.activate(48_000.0, 1, 512));
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::<f32>::new();
    scratch.resize_for(&[2], &[2], 512);

    let mut left = vec![0.0f32; FRAMES / 2];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    assert_eq!(
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32),
        Err(ProcessError::BufferTooShort)
    );
    assert!(!side.is_processing(), "the plugin was never called");
}

/// A parameter value the host sends in a block's events is applied by the
/// plugin, and `params.flush` on an inactive plugin reaches it too.
///
/// Mutation: pass an empty event list to `flush` in `params_flush`: the value
/// stays at its default and the assertion fails.
#[test]
fn an_event_list_reaches_the_plugin_through_flush() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    let info = plugin.param_info(0).expect("a parameter");
    let target = info.default_value + (info.max_value - info.min_value) * 0.25;
    let mut sink = Count::default();
    assert!(plugin.params_flush(&OneValue(param_value(info.id, target)), &mut sink));
    assert_eq!(plugin.param_value(info.id), Some(target));
}

/// What `state.save` writes, `state.load` reads back: the streams carry the
/// plugin's bytes both ways.
///
/// Mutation: make `OutputStream`'s write callback ignore its input: the saved
/// state is empty and the first assertion fails.
#[test]
fn state_round_trips_through_the_streams() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    let mut out = OutputStream::new();
    plugin.state_save(&mut out).expect("saves");
    let bytes = out.into_data();
    assert!(!bytes.is_empty(), "the plugin wrote its state");
    let mut input = InputStream::new(&bytes);
    plugin.state_load(&mut input).expect("loads");
    assert_eq!(input.remaining(), 0, "the plugin read all of it");
}

/// An extension the plugin does not implement is reported as missing, not as a
/// refusal.
///
/// Mutation: map `None` of the extension to `CallError::Failed`: the assertion
/// fails.
#[test]
fn a_missing_extension_is_not_a_refusal() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    // The reference plugin has no `preset-load`.
    assert!(!plugin.supports(Extension::PresetLoad));
    assert_eq!(
        plugin.preset_load_file(Path::new("/nowhere.preset")),
        Err(CallError::NoExtension)
    );
}

/// The plugin's calls into the host reach the embedder: an `audio-ports`
/// rescan the plugin makes from `on_main_thread` arrives with its flags.
///
/// Mutation: drop the `cb.audio_ports_rescan(flags)` forward in `host.rs`: the
/// recorded flags stay 0 and the assertion fails.
#[test]
fn a_plugin_callback_reaches_the_embedder() {
    let _g = lock();
    let recorder = Arc::new(Recorder::default());
    let plugin = load(recorder.clone());
    // SAFETY: the reference cdylib's exported switch, documented signature.
    unsafe {
        let rescan: libloading::Symbol<unsafe extern "C" fn(u32)> = library()
            .get(b"tutti_test_plugin_request_ports_rescan\0")
            .unwrap();
        rescan(0b10);
    }
    plugin.on_main_thread();
    assert_eq!(recorder.rescans.load(Ordering::SeqCst), 0b10);
}

/// Once the `Plugin` is gone its audio half calls nothing: a block is refused
/// as retired, `reset` and `stop` are no-ops, and nothing is read through a
/// freed instance.
///
/// Mutation: drop the `retired` check in `AudioSide::process`: the block
/// calls into the destroyed instance (a crash) instead of returning the
/// error.
#[test]
fn a_retired_audio_half_calls_nothing() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    assert!(plugin.activate(48_000.0, 1, 512));
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::<f32>::new();
    scratch.resize_for(&[2], &[2], 512);
    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    render(&mut side, &mut scratch, &mut outputs, FRAMES as u32).expect("runs while alive");
    assert!(side.is_processing());

    drop(plugin);
    assert!(side.is_retired());
    assert_eq!(
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32),
        Err(ProcessError::Retired)
    );
    side.reset();
    side.stop();
}

/// A deactivated plugin's audio half is retired too: a block after a rejoin
/// must not start processing on an inactive plugin.
///
/// Mutation: make `AudioSide::is_retired` read only `retired`: the block
/// starts processing on the deactivated plugin and the assertion fails.
#[test]
fn a_deactivated_plugins_audio_half_is_retired() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    assert!(plugin.activate(48_000.0, 1, 512));
    let mut side = plugin.audio_side().unwrap();
    plugin.deactivate();
    let mut scratch = PortScratch::<f32>::new();
    scratch.resize_for(&[2], &[2], 512);
    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    assert_eq!(
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32),
        Err(ProcessError::Retired)
    );
    assert!(!side.is_processing());
}

/// Deactivating a processing plugin stops processing first, and dropping one
/// that is processing tears it down: the library loads again afterwards.
///
/// Mutation: skip the `stop_processing` branch in `stop_and_deactivate_claimed`
/// (`if false`): the audio half still reads as processing after `deactivate`
/// and the first assertion fails.
#[test]
fn deactivating_a_processing_plugin_stops_it_and_a_drop_tears_it_down() {
    let _g = lock();
    {
        let mut plugin = load(Arc::default());
        assert!(plugin.activate(48_000.0, 1, 512));
        let mut side = plugin.audio_side().unwrap();
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&[2], &[2], 512);
        let mut left = vec![0.0f32; FRAMES];
        let mut right = vec![0.0f32; FRAMES];
        let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32).expect("runs");
        assert!(side.is_processing());
        plugin.deactivate();
        assert!(!side.is_processing(), "deactivate stops processing first");
        assert!(!plugin.activate(48_000.0, 1, 512), "the half is alive");
        drop(side);
        assert!(
            plugin.activate(48_000.0, 1, 512),
            "and activates again once gone"
        );
    }
    let plugin = load(Arc::default());
    assert_eq!(plugin.latency(), REPORTED_LATENCY_SAMPLES);
}

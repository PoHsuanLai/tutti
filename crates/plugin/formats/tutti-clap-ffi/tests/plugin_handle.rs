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
    ActivateError, Bundle, CallError, EventRef, EventSink, EventSource, Extension, Host,
    HostCallbacks, InputStream, OutputStream, Planar, Plugin, PluginEvent, PortScratch,
    ProcessCall, ProcessError, TaskPool, ThreadRoles, header_size,
};
use tutti_clap_test_plugin::{PLUGIN_ID, REPORTED_LATENCY_SAMPLES, probe_tag};
use tutti_plugin_types::SampleFormat;

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
    scratch: &mut PortScratch,
    outputs: &mut [&mut [f32]],
    frames: u32,
) -> Result<i32, ProcessError> {
    let mut sink = Count::default();
    side.process(ProcessCall {
        scratch,
        audio: Planar::F32 {
            inputs: &[],
            outputs,
        },
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
/// `activate` succeeds and the assertion fails; return `Refused` for it: the
/// error assertion fails.
#[test]
fn activation_hands_out_one_audio_half_and_refuses_a_second_activation() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    assert!(plugin.audio_side().is_none(), "inactive: no audio half");
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let side = plugin.audio_side().expect("active: an audio half");
    assert_eq!(plugin.audio_sides(), 1);
    assert!(plugin.owns(&side));
    assert_eq!(
        plugin.activate(48_000.0, 1, 512),
        Err(ActivateError::ProcessorStillAlive),
        "the half is still alive: its own error, not a refusal"
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
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::new(SampleFormat::Float32);
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
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::new(SampleFormat::Float32);
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
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let mut side = plugin.audio_side().unwrap();
    let mut scratch = PortScratch::new(SampleFormat::Float32);
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
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let mut side = plugin.audio_side().unwrap();
    plugin.deactivate();
    let mut scratch = PortScratch::new(SampleFormat::Float32);
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
        plugin.activate(48_000.0, 1, 512).expect("activates");
        let mut side = plugin.audio_side().unwrap();
        let mut scratch = PortScratch::new(SampleFormat::Float32);
        scratch.resize_for(&[2], &[2], 512);
        let mut left = vec![0.0f32; FRAMES];
        let mut right = vec![0.0f32; FRAMES];
        let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32).expect("runs");
        assert!(side.is_processing());
        plugin.deactivate();
        assert!(!side.is_processing(), "deactivate stops processing first");
        assert_eq!(
            plugin.activate(48_000.0, 1, 512),
            Err(ActivateError::ProcessorStillAlive)
        );
        drop(side);
        assert!(
            plugin.activate(48_000.0, 1, 512).is_ok(),
            "and activates again once gone"
        );
    }
    let plugin = load(Arc::default());
    assert_eq!(plugin.latency(), REPORTED_LATENCY_SAMPLES);
}

/// Calls an exported `extern "C" fn(u32)` switch of the reference plugin.
fn switch_u32(symbol: &[u8], value: u32) {
    // SAFETY: the reference cdylib's exported switch, documented signature.
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn(u32)> =
            library().get(symbol).expect("switch symbol");
        f(value);
    }
}

/// A pool that counts the jobs it is given and runs them on the caller.
#[derive(Default)]
struct CountingPool {
    runs: AtomicU32,
}

impl TaskPool for CountingPool {
    fn participants(&self) -> usize {
        2
    }

    fn try_run(&self, _wake: usize, job: &(dyn Fn(usize) + Sync)) -> bool {
        self.runs.fetch_add(1, Ordering::SeqCst);
        job(0);
        true
    }
}

/// Activates `plugin` and returns its audio half and a scratch for it.
fn start(plugin: &mut Plugin, max_frames: u32) -> (tutti_clap_ffi::AudioSide, PortScratch) {
    plugin.activate(48_000.0, 1, max_frames).expect("activates");
    let mut scratch = PortScratch::new(SampleFormat::Float32);
    scratch.resize_for(&[2], &[2], max_frames as usize);
    (plugin.audio_side().unwrap(), scratch)
}

/// A block through `side` with `pool`, four tasks requested by the plugin.
fn render_with_pool(
    side: &mut tutti_clap_ffi::AudioSide,
    scratch: &mut PortScratch,
    pool: &tutti_clap_ffi::PoolHandle,
) {
    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    let mut sink = Count::default();
    side.process(ProcessCall {
        scratch,
        audio: Planar::F32 {
            inputs: &[],
            outputs: &mut outputs,
        },
        input_ports: &[2],
        output_ports: &[2],
        frames: FRAMES as u32,
        steady_time: 0,
        transport: None,
        in_events: &NoEvents,
        out_events: &mut sink,
        pool: Some(pool),
    })
    .expect("the block runs");
}

/// A pool handle serves only the instance whose vtable its `exec` came from.
/// Handed to another instance's `process` it is ignored, so `exec` is never
/// called with a `clap_plugin` it does not belong to.
///
/// Mutation: drop the `handle.is_for(&self.core)` filter in `process`: the
/// foreign handle is served, the pool runs, and the second assertion fails.
#[test]
fn a_pool_handle_serves_only_its_own_instance() {
    let _g = lock();
    switch_u32(b"tutti_test_plugin_set_thread_pool_tasks\0", 4);
    let mut a = load(Arc::default());
    let mut b = load(Arc::default());
    let (mut side_a, mut scratch_a) = start(&mut a, 512);
    let (mut side_b, mut scratch_b) = start(&mut b, 512);
    let pool = Arc::new(CountingPool::default());
    let handle_a = a.pool_handle(pool.clone()).expect("the plugin has exec");

    render_with_pool(&mut side_a, &mut scratch_a, &handle_a);
    let own = pool.runs.load(Ordering::SeqCst);
    assert!(own > 0, "A's own handle is served on A");

    render_with_pool(&mut side_b, &mut scratch_b, &handle_a);
    assert_eq!(
        pool.runs.load(Ordering::SeqCst),
        own,
        "A's handle is ignored on B"
    );
    switch_u32(b"tutti_test_plugin_set_thread_pool_tasks\0", 0);
}

/// A block longer than the plugin was activated for is refused before any
/// call, whoever the caller is.
///
/// Mutation: drop the `max_frames` check in `process`: the block runs
/// (`is_processing` flips) and the error assertion fails.
#[test]
fn a_block_past_the_activated_size_is_refused() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    let (mut side, mut scratch) = start(&mut plugin, FRAMES as u32 / 2);
    scratch.resize_for(&[2], &[2], FRAMES);
    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    assert_eq!(
        render(&mut side, &mut scratch, &mut outputs, FRAMES as u32),
        Err(ProcessError::BlockTooLarge)
    );
    assert!(!side.is_processing());
}

/// The entries CLAP gives to `[!active]` are refused while the plugin is
/// active (nothing is called), and reach the plugin when it is not.
///
/// Mutation: drop `require_inactive()` from any one of them: that entry
/// returns `Ok` while active and its assertion fails.
#[test]
fn state_gated_entries_are_refused_while_active() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    let request = [tutti_clap_ffi::PortConfigRequest {
        is_input: true,
        port_index: 0,
        channel_count: 2,
    }];
    // Inactive: each reaches the plugin (the reference plugin has none of the
    // extensions, so the answer is `false`, not a state error).
    assert_eq!(plugin.select_audio_ports_config(0), Ok(false));
    assert_eq!(
        plugin.can_apply_audio_port_configuration(&request),
        Ok(false)
    );
    assert_eq!(plugin.apply_audio_port_configuration(&request), Ok(false));
    assert_eq!(plugin.add_audio_port(true, 2, None), Ok(false));
    assert_eq!(plugin.remove_audio_port(true, 0), Ok(false));

    plugin.activate(48_000.0, 1, 512).expect("activates");
    assert_eq!(
        plugin.select_audio_ports_config(0),
        Err(CallError::WrongState)
    );
    assert_eq!(
        plugin.can_apply_audio_port_configuration(&request),
        Err(CallError::WrongState)
    );
    assert_eq!(
        plugin.apply_audio_port_configuration(&request),
        Err(CallError::WrongState)
    );
    assert_eq!(
        plugin.add_audio_port(true, 2, None),
        Err(CallError::WrongState)
    );
    assert_eq!(
        plugin.remove_audio_port(true, 0),
        Err(CallError::WrongState)
    );
}

/// `audio-ports-activation.set_active` on a processing plugin is refused
/// unless the plugin said it can activate a port while processing.
///
/// Mutation: drop the processing check in `set_audio_port_active`: the call
/// reaches the (absent) extension and answers `Ok(false)`, and the assertion
/// fails.
#[test]
fn a_port_cannot_be_activated_while_processing_unless_the_plugin_allows() {
    let _g = lock();
    let mut plugin = load(Arc::default());
    let (mut side, mut scratch) = start(&mut plugin, 512);
    assert_eq!(plugin.set_audio_port_active(true, 0, true, 32), Ok(false));
    let mut left = vec![0.0f32; FRAMES];
    let mut right = vec![0.0f32; FRAMES];
    let mut outputs: Vec<&mut [f32]> = vec![&mut left, &mut right];
    render(&mut side, &mut scratch, &mut outputs, FRAMES as u32).expect("runs");
    assert!(side.is_processing());
    assert_eq!(
        plugin.set_audio_port_active(true, 0, true, 32),
        Err(CallError::WrongState)
    );
}

/// What the reference plugin's gui recorded.
fn gui_capture() -> tutti_clap_test_plugin::gui::GuiCapture {
    // SAFETY: the reference cdylib's exported capture reader, which fills a
    // `GuiCapture` of the layout this crate shares with it.
    unsafe {
        let f: libloading::Symbol<
            unsafe extern "C" fn(*mut tutti_clap_test_plugin::gui::GuiCapture) -> bool,
        > = library()
            .get(b"tutti_test_plugin_gui_capture\0")
            .expect("capture");
        let mut cap = std::mem::MaybeUninit::zeroed();
        assert!(f(cap.as_mut_ptr()));
        cap.assume_init()
    }
}

fn gui_reset() {
    // SAFETY: the reference cdylib's exported reset.
    unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn()> = library()
            .get(b"tutti_test_plugin_gui_reset\0")
            .expect("reset");
        f();
    }
}

/// `clap.gui` entries follow `create` and precede `destroy`: before `create`
/// a resize or a close reaches nothing, a second `create` is refused, and
/// `destroy` runs once however often the editor is closed.
///
/// Mutation: drop the `gui_created` test from `close_gui`: the second close
/// destroys again and the balance reads -1; drop it from `resize_gui`: the
/// early resize reaches the plugin and `NotCreated` is not returned; drop the
/// `AlreadyCreated` test: the balance reads 2.
#[test]
fn the_editor_is_created_once_and_destroyed_once() {
    let _g = lock();
    switch_u32(b"tutti_test_plugin_set_gui_mode\0", 1);
    gui_reset();
    let mut plugin = load(Arc::default());
    let parent = {
        // SAFETY: the reference plugin never dereferences its parent.
        unsafe { tutti_plugin_types::WindowHandle::from_raw(std::ptr::null_mut()) }
    };

    assert_eq!(
        plugin.resize_gui(10, 10),
        Err(tutti_clap_ffi::GuiError::NotCreated)
    );
    assert!(plugin.gui_hints().is_none(), "no hints before create");
    plugin.close_gui(false);
    assert_eq!(gui_capture().call_count, 0, "nothing reached the plugin");

    plugin.open_embedded(&parent, 1.0).expect("opens");
    assert_eq!(gui_capture().create_balance, 1);
    assert_eq!(
        plugin.open_embedded(&parent, 1.0).unwrap_err(),
        tutti_clap_ffi::GuiError::AlreadyCreated
    );
    assert_eq!(gui_capture().create_balance, 1, "no second create");

    plugin.close_gui(false);
    plugin.close_gui(false);
    assert_eq!(gui_capture().create_balance, 0, "destroyed exactly once");
    switch_u32(b"tutti_test_plugin_set_gui_mode\0", 0);
}

/// A 64-bit block through the same call and type as a 32-bit one: the scratch
/// carries its format as a value, one `process` runs either kernel, and a
/// block of the other width is refused before the plugin is called.
///
/// Mutations (run): bind the 64-bit table as `data32` in `Sample for f64`: the
/// 64-bit block never reaches `data64` and the tag assertion fails; make the
/// catch-all arm of `process` answer `Ok`: the mismatch assertion fails.
#[test]
fn one_process_call_runs_either_width_and_refuses_a_mismatch() {
    let _g = lock();
    set_render_mode(RENDER_TAG_ONLY);
    // Both widths need every port flagged 64-bit before the plugin loads.
    switch_u32(b"tutti_test_plugin_set_ports_64bit\0", 3);
    let mut plugin = load(Arc::default());
    plugin.activate(48_000.0, 1, 512).expect("activates");
    let mut side = plugin.audio_side().unwrap();

    let mut wide = PortScratch::new(SampleFormat::Float64);
    wide.resize_for(&[2], &[2], 512);
    assert_eq!(wide.format(), SampleFormat::Float64);
    let mut left = vec![0.0f64; FRAMES];
    let mut right = vec![0.0f64; FRAMES];
    let mut outputs: Vec<&mut [f64]> = vec![&mut left, &mut right];
    let mut sink = Count::default();
    let mut call = |scratch: &mut PortScratch, audio: Planar<'_, '_>| {
        side.process(ProcessCall {
            scratch,
            audio,
            input_ports: &[2],
            output_ports: &[2],
            frames: FRAMES as u32,
            steady_time: 0,
            transport: None,
            in_events: &NoEvents,
            out_events: &mut sink,
            pool: None,
        })
    };
    call(
        &mut wide,
        Planar::F64 {
            inputs: &[],
            outputs: &mut outputs,
        },
    )
    .expect("a 64-bit block runs");
    assert_eq!(left[0], f64::from(probe_tag(0, 0)));
    assert_eq!(right[0], f64::from(probe_tag(0, 1)));

    // A 32-bit block into the 64-bit scratch: refused, plugin untouched.
    let mut l32 = vec![0.0f32; FRAMES];
    let mut r32 = vec![0.0f32; FRAMES];
    let mut out32: Vec<&mut [f32]> = vec![&mut l32, &mut r32];
    assert_eq!(
        call(
            &mut wide,
            Planar::F32 {
                inputs: &[],
                outputs: &mut out32
            }
        ),
        Err(ProcessError::FormatMismatch)
    );
    assert_eq!(out32[0][0], 0.0, "the plugin wrote nothing");

    // The same call at the other width through a 32-bit scratch.
    let mut narrow = PortScratch::new(SampleFormat::Float32);
    narrow.resize_for(&[2], &[2], 512);
    call(
        &mut narrow,
        Planar::F32 {
            inputs: &[],
            outputs: &mut out32,
        },
    )
    .expect("a 32-bit block runs");
    assert_eq!(l32[0], probe_tag(0, 0));
    switch_u32(b"tutti_test_plugin_set_ports_64bit\0", 0);
}

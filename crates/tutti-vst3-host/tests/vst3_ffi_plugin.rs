//! `tutti_vst3_ffi::Plugin` against the in-repo reference plugin (`audio-probe`):
//! the audio half's hand-out, the state changes it refuses, and the claim
//! against a real `Drop`.
//!
//! These live here, not in the FFI crate, because the probe is built by this
//! crate's build script and the FFI crate cannot depend on this one.

use std::path::PathBuf;
use std::sync::Arc;
use tutti_fixtures::main_thread;

use tutti_vst3_ffi::{
    AudioSide, BusBuffers, EventList, MIN_POINT_ROOM, Module, ParameterChangesImpl, Plugin,
    ProcessIo, SampleWidth, Vst3Error, zeroed,
};
use vst3::Steinberg::Vst::ProcessSetup;

const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");
const BLOCK: usize = 64;

fn probe_path() -> PathBuf {
    let bundle = std::path::Path::new(PROBE_DIR_BUILT).join("audio-probe.vst3");
    tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or(bundle)
}

fn setup() -> ProcessSetup {
    ProcessSetup {
        processMode: 0,
        symbolicSampleSize: SampleWidth::F32.symbolic_size(),
        maxSamplesPerBlock: BLOCK as i32,
        sampleRate: 48_000.0,
    }
}

/// A probe plugin, initialized and not yet set up.
fn plugin() -> (Arc<Module>, Plugin) {
    let module = Module::open(&probe_path()).expect("the probe loads");
    let class = module
        .classes()
        .find(|c| c.category.contains("Audio"))
        .expect("the probe exports an audio class");
    let mut plugin = module.create_plugin(&class.cid).expect("a plugin");
    plugin
        .initialize_component(&main_thread())
        .expect("initialize");
    (module, plugin)
}

/// Sets up and activates the plugin, as the host's activation does.
fn activate(plugin: &mut Plugin) {
    plugin
        .setup_processing(&main_thread(), setup())
        .expect("setup");
    plugin.set_active(&main_thread(), true).expect("activate");
}

/// No audio half until the plugin is set up and active; one at a time after.
///
/// Mutation: drop the `state.active && state.setup.is_some()` check in
/// `Shared::take_audio_side` and the first assertion fails; drop the
/// `audio_out` swap and the last one fails.
#[test]
fn the_audio_half_needs_a_set_up_active_plugin_and_comes_once() {
    let (_module, mut plugin) = plugin();
    assert!(plugin.audio_side().is_none(), "not set up");
    plugin.setup_processing(&main_thread(), setup()).unwrap();
    assert!(plugin.audio_side().is_none(), "set up, not active");
    plugin.set_active(&main_thread(), true).unwrap();
    let side = plugin.audio_side().expect("set up and active");
    assert!(plugin.audio_side().is_none(), "a second half is refused");
    drop(side);
    assert!(plugin.audio_side().is_some(), "free again once it drops");
}

/// While the audio half exists, the state changes are refused.
///
/// Mutation: make `lock_for_control` ignore `audio_out` and the first
/// assertion fails.
#[test]
fn state_changes_are_refused_while_the_audio_half_is_out() {
    let (_module, mut plugin) = plugin();
    activate(&mut plugin);
    let side = plugin.audio_side().unwrap();
    assert_eq!(
        plugin.set_active(&main_thread(), false),
        Err(Vst3Error::Busy)
    );
    assert_eq!(plugin.set_processing(true), Err(Vst3Error::Busy));
    assert_eq!(
        plugin.setup_processing(&main_thread(), setup()),
        Err(Vst3Error::Busy)
    );
    assert_eq!(
        plugin.set_bus_arrangements(&main_thread(), &mut [], &mut []),
        Err(Vst3Error::Busy)
    );
    assert!(plugin.is_active(), "refused changes changed nothing");
    drop(side);
    assert_eq!(plugin.set_active(&main_thread(), false), Ok(()));
}

/// A real block through the audio half; after the plugin is dropped the half
/// answers `NotInitialized` and touches nothing.
///
/// Mutation: drop the `retired` flag in `Plugin::drop` and the post-drop call
/// reaches a terminated processor (the probe then answers or crashes instead
/// of `NotInitialized`).
#[test]
fn the_audio_half_answers_not_initialized_after_the_plugin_drops() {
    let (_module, mut plugin) = plugin();
    activate(&mut plugin);
    let mut side: AudioSide = plugin.audio_side().unwrap();

    let channels = plugin.audio_bus_channels(&main_thread(), tutti_vst3_ffi::BusDirection::Output);
    let out_width = channels.iter().sum::<usize>().max(2);
    let in_channels =
        plugin.audio_bus_channels(&main_thread(), tutti_vst3_ffi::BusDirection::Input);
    let in_width = in_channels.iter().sum::<usize>();
    let mut input = BusBuffers::new(
        SampleWidth::F32,
        &in_channels,
        tutti_types::ChannelLayout::try_from(in_width.max(1)).expect("a test plugin is narrow"),
        BLOCK,
    )
    .unwrap();
    let mut output = BusBuffers::new(
        SampleWidth::F32,
        &channels,
        tutti_types::ChannelLayout::try_from(out_width).expect("a test plugin is narrow"),
        BLOCK,
    )
    .unwrap();
    let in_events = EventList::input();
    let out_events = EventList::output();
    let params = ParameterChangesImpl::with_queues(
        4,
        MIN_POINT_ROOM,
        tutti_plugin_types::PointPool::default(),
    );

    let mut block = |side: &mut AudioSide| {
        let ins_data = vec![0.0f32; BLOCK];
        let mut outs_data: Vec<Vec<f32>> = vec![vec![0.0; BLOCK]; out_width];
        let ins: Vec<&[f32]> = (0..in_width).map(|_| ins_data.as_slice()).collect();
        let mut outs: Vec<&mut [f32]> = outs_data.iter_mut().map(Vec::as_mut_slice).collect();
        let mut context = zeroed();
        let setup = setup();
        side.process(ProcessIo {
            mode: tutti_vst3_ffi::ProcessMode::Realtime,
            num_samples: BLOCK,
            inputs: input.bind_inputs_f32(&ins)?,
            outputs: output.bind_outputs_f32(&mut outs)?,
            input_events: &in_events,
            output_events: &out_events,
            input_params: None,
            output_params: &params,
            context: &mut context,
            setup: &setup,
        })
    };

    assert_eq!(block(&mut side), Ok(()));
    drop(plugin);
    assert_eq!(block(&mut side), Err(Vst3Error::NotInitialized));
    assert_eq!(side.set_processing(true), Err(Vst3Error::NotInitialized));
}

/// `kMisbehaveSetupFails` in `probeids.h`: `setupProcessing` answers
/// `kResultFalse`.
const SETUP_FAILS: &str = "7";

/// A setup the plugin answers `kResultFalse` to is tolerated (the host's
/// activation proceeds, as `a_refused_setup_is_tolerated_by_design` pins), and
/// the setup is recorded so the audio half is handed out and holds blocks to it.
///
/// Mutation: record the setup only for `Ok(true)` and the last assertion fails.
/// (A setup call that *errors* clears the previous setup; no probe behaviour
/// makes `setupProcessing` error, so that path is not exercised here.)
#[test]
fn a_setup_answered_false_is_tolerated_and_recorded() {
    // SAFETY: nextest runs each test in its own process, so nothing else reads
    // the environment while this sets it, and the probe reads it at creation.
    unsafe { std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", SETUP_FAILS) };
    let (_module, mut plugin) = plugin();
    assert_eq!(plugin.setup_processing(&main_thread(), setup()), Ok(false));
    plugin.set_active(&main_thread(), true).expect("activate");
    assert!(plugin.audio_side().is_some());
}

/// Opening one module image from several threads at once is safe.
///
/// The SDK's plugin side keeps process-global state that a module open
/// mutates unlocked, and `dlopen` hands every thread the same image, so
/// `Module::open` serialises itself. The modules are kept until the threads
/// have joined and dropped on this thread, so this exercises the open alone;
/// the next test adds the drops.
///
/// This is a probabilistic test: the race is in the plugin's memory, so a
/// broken open usually aborts the process (a heap-corruption abort or an
/// overflow panic in the host's COM release) and sometimes does not. Without
/// the lock this test killed the process (abort or segfault) in 99 of 100
/// runs on the machine it was written on; with it, none of 350 runs failed.
///
/// Mutation: remove the lock in `Module::open` and the process aborts.
#[test]
fn concurrent_opens_of_one_image_do_not_corrupt_the_plugins_globals() {
    const THREADS: usize = 8;
    const OPENS: usize = 25;
    let path = probe_path();
    let start = std::sync::Barrier::new(THREADS);
    let modules: Vec<Arc<Module>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                s.spawn(|| {
                    start.wait();
                    (0..OPENS)
                        .map(|_| Module::open(&path).expect("the probe opens"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("an open did not panic"))
            .collect()
    });
    assert_eq!(modules.len(), THREADS * OPENS);
    assert!(modules.iter().all(|m| m.class_count() > 0));
}

/// Opening and dropping one image from several threads at once is safe.
///
/// A drop releases the factory, runs the module exit and unloads the image, and
/// the plugin's `~CPluginFactory` clears `gPluginFactory` while another
/// thread's `GetPluginFactory` may add a reference to it, so teardown has to
/// take the same lock as an open.
///
/// Probabilistic, like the test above: 4 threads of 400,000 opens and drops
/// (about 9 s). With the lock left out of `Module::drop` (the open lock kept)
/// the process died in 37 of 40 runs on the machine it was written on (an
/// abort in `~CPluginFactory`); with it, none of 100 runs failed. Fewer loops
/// detect less: 4 x 20,000 killed 19 of 60 runs, 8 x 100 only 2 of 200.
///
/// Mutation: remove the lock in `Module::drop` and the process aborts.
#[test]
fn concurrent_opens_and_drops_of_one_image_do_not_corrupt_the_plugins_globals() {
    const THREADS: usize = 3;
    const LOOPS: usize = 400_000;
    let path = probe_path();
    let start = std::sync::Barrier::new(THREADS);
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                start.wait();
                for _ in 0..LOOPS {
                    let module = Module::open(&path).expect("the probe opens");
                    assert!(module.class_count() > 0);
                }
            });
        }
    });
}

/// An open that fails after the library is loaded releases the lock, so the
/// next open on the same thread does not wait on itself.
///
/// The library is a system one with no `GetPluginFactory`, so `open` drops what
/// it built (the library, the module entry) on its error path while it holds
/// the lock. `open` builds a `Module` only as its last step; one dropped on
/// that path would wait on the lock it holds. The check runs on a worker so a
/// self-deadlock fails the test instead of hanging it.
///
/// Mutation (run): taking the lock a second time in `open` stops the worker
/// from answering and the test fails after its 30 s wait.
#[test]
fn a_failed_open_does_not_deadlock_the_next() {
    #[cfg(target_os = "linux")]
    const NOT_A_PLUGIN: &str = "libm.so.6";
    #[cfg(target_os = "macos")]
    const NOT_A_PLUGIN: &str = "/usr/lib/libSystem.B.dylib";
    #[cfg(windows)]
    const NOT_A_PLUGIN: &str = "kernel32.dll";

    let path = probe_path();
    let (done, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let refused = Module::open(std::path::Path::new(NOT_A_PLUGIN)).is_err();
        let opened = Module::open(&path).is_ok();
        let _ = done.send((refused, opened));
    });
    let (refused, opened) = answer
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the opens finished: a failed open did not release the lock");
    assert!(refused, "a system library is not a VST3 module");
    assert!(opened);
}

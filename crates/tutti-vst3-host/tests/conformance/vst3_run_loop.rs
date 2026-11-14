//! What the host does with the run-loop timers and file descriptors a plugin
//! leaves behind, and with a plugin that never initialized.
//!
//! The witness is the shared test plugin (`testing/tutti-shared-plugin`), whose
//! VST3 shell registers a timer and a file descriptor with the host's run loop
//! when its view is attached, the way VSTGUI does on Linux (through the
//! `IRunLoop` of the host context the factory was given), and in its leaky mode
//! never unregisters them. The mode is a byte of the plugin's state, set the way
//! the contract sets a mode: save, patch, load.

#![cfg(all(target_os = "linux", feature = "conformance"))]

use std::sync::Arc;
use std::time::Duration;
use tutti_fixtures::main_thread;

use tutti_plugin_types::{ContentScale, Interest, MainThreadMarker, Ready, Wakeups};
use tutti_vst3_ffi::{Module, Plugin};

/// `core::EDITOR_*`, from the plugin's spec.
const TIDY: u32 = 1;
const LEAKY: u32 = 2;
const FAILING: u32 = 3;
const LATE: u32 = 4;

fn module() -> Arc<Module> {
    let library = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    Module::open(&library).expect("the shared plugin loads")
}

fn create(module: &Arc<Module>) -> Plugin {
    create_for(module, &tutti_fixtures::detached_work())
}

/// A plugin whose off-thread drop is handed to `work`.
fn create_for(module: &Arc<Module>, work: &tutti_plugin_types::MainWorkHandle) -> Plugin {
    let class = module
        .classes()
        .find(|c| c.category.contains("Audio"))
        .expect("an audio class");
    module
        .create_plugin(tutti_fixtures::main_thread(), work, &class.cid)
        .expect("a plugin")
}

/// A plugin initialized, with its editor behaviour set through its state.
fn plugin_with_editor(module: &Arc<Module>, editor: u32) -> Plugin {
    plugin_with_editor_for(module, editor, &tutti_fixtures::detached_work())
}

/// [`plugin_with_editor`], whose off-thread drop is handed to `work`.
fn plugin_with_editor_for(
    module: &Arc<Module>,
    editor: u32,
    work: &tutti_plugin_types::MainWorkHandle,
) -> Plugin {
    let mut plugin = create_for(module, work);
    plugin
        .initialize_component(main_thread())
        .expect("initialize");
    let mut blob = plugin.component_state(main_thread()).expect("state");
    // The second byte of the mode word, at offset 4.
    blob[4..8].copy_from_slice(&(editor << 8).to_le_bytes());
    plugin
        .set_component_state(main_thread(), &blob)
        .expect("load");
    plugin
}

fn try_open(
    plugin: &mut Plugin,
    main: MainThreadMarker,
) -> Result<Option<dpi::Size>, tutti_vst3_ffi::EditorError> {
    // The plugin ignores the handle; it is never dereferenced.
    let parent = tutti_fixtures::StandInWindow.handle();
    plugin.open_editor(main, parent, ContentScale::default())
}

fn open(plugin: &mut Plugin, main: MainThreadMarker) {
    try_open(plugin, main).expect("the editor opens");
}

fn registered(plugin: &Plugin) -> (usize, usize) {
    let a = plugin.run_loop_activity();
    (a.timers_registered, a.event_handlers_registered)
}

/// **A tidy view's registrations are the run loop's while the editor is open,
/// and gone after.** The control: the plugin registers through the module's
/// host context, and the host sees them.
/// Spec: vst3.IRunLoop.registerEventHandler, vst3.IRunLoop.registerTimer, vst3.IRunLoop.unregisterEventHandler, vst3.IRunLoop.unregisterTimer
#[test]
fn a_tidy_editor_registers_and_unregisters() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = plugin_with_editor(&module, TIDY);
    open(&mut plugin, main);
    assert_eq!(registered(&plugin), (1, 1));
    plugin.close_editor(main);
    assert_eq!(registered(&plugin), (0, 0));
}

/// **The run loop is described to the host's wait, and what the wait found is
/// run.** A tidy view registers a 1 ms timer and a descriptor. The description
/// has the descriptor (for reading) and the timer's deadline; a wake just before
/// the deadline runs nothing, one at it runs the timer, and a descriptor runs its
/// handler only when it is reported readable (not when it is reported writable).
#[test]
fn the_run_loop_is_described_and_what_the_wait_found_is_run() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = plugin_with_editor(&module, TIDY);
    open(&mut plugin, main);

    let mut wakeups = Wakeups::new();
    plugin.editor_wakeups(main, &mut wakeups);
    let [watch] = wakeups.watches() else {
        panic!("one descriptor registered: {:?}", wakeups.watches());
    };
    assert_eq!(watch.interest, Interest::READ);
    let deadline = wakeups.deadline().expect("one timer registered");
    let fd = watch.fd;

    plugin.wake_editor_loop(main, &[], deadline - Duration::from_micros(1));
    assert_eq!(plugin.run_loop_activity().timers_fired, 0, "not due yet");
    plugin.wake_editor_loop(main, &[], deadline);
    assert_eq!(
        plugin.run_loop_activity().timers_fired,
        1,
        "due at its deadline"
    );

    let writable = Ready {
        fd,
        events: Interest::WRITE,
    };
    plugin.wake_editor_loop(main, &[writable], deadline);
    assert_eq!(plugin.run_loop_activity().fds_dispatched, 0, "not readable");
    let readable = Ready {
        fd,
        events: Interest::READ,
    };
    plugin.wake_editor_loop(main, &[readable], deadline);
    assert_eq!(plugin.run_loop_activity().fds_dispatched, 1, "readable");

    plugin.close_editor(main);
    let mut after = Wakeups::new();
    plugin.editor_wakeups(main, &mut after);
    assert!(
        after.is_empty(),
        "nothing left to wait on once the editor closed"
    );
}

/// **A timer and a descriptor a plugin leaves registered when its editor closes
/// are not called afterwards.** A VSTGUI frame that fails to unregister leaves
/// raw pointers into a view the host has released; the next `run_iteration`,
/// for any plugin of the module, would call them.
#[test]
fn a_leaked_registration_is_purged_when_the_editor_closes() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = plugin_with_editor(&module, LEAKY);
    open(&mut plugin, main);
    assert_eq!(registered(&plugin), (1, 1));
    plugin.close_editor(main);
    assert_eq!(registered(&plugin), (0, 0), "the leak was taken back");

    std::thread::sleep(Duration::from_millis(5));
    plugin.run_editor_loop_iteration(main);
    let activity = plugin.run_loop_activity();
    assert_eq!(
        activity.timers_fired, 0,
        "nothing of the closed editor runs"
    );
    assert_eq!(activity.fds_dispatched, 0);
}

/// **A plugin that leaks and is dropped with its editor open does not leave
/// its registrations to the module's other plugins.**
#[test]
fn a_dropped_plugin_leaves_nothing_in_the_run_loop() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut leaky = plugin_with_editor(&module, LEAKY);
    open(&mut leaky, main);
    let bystander = create(&module);
    assert_eq!(registered(&bystander), (1, 1), "one loop for the module");
    drop(leaky);
    assert_eq!(registered(&bystander), (0, 0));
    std::thread::sleep(Duration::from_millis(5));
    bystander.run_editor_loop_iteration(main);
    assert_eq!(bystander.run_loop_activity().timers_fired, 0);
}

/// **A view that registers and then fails `attached` leaves nothing in the run
/// loop.** `open_editor` returns the error; the registration the view made
/// before failing has no editor to be closed with.
#[test]
fn a_failed_attach_leaves_nothing_in_the_run_loop() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = plugin_with_editor(&module, FAILING);
    assert!(try_open(&mut plugin, main).is_err());
    assert_eq!(registered(&plugin), (0, 0));
    std::thread::sleep(Duration::from_millis(5));
    plugin.run_editor_loop_iteration(main);
    assert_eq!(plugin.run_loop_activity().timers_fired, 0);
}

/// **A registration made in a later call (a toolkit's first layout, in
/// `onSize`) is the plugin's, and goes when its editor closes.** It arrives
/// through the module's own host context, which names no owner; the scope
/// every token-taking call runs under is what names one.
#[test]
fn a_registration_from_a_later_call_is_purged_on_close() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = plugin_with_editor(&module, LATE);
    open(&mut plugin, main);
    assert_eq!(registered(&plugin), (0, 0), "attached registered nothing");
    plugin
        .resize_editor(main, dpi::PhysicalSize::new(300, 200).into())
        .expect("resize");
    assert_eq!(registered(&plugin), (1, 1), "onSize registered");
    plugin.close_editor(main);
    assert_eq!(registered(&plugin), (0, 0));
}

/// **A class whose `initialize` fails is not terminated.** The shared plugin's
/// second class refuses `initialize`, and aborts the process on `terminate`
/// without a successful one.
#[test]
fn a_plugin_whose_initialize_failed_is_not_terminated() {
    let module = module();
    let class = module
        .classes()
        .find(|c| c.name.contains("refused"))
        .expect("the class that refuses initialize");
    let mut plugin = module
        .create_plugin(
            tutti_fixtures::main_thread(),
            &tutti_fixtures::detached_work(),
            &class.cid,
        )
        .expect("a plugin");
    assert!(plugin.initialize_component(main_thread()).is_err());
    drop(plugin);
}

/// **A plugin that never initialized is not terminated.** The SDK allows
/// `terminate` after a successful `initialize` only, and the shared plugin
/// aborts the process on any other.
#[test]
fn a_plugin_never_initialized_is_not_terminated() {
    let module = module();
    drop(create(&module));
}

/// **A plugin dropped off the main thread with its editor open is torn down on
/// the main thread, later, not leaked.** A view belongs to the thread that
/// attached it, and `setActive(0)`/`terminate` are main-thread calls (the
/// shared plugin aborts the process if `removed` runs off the attaching thread
/// or `terminate` off the initializing one, as a real plugin may crash).
/// `Drop` has no token, so the plugin is handed to its owner's collector
/// (`MainWork`, as the CLAP and VST2 hosts do): still registered on the run loop
/// after the drop, torn down at the collector's next run on the main thread.
#[test]
fn a_plugin_dropped_off_the_main_thread_is_torn_down_on_it_later() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let (work, handle) = tutti_plugin_types::MainWork::new();
    let mut plugin = plugin_with_editor_for(&module, TIDY, &handle);
    open(&mut plugin, main);
    let bystander = create(&module);
    assert_eq!(registered(&bystander), (1, 1));
    std::thread::spawn(move || drop(plugin)).join().unwrap();
    assert_eq!(
        registered(&bystander),
        (1, 1),
        "nothing was torn down on the dropping thread"
    );
    assert_eq!(work.run(main), 1);
    assert_eq!(
        registered(&bystander),
        (0, 0),
        "the main thread detached it, and the run loop is purged"
    );
}

/// **`activate_bus` reports the plugin's refusal.** The shared plugin has the
/// one bus of each kind, at index 0, and refuses any other index; the result
/// used to be dropped and `Ok(())` returned.
/// Spec: vst3.IComponent.activateBus
#[test]
fn activate_bus_reports_a_refusal() {
    let main = tutti_fixtures::main_thread();
    let module = module();
    let mut plugin = create(&module);
    plugin.initialize_component(main).expect("initialize");
    use tutti_vst3_ffi::{BusDirection, MediaType};
    assert!(
        plugin
            .activate_bus(main, MediaType::Audio, BusDirection::Input, 0)
            .is_ok()
    );
    assert!(
        plugin
            .activate_bus(main, MediaType::Audio, BusDirection::Input, 5)
            .is_err()
    );
}

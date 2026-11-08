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

use tutti_plugin_types::{ContentScale, MainThread, WindowHandle};
use tutti_vst3_ffi::{Module, Plugin};

/// `core::EDITOR_TIDY` and `core::EDITOR_LEAKY`, from the plugin's spec.
const TIDY: u32 = 1;
const LEAKY: u32 = 2;

fn module() -> Arc<Module> {
    let library = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    Module::open(&library).expect("the shared plugin loads")
}

fn create(module: &Arc<Module>) -> Plugin {
    let class = module
        .classes()
        .find(|c| c.category.contains("Audio"))
        .expect("an audio class");
    module.create_plugin(&class.cid).expect("a plugin")
}

/// A plugin initialized, with its editor behaviour set through its state.
fn plugin_with_editor(module: &Arc<Module>, editor: u32) -> Plugin {
    let mut plugin = create(module);
    plugin.initialize_component().expect("initialize");
    let mut blob = plugin.component_state().expect("state");
    // The second byte of the mode word, at offset 4.
    blob[4..8].copy_from_slice(&(editor << 8).to_le_bytes());
    plugin.set_component_state(&blob).expect("load");
    plugin
}

fn open(plugin: &mut Plugin, main: &MainThread) {
    // SAFETY: the plugin ignores the handle; it is never dereferenced.
    let parent = unsafe { WindowHandle::from_raw(std::ptr::dangling_mut()) };
    plugin
        .open_editor(main, parent, ContentScale::default())
        .expect("the editor opens");
}

fn registered(plugin: &Plugin) -> (usize, usize) {
    let a = plugin.run_loop_activity();
    (a.timers_registered, a.event_handlers_registered)
}

/// **A tidy view's registrations are the run loop's while the editor is open,
/// and gone after.** The control: the plugin registers through the module's
/// host context, and the host sees them.
#[test]
fn a_tidy_editor_registers_and_unregisters() {
    let main = MainThread::claim().unwrap();
    let module = module();
    let mut plugin = plugin_with_editor(&module, TIDY);
    open(&mut plugin, &main);
    assert_eq!(registered(&plugin), (1, 1));
    plugin.close_editor(&main);
    assert_eq!(registered(&plugin), (0, 0));
}

/// **A timer and a descriptor a plugin leaves registered when its editor closes
/// are not called afterwards.** A VSTGUI frame that fails to unregister leaves
/// raw pointers into a view the host has released; the next `run_iteration`,
/// for any plugin of the module, would call them.
///
/// Mutation: drop the `purge_run_loop` call in `Plugin::release_editor` -> the
/// registrations stay (the first assertion fails) and the leaked timer fires.
#[test]
fn a_leaked_registration_is_purged_when_the_editor_closes() {
    let main = MainThread::claim().unwrap();
    let module = module();
    let mut plugin = plugin_with_editor(&module, LEAKY);
    open(&mut plugin, &main);
    assert_eq!(registered(&plugin), (1, 1));
    plugin.close_editor(&main);
    assert_eq!(registered(&plugin), (0, 0), "the leak was taken back");

    std::thread::sleep(Duration::from_millis(5));
    plugin.run_editor_loop_iteration(&main);
    let activity = plugin.run_loop_activity();
    assert_eq!(
        activity.timers_fired, 0,
        "nothing of the closed editor runs"
    );
    assert_eq!(activity.fds_dispatched, 0);
}

/// **A plugin that leaks and is dropped with its editor open does not leave
/// its registrations to the module's other plugins.**
///
/// Mutation: drop the `purge_run_loop` calls (in `release_editor` and in
/// `Plugin::drop`) -> the second plugin sees (1, 1) and its `run_iteration`
/// fires the dead plugin's timer.
#[test]
fn a_dropped_plugin_leaves_nothing_in_the_run_loop() {
    let main = MainThread::claim().unwrap();
    let module = module();
    let mut leaky = plugin_with_editor(&module, LEAKY);
    open(&mut leaky, &main);
    let bystander = create(&module);
    assert_eq!(registered(&bystander), (1, 1), "one loop for the module");
    drop(leaky);
    assert_eq!(registered(&bystander), (0, 0));
    std::thread::sleep(Duration::from_millis(5));
    bystander.run_editor_loop_iteration(&main);
    assert_eq!(bystander.run_loop_activity().timers_fired, 0);
}

/// **A plugin that never initialized is not terminated.** The SDK allows
/// `terminate` after a successful `initialize` only, and the shared plugin
/// aborts the process on any other.
///
/// Mutation: drop the `component_initialized` check around
/// `self.component.terminate()` in `Plugin::drop` -> the plugin aborts and the
/// test process dies.
#[test]
fn a_plugin_never_initialized_is_not_terminated() {
    let module = module();
    drop(create(&module));
}

/// **A plugin dropped off the main thread with its editor open leaves the view
/// alone.** A view belongs to the thread that attached it (the shared plugin's
/// aborts the process if `removed` runs elsewhere, as a toolkit may crash), and
/// `Drop` has no token to prove the thread. It detaches only where
/// `MainThread::current` finds one, and leaks the view otherwise; the run loop
/// is purged either way.
///
/// Mutation: make `release_editor` close the editor whatever `main` is ->
/// `removed` runs on the spawned thread and the plugin aborts the test.
#[test]
fn a_plugin_dropped_off_the_main_thread_is_not_detached_there() {
    let main = MainThread::claim().unwrap();
    let module = module();
    let mut plugin = plugin_with_editor(&module, TIDY);
    open(&mut plugin, &main);
    let bystander = create(&module);
    assert_eq!(registered(&bystander), (1, 1));
    std::thread::spawn(move || drop(plugin)).join().unwrap();
    assert_eq!(registered(&bystander), (0, 0), "purged though not detached");
}

//! Plugins hosted in this process, through the session, on the process's main thread.
//!
//! A plugin hosted in process is driven from the main thread: a call through its handle
//! from any other thread hops to the session's update, and a fork for an export is made
//! there. `MainThreadMarker::new` asks the OS, and the default harness runs every test on a
//! thread it spawns, so this target sets `harness = false` and owns `main()`, which is the
//! main thread (`tutti_fixtures::main_thread_cases` answers the command line nextest
//! drives it with). The reference plugins load in process; a missing server is a failure,
//! never a skip.

#[cfg(feature = "clap")]
#[path = "../support/mod.rs"]
mod support;

// The handle of an in-process VST2 answers a pool thread while the main thread updates.
#[cfg(feature = "vst2")]
mod vst2_off_main_handle;
// Modulation lanes on the reference VST2 plugin's parameters.
#[cfg(all(feature = "vst2", feature = "modulation"))]
mod vst2_modulation;
// `typed` over a real format's own value map: the reference VST2 plugin's
// integer-ranged parameter, in process.
#[cfg(feature = "vst2")]
mod vst2_params;
// A hosted plugin's own event sources, served by the view's update: the
// reference CLAP plugin in process, with an editor open (an X11 window id).
#[cfg(all(feature = "clap", unix, not(target_os = "macos")))]
mod plugin_event_sources;
#[cfg(not(all(feature = "clap", unix, not(target_os = "macos"))))]
mod plugin_event_sources {
    /// No case here: the editor embeds into an X11 window id.
    pub(crate) const CASES: &[(&str, fn())] = &[];
}
// A hosted plugin exported through a fork of its own server: fails without
// `plugin-server`, by design.
#[cfg(all(feature = "export", feature = "clap"))]
mod export_fork_plugin;

fn main() {
    let mut cases: Vec<(&str, fn())> = Vec::new();
    #[cfg(feature = "vst2")]
    cases.extend_from_slice(vst2_off_main_handle::CASES);
    #[cfg(all(feature = "vst2", feature = "modulation"))]
    cases.extend_from_slice(vst2_modulation::CASES);
    #[cfg(feature = "vst2")]
    cases.extend_from_slice(vst2_params::CASES);
    cases.extend_from_slice(plugin_event_sources::CASES);
    #[cfg(all(feature = "export", feature = "clap"))]
    cases.extend_from_slice(export_fork_plugin::CASES);
    tutti_fixtures::main_thread_cases(&cases);
}

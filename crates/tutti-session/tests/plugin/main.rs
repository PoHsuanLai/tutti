//! Hosted plugins through the session: latency, bypass, health, modulation and exports.

#[cfg(feature = "clap")]
#[path = "../support/mod.rs"]
mod support;

// Hosted plugins, their render mode and exports, over the reference CLAP
// plugin in a real `plugin-server`: fails without the server, by design.
#[cfg(all(feature = "clap", feature = "export"))]
mod plugin;
// A hosted plugin's bypass is the graph's `NodeMode` (doc 013 M2's host half):
// the reference CLAP plugin in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_bypass;
// Which plugin of a bundle a load opens, and whose state a restore accepts:
// the reference CLAP bundle exports two, in a real `plugin-server`.
#[cfg(feature = "clap")]
mod plugin_identity;
// What `Phase::Plugins` does with a hosted plugin's health, over a fake
// backend (no `plugin-server`): the session half of `bevy-tutti`'s
// `plugin_host::health` tests.
#[cfg(feature = "plugin")]
mod plugin_health;
// A hosted plugin's latency is the session's, across a crossfade and a
// restart (the session halves of `bevy-tutti`'s `plugin_capture` and restart
// tests): the reference CLAP plugin in a real `plugin-server`.
#[cfg(all(feature = "clap", feature = "device"))]
mod plugin_latency;
// Modulation lanes on a hosted plugin's parameters: across a recovery and a
// crossfade, and an export equal to the live render (the session halves of
// `bevy-tutti`'s `plugin_recovery`, `plugin_capture` and `mod_plugin`).
#[cfg(all(feature = "clap", feature = "modulation", feature = "export"))]
mod plugin_modulation;
// A hosted plugin exported through a fork of its own server: fails without
// `plugin-server`, by design.
#[cfg(all(feature = "export", feature = "clap"))]
mod export_fork_plugin;
// Work handed to the main thread runs on the session's update.
#[cfg(feature = "plugin")]
mod deferred_main;
#[cfg(feature = "vst2")]
mod vst2_meter;
#[cfg(all(feature = "vst2", feature = "modulation"))]
mod vst2_modulation;
// The reference VST2 plugin, in process: the project meter it is given, and
// modulation lanes on its parameters (the session halves of `bevy-tutti`'s
// `plugin_vst2_meter` and `mod_plugin` VST2 tests).
#[cfg(feature = "vst2")]
mod vst2_off_main_handle;

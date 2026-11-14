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
// What a load asks of the catalog (a blacklisted plugin is refused at load) and
// of the ref's `Hosting`: the reference CLAP plugin.
#[cfg(feature = "clap")]
mod plugin_hosting;
// What `Phase::Plugins` does with a hosted plugin's health, over a fake
// backend (no `plugin-server`).
#[cfg(feature = "plugin")]
mod plugin_health;
// A hosted plugin's latency is the session's, across a crossfade and a
// restart: the reference CLAP plugin in a real `plugin-server`.
#[cfg(all(feature = "clap", feature = "device", feature = "export"))]
mod plugin_latency;
// Modulation lanes on a hosted plugin's parameters: across a recovery and a
// crossfade, and an export equal to the live render.
#[cfg(all(feature = "clap", feature = "modulation", feature = "export"))]
mod plugin_modulation;
// The main-thread view: editors serviced by its update, the sizes they ask
// for, and the graveyard it empties; a fake plugin with an editor.
#[cfg(feature = "plugin")]
mod main_view;
// A hosted plugin's parameters through the session's views: gestures, typed units, rescans; a
// fake backend.
#[cfg(feature = "plugin")]
mod plugin_params;
// The reference VST2 plugin, in process: the project meter it is given.
#[cfg(feature = "vst2")]
mod vst2_meter;
// Presets through the main-thread view: a list that loads by id (VST2, in
// process) and a file that loads by path (CLAP, in a real `plugin-server`).
#[cfg(any(feature = "clap", feature = "vst2"))]
mod plugin_presets;
// Diagnostics and buses: the counts either hosting reads the
// same way, an aux input wired through the plugin's bus handles, a server named
// and ended.
#[cfg(any(feature = "clap", feature = "vst2"))]
mod plugin_diagnostics;

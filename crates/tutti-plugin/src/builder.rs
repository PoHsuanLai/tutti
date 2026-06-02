//! Fluent builder for plugin hosts (VST3 / CLAP / AU / VST2).
//!
//! Construct via [`vst3`], [`clap`], [`au`], or [`vst2`] (each behind its
//! own crate feature). Each takes a host [`sample_rate`] and a path;
//! [`PluginBuilder::build`] loads the plugin, applies any queued initial
//! parameter values, and returns the audio unit together with its
//! [`PluginHandle`](crate::control_handle::PluginHandle) for main-thread
//! control.
//!
//! VST3, CLAP, and AU run in a subprocess (audio + control IPC) with
//! their editor lazily loaded in the host process — the standard
//! audio-out-of-process / GUI-in-host split.
//!
//! VST2 is different. With the `vst2-in-process` feature enabled (the
//! default once a user opts into editor support), the plugin runs
//! entirely in the host process via `vst2-host`: VST2's `AEffect` fuses
//! editor and audio processor, so the editor cannot live in a different
//! process from audio. Without that feature, VST2 falls back to the
//! subprocess path with `open_editor` returning an error.

use crate::audio_node::PluginClient;
use crate::config::BridgeConfig;
use crate::control_handle::PluginHandle;
use crate::error::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for unique per-plugin socket paths within a process.
static PLUGIN_SOCKET_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Starts a [`PluginBuilder`] for a VST3 plugin bundle.
///
/// ```ignore
/// let (unit, handle) = tutti_plugin::vst3(engine.sample_rate, "Foo.vst3").build()?;
/// let id = engine.graph.add(unit);
/// ```
#[cfg(feature = "vst3")]
pub fn vst3(sample_rate: f64, path: impl Into<PathBuf>) -> PluginBuilder {
    PluginBuilder::new(sample_rate, path.into())
}

/// Starts a [`PluginBuilder`] for a CLAP plugin binary.
#[cfg(feature = "clap")]
pub fn clap(sample_rate: f64, path: impl Into<PathBuf>) -> PluginBuilder {
    PluginBuilder::new(sample_rate, path.into())
}

/// Starts a [`PluginBuilder`] for an Audio Unit `.component` bundle.
#[cfg(feature = "au")]
pub fn au(sample_rate: f64, path: impl Into<PathBuf>) -> PluginBuilder {
    PluginBuilder::new(sample_rate, path.into())
}

/// Starts a [`PluginBuilder`] for a VST2 plugin bundle (`.vst` / `.dll` / `.so`).
///
/// With the `vst2-in-process` feature, the plugin runs entirely in the
/// host process and the editor is fully supported. Without it, audio
/// runs in a subprocess and `open_editor` returns an error.
#[cfg(feature = "vst2")]
pub fn vst2(sample_rate: f64, path: impl Into<PathBuf>) -> PluginBuilder {
    PluginBuilder::new(sample_rate, path.into())
}

/// Fluent builder for out-of-process audio plugin hosts.
///
/// [`Self::build`] returns `(Box<dyn AudioUnit>, PluginHandle)`: add the
/// unit to the graph, keep the handle alive to hold the subprocess open
/// and drive parameter changes. Built synchronously — a short-lived
/// single-thread tokio runtime is spun up internally for the
/// [`PluginClient::new`] await so callers don't have to be in an async
/// context.
pub struct PluginBuilder {
    sample_rate: f64,
    path: PathBuf,
    params: HashMap<String, f32>,
}

impl PluginBuilder {
    #[cfg(any(feature = "vst3", feature = "clap", feature = "au", feature = "vst2"))]
    fn new(sample_rate: f64, path: PathBuf) -> Self {
        Self {
            sample_rate,
            path,
            params: HashMap::new(),
        }
    }

    /// Queues an initial parameter value, keyed by its numeric parameter
    /// id encoded as a string. Non-numeric keys are ignored at build time.
    pub fn param(mut self, name: impl Into<String>, value: f32) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// Loads the plugin (in-process for VST2 with the `vst2-in-process`
    /// feature, subprocess otherwise), applies any queued [`Self::param`]
    /// values, and returns the audio unit together with its [`PluginHandle`].
    pub fn build(self) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
        #[cfg(feature = "vst2-in-process")]
        if matches!(
            self.path.extension().and_then(|s| s.to_str()),
            Some("vst") | Some("VST")
        ) {
            return load_plugin_vst2_in_process(self.sample_rate, self.path, &self.params);
        }

        load_plugin(self.sample_rate, self.path, &self.params)
    }
}

#[cfg(feature = "vst2-in-process")]
fn load_plugin_vst2_in_process(
    sample_rate: f64,
    path: PathBuf,
    params: &HashMap<String, f32>,
) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
    let (unit, handle) = crate::in_process::vst2::load(&path, sample_rate)?;
    for (name, value) in params {
        if let Ok(param_id) = name.parse::<u32>() {
            handle.set_parameter(param_id, *value);
        }
    }
    Ok((unit, handle))
}

/// Load a plugin out-of-process via `tutti-plugin-server`.
/// The plugin runs in a child process; crashes are isolated.
fn load_plugin(
    sample_rate: f64,
    path: PathBuf,
    params: &HashMap<String, f32>,
) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
    let counter = PLUGIN_SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
    let socket_path = std::env::temp_dir().join(format!(
        "tutti-plugin-{}-{}.sock",
        std::process::id(),
        counter,
    ));

    let config = BridgeConfig {
        socket_path,
        ..BridgeConfig::default()
    };

    let client = PluginClient::new(config, path, sample_rate)?;

    for (name, value) in params {
        if let Ok(param_id) = name.parse::<u32>() {
            client.set_parameter(param_id, *value);
        }
    }

    let plugin_handle = PluginHandle::from_client(&client);
    Ok((Box::new(client), plugin_handle))
}

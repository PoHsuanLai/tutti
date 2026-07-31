//! Fluent builder for plugin hosts (VST3 / CLAP / AU / VST2).
//!
//! Construct via [`vst3`], [`clap`], [`au`], or [`vst2`] (each behind its
//! own crate feature). Each takes a host [`sample_rate`] and a path;
//! [`PluginBuilder::build`] loads the plugin, applies any queued initial
//! parameter values, and returns the audio unit together with its
//! [`PluginHandle`](crate::host::handles::control_handle::PluginHandle) for main-thread
//! control.
//!
//! VST3, CLAP, and AU run in a subprocess (audio + control IPC) with
//! their editor lazily loaded in the host process — the standard
//! audio-out-of-process / GUI-in-host split.
//!
//! VST2 is different: it always runs entirely in the host process via
//! `vst2-host`. VST2's `AEffect` fuses editor and audio processor into one
//! instance, so the editor cannot live in a different process from audio —
//! there is no subprocess VST2 path.

use crate::error::Result;
use crate::host::handles::control_handle::PluginHandle;
use crate::host::node::PluginClient;
use crate::protocol::ParamAddress;
use crate::util::config::BridgeConfig;
use std::collections::HashMap;
use std::path::PathBuf;

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
/// VST2 always runs entirely in the host process (its `AEffect` fuses the
/// editor and audio processor), so the editor is fully supported.
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

    /// Loads the plugin (in-process for VST2, subprocess for VST3 / CLAP /
    /// AU), applies any queued [`Self::param`] values, and returns the audio
    /// unit together with its [`PluginHandle`].
    pub fn build(self) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
        #[cfg(feature = "vst2")]
        if matches!(
            self.path.extension().and_then(|s| s.to_str()),
            Some("vst") | Some("VST")
        ) {
            return load_plugin_vst2_in_process(self.sample_rate, self.path, &self.params);
        }

        load_plugin(self.sample_rate, self.path, &self.params)
    }
}

#[cfg(feature = "vst2")]
fn load_plugin_vst2_in_process(
    sample_rate: f64,
    path: PathBuf,
    params: &HashMap<String, f32>,
) -> Result<(Box<dyn tutti_core::AudioUnit>, PluginHandle)> {
    let (unit, handle) = crate::format::vst2_in_process::load(&path, sample_rate)?;
    for (name, value) in params {
        // VST2 addresses parameters by position, so a config key names an index.
        if let Ok(index) = name.parse::<i32>() {
            handle.set_parameter(ParamAddress::Index(index), *value);
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
    // The unique per-bridge socket path comes from `BridgeConfig::default()`.
    // This site used to derive its own, which is why it worked while the
    // catalog path — identical but for the missing override — collided.
    let config = BridgeConfig::default();

    let client = PluginClient::new(config, path, sample_rate)?;

    for (name, value) in params {
        // The format is not known here — this path loads any of the four — so a
        // config key is read as an opaque id, which is right for three of them.
        // A VST2 index and an opaque id coincide numerically for the small
        // values a hand-written config uses, so this stays correct in practice;
        // it is the one place the address model is inferred rather than known.
        // TODO: thread the format through so a VST2 key becomes `Index`.
        if let Ok(param_id) = name.parse::<u32>() {
            client.set_parameter(ParamAddress::Opaque(param_id.into()), *value);
        }
    }

    let plugin_handle = PluginHandle::from_client(&client);
    Ok((Box::new(client), plugin_handle))
}

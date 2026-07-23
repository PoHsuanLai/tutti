//! Format-polymorphic plugin wrapper.
//!
//! A single [`Plugin`] enum hides which format-host crate is behind each
//! loaded plugin. All cfg-gating and file-extension dispatch lives here
//! (`probe`, `load`); the server code above sees only a uniform
//! `&mut dyn PluginInstance`.
//!
//! [`poll_async_events`] folds the format-specific host-callback fan-out
//! (VST2 parameter changes, CLAP runtime-latency notifications) into one
//! list the server can drain after each audio block.

use std::path::Path;
use tutti_plugin::server::{
    Features, LoadedPlugin, PluginDescriptor, PluginInstance, SampleFormat,
};
use tutti_plugin::{BridgeError, LoadStage, Result};

#[cfg(feature = "vst2")]
use crate::loaders::vst2::Vst2Instance;

#[cfg(feature = "vst3")]
use crate::loaders::vst3::Vst3Instance;

#[cfg(feature = "clap")]
use crate::loaders::clap::ClapInstance;

#[cfg(all(feature = "au", target_os = "macos"))]
use crate::loaders::au::AuInstance;

#[allow(clippy::large_enum_variant)]
pub(crate) enum Plugin {
    #[cfg(feature = "vst2")]
    Vst2(Vst2Instance),
    #[cfg(feature = "vst3")]
    Vst3(Vst3Instance),
    #[cfg(feature = "clap")]
    Clap(ClapInstance),
    #[cfg(all(feature = "au", target_os = "macos"))]
    Au(AuInstance),
}

/// Events a plugin can queue between audio blocks that the server needs
/// to forward to the host out-of-band of the per-block reply.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants are constructed only under vst2/clap/vst3 features.
pub(crate) enum AsyncEvent {
    ParameterChanged {
        index: i32,
        value: f32,
    },
    LatencyChanged {
        samples: usize,
    },
    /// Plugin changed its own parameter values at runtime (e.g. preset load).
    /// The client should re-read parameter values.
    ParamValuesChanged,
    /// Plugin changed parameter titles/units/flags. The client should re-pull
    /// the parameter list.
    ParamTitlesChanged,
    /// Plugin's bus arrangement changed and was re-enumerated. The client
    /// should rewire its audio graph from the refreshed metadata.
    IoChanged,
    /// Plugin was torn down and rebuilt in place (`kReloadComponent`). The
    /// client should resync everything — it is effectively a fresh instance.
    Reloaded,
}

impl Plugin {
    /// Probe metadata without a full load. VST2 has no lightweight probe
    /// path in the underlying crate; we fall through to `load` and drop
    /// the instance, which matches the previous behavior.
    #[allow(unreachable_code, unused_variables)]
    pub(crate) fn probe(path: &Path) -> Result<PluginDescriptor> {
        if !path.exists() {
            return Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Scanning,
                reason: "Plugin not found".to_string(),
            });
        }

        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match extension.to_lowercase().as_str() {
            #[cfg(feature = "vst3")]
            "vst3" => Vst3Instance::probe(path),
            #[cfg(feature = "clap")]
            "clap" => ClapInstance::probe(path),
            #[cfg(all(feature = "au", target_os = "macos"))]
            "component" => AuInstance::probe(path),
            #[cfg(feature = "vst2")]
            "vst" | "dll" | "so" => {
                let vst = Vst2Instance::load(path, 44100.0, 512)?;
                Ok(vst.descriptor().clone())
            }
            _ => Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Unsupported format: .{extension}"),
            }),
        }
    }

    /// Fully load and instantiate a plugin. Returns the wrapped plugin, its
    /// catalog descriptor, its runtime load data, and the negotiated sample
    /// format (which already accounts for the plugin's declared f64 support,
    /// and for VST3 rejecting the f64 setup call).
    #[allow(unreachable_code, unused_variables)]
    pub(crate) fn load(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        preferred_format: SampleFormat,
    ) -> Result<(Self, PluginDescriptor, LoadedPlugin, SampleFormat)> {
        if !path.exists() {
            return Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Scanning,
                reason: "Plugin not found".to_string(),
            });
        }

        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let plugin: Plugin = match extension.to_lowercase().as_str() {
            #[cfg(feature = "vst3")]
            "vst3" => {
                let prefer_f64 = preferred_format == SampleFormat::Float64;
                Plugin::Vst3(Vst3Instance::load(
                    path,
                    sample_rate,
                    block_size,
                    prefer_f64,
                )?)
            }

            #[cfg(feature = "vst2")]
            "vst" | "dll" | "so" => {
                Plugin::Vst2(Vst2Instance::load(path, sample_rate, block_size)?)
            }

            #[cfg(feature = "clap")]
            "clap" => Plugin::Clap(ClapInstance::load(path, sample_rate, block_size)?),

            #[cfg(all(feature = "au", target_os = "macos"))]
            "component" => Plugin::Au(AuInstance::load(path, sample_rate, block_size)?),

            _ => {
                return Err(BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Opening,
                    reason: format!(
                        "Unsupported plugin format: {extension}. \
                         Supported: .vst3, .vst/.dll/.so (VST2), .clap, .component (AU). \
                         WASM plugins are loaded in-process via the `tutti-wasm-plugin` crate (`tutti_wasm_plugin::load`)."
                    ),
                });
            }
        };

        let descriptor = plugin.instance().descriptor().clone();
        let loaded = plugin.instance().loaded().clone();

        let negotiated = if preferred_format == SampleFormat::Float64
            && loaded.features.contains(Features::F64_AUDIO)
        {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        };
        Ok((plugin, descriptor, loaded, negotiated))
    }

    #[allow(dead_code)] // used by `#[cfg(feature = "clap")]` tests
    pub(crate) fn instance(&self) -> &dyn PluginInstance {
        match self {
            #[cfg(feature = "vst2")]
            Plugin::Vst2(p) => p,
            #[cfg(feature = "vst3")]
            Plugin::Vst3(p) => p,
            #[cfg(feature = "clap")]
            Plugin::Clap(p) => p,
            #[cfg(all(feature = "au", target_os = "macos"))]
            Plugin::Au(p) => p,
            #[cfg(not(any(
                feature = "vst2",
                feature = "vst3",
                feature = "clap",
                all(feature = "au", target_os = "macos"),
            )))]
            _ => unreachable!("no plugin format features enabled"),
        }
    }

    pub(crate) fn instance_mut(&mut self) -> &mut dyn PluginInstance {
        match self {
            #[cfg(feature = "vst2")]
            Plugin::Vst2(p) => p,
            #[cfg(feature = "vst3")]
            Plugin::Vst3(p) => p,
            #[cfg(feature = "clap")]
            Plugin::Clap(p) => p,
            #[cfg(all(feature = "au", target_os = "macos"))]
            Plugin::Au(p) => p,
            #[cfg(not(any(
                feature = "vst2",
                feature = "vst3",
                feature = "clap",
                all(feature = "au", target_os = "macos"),
            )))]
            _ => unreachable!("no plugin format features enabled"),
        }
    }

    /// Drain host-callback events queued since the previous poll.
    ///
    /// Only formats with a real notification mechanism contribute events.
    /// VST3 surfaces runtime latency via `restartComponent(kLatencyChanged)`,
    /// drained here through the host consumer. AU still requires host-side
    /// callback infrastructure that isn't wired yet — see the TODO in
    /// `au-host/src/instance.rs`.
    pub(crate) fn poll_async_events(&mut self) -> Vec<AsyncEvent> {
        #[allow(unused_mut)] // only mutated under vst2/clap/vst3 features
        let mut out = Vec::new();
        match self {
            #[cfg(feature = "vst2")]
            Plugin::Vst2(vst2) => {
                for (index, value) in vst2.poll_parameter_changes() {
                    out.push(AsyncEvent::ParameterChanged { index, value });
                }
            }
            #[cfg(feature = "vst3")]
            Plugin::Vst3(vst3) => {
                let changes = vst3.poll_restart();
                // A reload is a superset resync, so it subsumes the finer-grained
                // param/io signals — emit just Reloaded (plus latency) in that case.
                if changes.reloaded {
                    out.push(AsyncEvent::Reloaded);
                } else {
                    if changes.param_values_changed {
                        out.push(AsyncEvent::ParamValuesChanged);
                    }
                    if changes.param_titles_changed {
                        out.push(AsyncEvent::ParamTitlesChanged);
                    }
                    if changes.io_changed {
                        out.push(AsyncEvent::IoChanged);
                    }
                }
                if let Some(samples) = changes.latency {
                    out.push(AsyncEvent::LatencyChanged { samples });
                }
            }
            #[cfg(feature = "clap")]
            #[allow(clippy::collapsible_match)]
            Plugin::Clap(clap) => {
                if clap.poll_latency_changed() {
                    out.push(AsyncEvent::LatencyChanged {
                        samples: clap.get_latency() as usize,
                    });
                }
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_missing_path_errors() {
        let result = Plugin::probe(Path::new("/nonexistent/plugin.vst3"));
        match result {
            Err(BridgeError::LoadFailed { stage, reason, .. }) => {
                assert_eq!(stage, LoadStage::Scanning);
                assert!(reason.contains("not found"));
            }
            other => panic!("expected LoadFailed/Scanning, got {other:?}"),
        }
    }

    #[test]
    fn load_missing_path_errors() {
        let result = Plugin::load(
            Path::new("/nonexistent/plugin.vst3"),
            44100.0,
            512,
            SampleFormat::Float32,
        );
        match result {
            Err(BridgeError::LoadFailed { stage, reason, .. }) => {
                assert_eq!(stage, LoadStage::Scanning);
                assert!(reason.contains("not found"));
            }
            Err(e) => panic!("expected LoadFailed/Scanning, got {e:?}"),
            Ok(_) => panic!("expected LoadFailed/Scanning, got Ok(_)"),
        }
    }

    #[test]
    fn load_unsupported_extension_errors() {
        let tmp = std::env::temp_dir().join(format!("fake_plugin_{}.xyz", std::process::id()));
        std::fs::write(&tmp, b"fake").unwrap();
        let result = Plugin::load(&tmp, 44100.0, 512, SampleFormat::Float32);
        let _ = std::fs::remove_file(&tmp);
        match result {
            Err(BridgeError::LoadFailed { stage, reason, .. }) => {
                assert_eq!(stage, LoadStage::Opening);
                assert!(reason.contains("Unsupported"));
            }
            Err(e) => panic!("expected LoadFailed/Opening, got {e:?}"),
            Ok(_) => panic!("expected LoadFailed/Opening, got Ok(_)"),
        }
    }
}

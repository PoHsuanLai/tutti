//! Format-polymorphic plugin wrapper.
//!
//! A single [`Plugin`] enum hides which format-host crate is behind each
//! loaded plugin. All cfg-gating and file-extension dispatch lives here
//! (`probe`, `load`); the server code above sees only a uniform
//! `&mut dyn PluginInstance`.
//!
//! [`poll_async_events`] folds the format-specific host-callback fan-out
//! (VST2 parameter changes, VST3 restart flags, CLAP runtime latency *and* tail
//! notifications, AU property changes) into one list the server can drain after
//! each audio block.

use std::path::Path;
use tutti_plugin::server::{
    Features, LoadedPlugin, PluginDescriptor, PluginInstance, PluginTail, SampleFormat, Samples,
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

/// One loaded plugin, whichever format backs it.
///
/// Every variant is behind its format's feature, and AU additionally behind
/// macOS — so a build with no format features enabled has an uninhabited enum,
/// which is why the accessors below carry an `unreachable!` arm.
///
/// Reach for [`Plugin::instance_mut`] rather than matching: the point of the
/// enum is that callers above it see only `&mut dyn PluginInstance`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Plugin {
    /// A VST2 plugin (`.vst`, `.dll`, `.so`).
    #[cfg(feature = "vst2")]
    Vst2(Vst2Instance),
    /// A VST3 plugin (`.vst3`).
    #[cfg(feature = "vst3")]
    Vst3(Vst3Instance),
    /// A CLAP plugin (`.clap`).
    #[cfg(feature = "clap")]
    Clap(ClapInstance),
    /// An Audio Unit (`.component`). macOS only.
    #[cfg(all(feature = "au", target_os = "macos"))]
    Au(AuInstance),
}

/// Events a plugin can queue between audio blocks that the server needs
/// to forward to the host out-of-band of the per-block reply.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants are constructed only under vst2/clap/vst3 features.
pub(crate) enum AsyncEvent {
    /// The plugin moved one of its own parameters — a knob turned in its editor.
    /// VST2 only; the other formats signal this as `ParamValuesChanged`.
    ParameterChanged {
        /// The plugin's own parameter index, not a `ParamAddress`.
        index: i32,
        /// The new value, normalized to `0..1` as VST2 reports it.
        value: f32,
    },
    /// The plugin reported new PDC latency at runtime. The host must re-plan
    /// delay compensation; ignoring it leaves the graph compensating a stale
    /// figure permanently.
    LatencyChanged {
        /// The new latency in `Samples`, freshly read from the plugin rather
        /// than served from the load-time cache.
        samples: Samples,
    },
    /// Plugin reported a new tail length at runtime.
    ///
    /// Dynamic in CLAP and AU, by different routes. `clap.tail` pairs the
    /// plugin's `get` with a host `changed` callback; AU has no dedicated
    /// callback but `kAudioUnitProperty_TailTime` is an ordinary property, so
    /// the generic property listener carries the same signal. Either way the
    /// case is a reverb whose decay is turned up after load. VST3's restart
    /// flags carry no tail member, so there the load-time read is the whole
    /// answer.
    TailChanged {
        /// The new tail length. A bounce sized from a stale value truncates the
        /// decay the user just dialled in.
        tail: PluginTail,
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
    /// Read a plugin's catalog metadata without keeping it loaded. Dispatches
    /// on the file extension.
    ///
    /// VST3, CLAP and AU have real probe paths. **VST2 does not** — the
    /// underlying crate offers none, so this falls through to a full `load` and
    /// drops the instance. Probing a directory of VST2s is correspondingly
    /// expensive, and runs the plugin's own init code.
    ///
    /// # Errors
    ///
    /// Returns `BridgeError::LoadFailed` with `LoadStage::Scanning` if the path
    /// does not exist, or `LoadStage::Opening` if the extension names no format
    /// this build supports.
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
    /// format.
    ///
    /// `preferred_format` is a request, not a setting: the returned format is
    /// `Float64` only when the caller asked for it **and** the plugin advertised
    /// `Features::F64_AUDIO`. VST3 narrows it further by rejecting the f64 setup
    /// call outright. Use the returned format, never the requested one.
    ///
    /// # Errors
    ///
    /// Returns `BridgeError::LoadFailed` with `LoadStage::Scanning` if the path
    /// does not exist, `LoadStage::Opening` for an unsupported extension, or the
    /// format loader's own error if instantiation fails.
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
                         Supported: .vst3, .vst/.dll/.so (VST2), .clap, .component (AU)."
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

    /// The loaded plugin as the format-agnostic trait object.
    ///
    /// # Panics
    ///
    /// Panics if the crate was built with no format feature enabled, which makes
    /// the enum uninhabited and this arm unreachable.
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

    /// The loaded plugin as a mutable, format-agnostic trait object — the
    /// handle the audio path and every request handler work through.
    ///
    /// # Panics
    ///
    /// Panics if the crate was built with no format feature enabled, which makes
    /// the enum uninhabited and this arm unreachable.
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
    /// Only formats with a notification mechanism contribute events. VST3
    /// surfaces runtime latency via `restartComponent(kLatencyChanged)`, CLAP
    /// via `clap_host_latency.changed`, and AU via an `AUEventListener` watching
    /// the three properties whose load-time values would otherwise be frozen.
    /// VST2 has no such mechanism for latency at all — `effIdle`/`initialDelay`
    /// carry no change signal — so its load-time figure is the whole answer.
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
                        samples: Samples(clap.get_latency() as usize),
                    });
                }
                // The tail twin of the latency poll above. Without it a plugin
                // whose decay is raised after load keeps reporting the tail it
                // had at load, and a bounce sized from that truncates the
                // decay the user just dialled in.
                if clap.poll_tail_changed() {
                    out.push(AsyncEvent::TailChanged {
                        tail: PluginTail::from_samples(clap.get_tail()),
                    });
                }
            }
            #[cfg(all(feature = "au", target_os = "macos"))]
            Plugin::Au(au) => {
                let changes = au.poll_changes();
                if let Some(samples) = changes.latency {
                    out.push(AsyncEvent::LatencyChanged { samples });
                }
                if let Some(tail) = changes.tail {
                    out.push(AsyncEvent::TailChanged { tail });
                }
                // `ParamTitlesChanged`, not `ParamValuesChanged`:
                // `kAudioUnitProperty_ParameterList` reports which parameters
                // *exist*, so the client has to re-pull the list rather than
                // re-read values against ids that may no longer be there. A
                // value change on an unchanged list arrives on the parameter
                // listener path instead.
                if changes.param_list_changed {
                    out.push(AsyncEvent::ParamTitlesChanged);
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

    /// An AU to run the property-notification tests below against.
    ///
    /// Deliberately a *bundled* unit, not a named third-party one: nothing these
    /// tests assert is specific to a particular AU — what is under test is the
    /// wiring from a property flag to a re-read — so depending on an installed
    /// plugin would fail every checkout that lacks it.
    ///
    /// `AUPeakLimiter` ships with macOS, so it is present wherever these tests
    /// can run at all. It also reports a non-zero latency (88 at 44.1 kHz),
    /// which most bundled units do not — `AUDelay`, `AUMatrixReverb`,
    /// `AUNBandEQ` and the filters all report zero. That is not what makes the
    /// latency test valid (`poison_cached_latency` does), but it keeps the
    /// figures under test away from a value that coincides with the default.
    ///
    /// `AU_SAMPLE_PLUGIN` overrides the choice for a machine that has something
    /// more interesting installed.
    #[cfg(all(feature = "au", target_os = "macos"))]
    fn au_fixture() -> Option<String> {
        fn loadable(name: &str) -> bool {
            crate::loaders::au::AuInstance::load(Path::new(name), 44100.0, 512).is_ok()
        }

        if let Ok(named) = std::env::var("AU_SAMPLE_PLUGIN") {
            if loadable(&named) {
                return Some(named);
            }
            eprintln!("AU_SAMPLE_PLUGIN={named} would not load; falling back");
        }
        const FALLBACK: &str = "AUPeakLimiter";
        if loadable(FALLBACK) {
            return Some(FALLBACK.to_string());
        }
        eprintln!("no loadable AU found (tried {FALLBACK}); skipping");
        None
    }

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

    /// A CLAP tail change must reach the drained event list.
    ///
    /// The flag-clearing half of this was already covered
    /// (`test_clap_poll_latency_tail`), and it passed for a year while the
    /// signal went nowhere: `poll_async_events` polled latency and never
    /// polled tail, so a plugin that raised its decay had the notification
    /// consumed and dropped. Asserting the *event* rather than the flag is
    /// what makes that unrepresentable — a test on `poll_tail_changed` alone
    /// cannot tell a wired path from an unwired one.
    ///
    /// Latency is asserted alongside it so a regression that swaps the two
    /// polls, or drops one while keeping the other, fails here.
    #[cfg(feature = "clap")]
    #[test]
    fn a_clap_tail_change_reaches_the_event_list() {
        use crate::loaders::clap::ClapInstance;

        let _lock = crate::test_utils::plugin_load_lock();
        /// The reference CLAP plugin, built as a dev-dependency by this same
        /// `cargo test` run. Resolved rather than hard-coded so these tests run on
        /// any machine — this used to name an absolute macOS path to a third-party
        /// plugin, which failed everywhere else.
        fn clap_plugin() -> &'static str {
            crate::test_utils::clap_probe_path()
        }
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512)
            .expect("failed to load the CLAP test plugin");
        let mut plugin = Plugin::Clap(instance);

        // Nothing pending: the drain must be empty, or the assertions below
        // would pass on a stuck flag rather than on the one raised here.
        assert!(
            plugin.poll_async_events().is_empty(),
            "a freshly loaded plugin reported an async event nobody raised"
        );

        let Plugin::Clap(clap) = &mut plugin else {
            unreachable!("constructed as Clap immediately above")
        };
        let state = clap.clap_loaded().host_state();
        state
            .processing
            .tail_changed
            .store(true, std::sync::atomic::Ordering::Release);
        state
            .processing
            .latency_changed
            .store(true, std::sync::atomic::Ordering::Release);

        let events = plugin.poll_async_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AsyncEvent::TailChanged { .. })),
            "the tail notification was polled and dropped; got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AsyncEvent::LatencyChanged { .. })),
            "the latency notification went missing; got {events:?}"
        );

        // Both flags are consumed, so a second drain is quiet — an event that
        // re-fires every block would spam the host with resyncs.
        assert!(
            plugin.poll_async_events().is_empty(),
            "the change flags were not cleared by the drain"
        );
    }

    /// An AU latency, tail or parameter-list change must reach the drained
    /// event list.
    ///
    /// Without a `Plugin::Au` arm in `poll_async_events` the match falls through
    /// `_ => {}`, and an AU that changes its latency on a mode switch leaves PDC
    /// compensating the load-time figure permanently — with the `AUEventListener`
    /// machinery carrying the signal working perfectly and nobody reading it.
    ///
    /// The flags are raised directly rather than by making a plugin move its
    /// latency, which is the same fixture `a_clap_tail_change_reaches_the_event_list`
    /// uses and for the same reason: what is under test here is the *wiring*
    /// from flag to `AsyncEvent`, and nothing installed on this machine changes
    /// its latency on request. The listener half — that a real
    /// `kAudioUnitProperty_Latency` notification raises the flag — is pinned
    /// separately in `tutti-au-host`'s `au_property_watch.rs`, against a probe
    /// that can post one.
    ///
    /// A missing listener fails the test rather than skipping it: `poll_changes`
    /// returns an empty result when `watch` is `None`, so a silent skip would
    /// make every assertion below vacuous. That is the *listener*, though — a
    /// missing **plugin** is a machine fact and skips, per [`au_fixture`].
    #[cfg(all(feature = "au", target_os = "macos"))]
    #[test]
    fn an_au_property_change_reaches_the_event_list() {
        use crate::loaders::au::AuInstance;
        use std::sync::atomic::Ordering;

        let _lock = crate::test_utils::plugin_load_lock();
        let Some(fixture) = au_fixture() else {
            return;
        };
        let instance = AuInstance::load(Path::new(&fixture), 44100.0, 512)
            .expect("the AU fixture resolved but would not load");
        let mut plugin = Plugin::Au(instance);

        // Nothing pending: the drain must be empty, or the assertions below
        // would pass on a stuck flag rather than on the ones raised here.
        assert!(
            plugin.poll_async_events().is_empty(),
            "a freshly loaded plugin reported an async event nobody raised"
        );

        // The `Arc` is cloned rather than borrowed so the flag writes and the
        // drain below do not overlap a borrow of `plugin`.
        let flags = {
            let Plugin::Au(au) = &mut plugin else {
                unreachable!("constructed as Au immediately above")
            };
            std::sync::Arc::clone(au.property_flags().expect(
                "the AU property listener was not installed, so every assertion \
                 below would pass against a host that noticed nothing",
            ))
        };
        flags.latency.store(true, Ordering::Release);
        flags.tail.store(true, Ordering::Release);
        flags.param_list.store(true, Ordering::Release);

        let events = plugin.poll_async_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AsyncEvent::LatencyChanged { .. })),
            "the latency change was polled and dropped; got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AsyncEvent::TailChanged { .. })),
            "the tail change went missing; got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AsyncEvent::ParamTitlesChanged)),
            "a parameter-list change must ask the client to re-pull the list; \
             got {events:?}"
        );

        // All three flags are consumed, so a second drain is quiet — an event
        // that re-fired every block would spam the host with resyncs, and a
        // latency event does a PDC re-plan each time.
        assert!(
            plugin.poll_async_events().is_empty(),
            "the change flags were not cleared by the drain"
        );
    }

    /// A latency change must refresh the metadata the client reads, not only the
    /// event it emits.
    ///
    /// The two answering differently is the load-time-only read's failure in
    /// permanent form: `loaded().latency_samples` would keep reporting the
    /// figure captured at load while the event carried a newer one, and a client
    /// that trusted the metadata over the event would compensate the stale value
    /// forever.
    #[cfg(all(feature = "au", target_os = "macos"))]
    #[test]
    fn an_au_latency_poll_refreshes_the_cached_metadata() {
        use crate::loaders::au::AuInstance;
        use std::sync::atomic::Ordering;
        use tutti_plugin::server::PluginMeta;

        let _lock = crate::test_utils::plugin_load_lock();
        let Some(fixture) = au_fixture() else {
            return;
        };
        let mut au = AuInstance::load(Path::new(&fixture), 44100.0, 512)
            .expect("the AU fixture resolved but would not load");

        let flags = std::sync::Arc::clone(
            au.property_flags()
                .expect("the AU property listener was not installed"),
        );
        // Poison the cache before polling. Without this the test cannot fail:
        // no installed AU changes its latency on request, so `loaded()` and the
        // re-read both return the load-time figure and agree whether or not the
        // refresh happened. Deleting the refresh line survives this test without
        // the poisoning — verified by mutation.
        let real_latency = au.loaded().latency_samples;
        au.poison_cached_latency(Samples(real_latency.0 + 1234));

        flags.latency.store(true, Ordering::Release);
        flags.tail.store(true, Ordering::Release);

        let changes = au.poll_changes();
        let reported_latency = changes
            .latency
            .expect("a raised latency flag must produce a re-read");
        let reported_tail = changes
            .tail
            .expect("a raised tail flag must produce a re-read");

        // The re-read must report the AU's real figure, not the poisoned one —
        // otherwise `poll_changes` echoed the cache instead of asking the unit.
        assert_eq!(
            reported_latency, real_latency,
            "the emitted latency must come from a fresh read, not the cache"
        );

        assert_eq!(
            au.loaded().latency_samples,
            reported_latency,
            "loaded() and the emitted event must not disagree about latency"
        );
        assert_eq!(
            au.loaded().tail,
            reported_tail,
            "loaded() and the emitted event must not disagree about tail"
        );
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

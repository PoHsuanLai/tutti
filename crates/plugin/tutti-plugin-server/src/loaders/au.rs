//! Audio Unit plugin loader — thin wrapper around `au-host` crate.
//!
//! Follows the same pattern as `vst3_loader.rs` and `clap_loader.rs`.

#[cfg(all(target_os = "macos", feature = "au"))]
use std::collections::HashMap;
use std::path::Path;
use tutti_plugin::server::{
    AuComponentType, EditorPresence, Features, LoadedPlugin, PluginClass, PluginDescriptor,
};
#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_plugin::server::{
    EditorSize, Normalized, ParamAddress, ParamFlags, ParamRange, ParamSteps, ParameterInfo,
    PluginAudio, PluginEditorHost, PluginMeta, PluginParams, PluginPresets, PluginResult,
    PluginState, PluginTail, Preset, PresetId, ProcessContext, ProcessOutput, RenderMode,
    WindowHandle,
};

use crate::loaders::common::{single_bus, Meta};
use tutti_plugin::{BridgeError, LoadStage, Result};

#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_au_host::{
    component, editor::AuEditor, instance::AuInstance as AuHostInstance, parameters, Samples,
};

/// Map the AU host's native component type to the wire `AuComponentType` mirror.
#[cfg(all(target_os = "macos", feature = "au"))]
fn map_au_type(t: tutti_au_host::component::AuType) -> AuComponentType {
    use tutti_au_host::component::AuType;
    match t {
        AuType::Effect => AuComponentType::Effect,
        AuType::Instrument => AuComponentType::Instrument,
        AuType::Generator => AuComponentType::Generator,
        AuType::MusicEffect => AuComponentType::MusicEffect,
        AuType::Mixer => AuComponentType::Mixer,
        AuType::Converter => AuComponentType::Converter,
        AuType::Output => AuComponentType::Output,
        AuType::MidiProcessor => AuComponentType::MidiProcessor,
        AuType::Unknown(code) => AuComponentType::Unknown(code),
    }
}

pub struct AuInstance {
    /// Declared **first** so it drops first. The registration holds the raw
    /// `AudioUnit` that `inner` owns, and AudioToolbox dereferences it on every
    /// delivery — disposing the unit while a listener is still registered is a
    /// use-after-free. Rust drops fields in declaration order, so this ordering
    /// is the guarantee; see `PropertyWatch`.
    ///
    /// `None` when the listener could not be created, in which case the
    /// load-time latency, tail and parameter list stand as the whole answer.
    #[cfg(all(target_os = "macos", feature = "au"))]
    watch: Option<PropertyWatch>,
    #[cfg(all(target_os = "macos", feature = "au"))]
    inner: AuHostInstance,
    #[cfg(all(target_os = "macos", feature = "au"))]
    editor: Option<AuEditor>,
    /// Declared `[min, max]` per parameter id, captured once at load.
    ///
    /// `ProcessContext::param_changes` carries **normalized** `0..=1` values (the
    /// host's authoring convention — see `PluginParams::get_parameter`), but AU's
    /// `AudioUnitSetParameter` takes **native plain units**. Denormalizing needs
    /// the declared range, and re-reading `kAudioUnitProperty_ParameterInfo` per
    /// automation point on the audio thread would be a property round-trip per
    /// block, so the ranges are cached here at load time.
    ///
    /// A `Vec` sorted by id rather than a `HashMap`: AU parameter counts are in
    /// the tens, so a binary search beats hashing and keeps the RT path
    /// allocation-free.
    #[cfg(all(target_os = "macos", feature = "au"))]
    param_ranges: Vec<(u32, ParamBounds)>,
    meta: Meta,
}

/// Declared plain-unit bounds for one AU parameter, as reported by
/// `kAudioUnitProperty_ParameterInfo`.
///
/// Raw `f32` Hz/dB/percent/seconds, not `tutti_types` unit newtypes: which
/// physical unit these bounds are in is per-parameter and only known at runtime
/// from `AudioUnitParameterInfo::unit`, and the values cross the AudioToolbox C
/// ABI verbatim.
#[cfg(all(target_os = "macos", feature = "au"))]
#[derive(Debug, Clone, Copy)]
struct ParamBounds {
    min: f32,
    max: f32,
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl ParamBounds {
    /// These bounds as the shared range type, which owns the conversion.
    ///
    /// `default` is unused by [`to_plain`](ParamRange::to_plain) /
    /// [`to_normalized`](ParamRange::to_normalized) — only the endpoints
    /// participate — so `min` stands in rather than a value invented here.
    fn as_range(self) -> ParamRange {
        ParamRange::Plain {
            min: self.min as f64,
            max: self.max as f64,
            default: self.min as f64,
        }
    }

    /// Map a normalized `0..=1` value onto `[min, max]`.
    ///
    /// Delegates to [`ParamRange::to_plain`], narrowing to the `f32` that
    /// `AudioUnitSetParameter` takes. **Delegation rather than an `f32`
    /// hand-copy**: the four guards this depends on — non-finite bounds,
    /// degenerate range, NaN value, clamp order — are stated once there, and two
    /// copies would drift apart silently.
    ///
    /// The narrowing is safe for the property this path needs: `to_plain`
    /// never returns a non-finite `f64`, and every finite `f64` narrows to a
    /// finite `f32` or to an infinity — which cannot arise here, because the
    /// result is bounded by `[min, max]` and both came *from* an `f32`.
    fn to_plain(self, normalized: f64) -> f32 {
        self.as_range().to_plain(normalized) as f32
    }

    /// Inverse of [`to_plain`](Self::to_plain): map the AU's plain value back
    /// onto normalized `0..=1`.
    ///
    /// The read direction of the same boundary. `PluginParams::get_parameter`
    /// is normalized for every format, and `AudioUnitGetParameter` answers in
    /// plain units, so a read without this reports a cutoff of `22050` where
    /// the caller expects `1.0`.
    fn to_normalized(self, plain: f32) -> f64 {
        self.as_range().to_normalized(plain as f64)
    }
}

/// Look up the declared bounds for `id` in a range table sorted by id.
#[cfg(all(target_os = "macos", feature = "au"))]
fn lookup_bounds(table: &[(u32, ParamBounds)], id: u32) -> Option<ParamBounds> {
    table
        .binary_search_by_key(&id, |&(pid, _)| pid)
        .ok()
        .map(|i| table[i].1)
}

/// Read every parameter's declared range off the AU, sorted by id for
/// [`lookup_bounds`].
#[cfg(all(target_os = "macos", feature = "au"))]
fn read_param_ranges(unit: tutti_au_host::types::AudioUnit) -> Vec<(u32, ParamBounds)> {
    let mut table: Vec<(u32, ParamBounds)> = parameters::list(unit)
        .into_iter()
        .map(|p| {
            (
                p.id,
                ParamBounds {
                    min: p.range.min,
                    max: p.range.max,
                },
            )
        })
        .collect();
    table.sort_unstable_by_key(|&(id, _)| id);
    table
}

/// Read `kAudioUnitProperty_TailTime` and classify it.
///
/// AU is the one format that reports tail in *seconds*, and the one where a
/// refusal is meaningful: every Apple instrument, mixer and generator rejects
/// the property outright, which is "no tail concept", not "no tail". That is
/// `Unknown`.
///
/// The infinite case is why `PluginTail` has an `Unbounded` arm at all. TAL
/// Reverb 4 answers `f64::INFINITY`, and `Seconds::to_samples` maps every
/// non-finite input to `Samples::ZERO` — deliberately, since that is right for
/// NaN and negatives. Converting first would make an infinite reverb
/// indistinguishable from a plugin with no tail, and a bounce sizing its render
/// from that number truncates the reverb entirely. So the finiteness question is
/// asked *before* the conversion, never after.
///
/// A function rather than an inline block because the load path and the
/// property-change poll both need it, and the two answering differently would
/// mean a tail that changed at runtime was classified by rules the load never
/// used.
#[cfg(all(target_os = "macos", feature = "au"))]
fn read_tail(inner: &AuHostInstance, sample_rate: f64) -> PluginTail {
    match inner.get_tail_time() {
        Err(_) => PluginTail::Unknown,
        Ok(seconds) if !seconds.get().is_finite() => PluginTail::Unbounded,
        Ok(seconds) => match seconds.to_samples_ceil(sample_rate) {
            // A declared-but-zero tail is a real answer: the unit has a tail
            // concept and says it has none.
            s if s == Samples::ZERO => PluginTail::None,
            // `_ceil`, not `_floor`: a render that rounds a tail down clips its
            // last partial block.
            s => PluginTail::Finite(s),
        },
    }
}

/// The three properties that go stale after load, and the flags a change to one
/// of them raises.
///
/// Latency and tail are `Float64` **seconds** properties an AU may rewrite at
/// any time — a linear-phase EQ switching modes, an oversampling toggle, a
/// reverb whose decay was turned up. Read once at load, they are a figure PDC
/// keeps compensating and a bounce keeps sizing from long after the plugin
/// stopped agreeing with them. The parameter list is the third: an AU that grows
/// or renames parameters leaves the range table denormalizing automation against
/// bounds that no longer exist.
///
/// # Why flags rather than values
///
/// The listener callback arrives on a **GCD queue thread**, not the server
/// thread (see `tutti_au_host::listener`'s module docs). Reading
/// `kAudioUnitProperty_Latency` from there would be a property call on a unit
/// the server thread may be rendering through, so the callback records only
/// *that* something changed and the server thread does the read when it drains,
/// between blocks. Three `AtomicBool`s rather than one: a latency change and a
/// parameter-list change have different consequences downstream, and collapsing
/// them would make every mode switch re-pull the whole parameter list.
///
/// This mirrors `tutti-clap-host`'s `HostState`, where `clap_host_latency
/// .changed` sets a flag that `poll_latency_changed` consumes. The shapes match
/// because the constraint does: a callback on a foreign thread, drained by the
/// thread that owns the plugin.
#[cfg(all(target_os = "macos", feature = "au"))]
#[derive(Default)]
pub(crate) struct PropertyFlags {
    pub(crate) latency: std::sync::atomic::AtomicBool,
    pub(crate) tail: std::sync::atomic::AtomicBool,
    pub(crate) param_list: std::sync::atomic::AtomicBool,
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PropertyFlags {
    /// Read and clear one flag.
    ///
    /// `swap`, not a load-then-store: the GCD thread can raise the flag between
    /// the two halves of a non-atomic read/clear, and the change would be
    /// dropped — the plugin's new latency would then reach nobody until the
    /// *next* change, which for a one-shot mode switch is never.
    fn take(flag: &std::sync::atomic::AtomicBool) -> bool {
        flag.swap(false, std::sync::atomic::Ordering::AcqRel)
    }
}

/// A live property-change registration on the AU, plus the flags it raises.
///
/// Held by [`AuInstance`] so the registration lives exactly as long as the unit
/// it watches. Field order is the invariant: `_listener` is declared **before**
/// `flags` so it is disposed first, and `AuInstance` declares this whole struct
/// before `inner` for the same reason — `AuParameterListener`'s safety contract
/// is that the `AudioUnit` outlives it, and AudioToolbox dereferences the raw
/// unit pointer on every delivery.
#[cfg(all(target_os = "macos", feature = "au"))]
struct PropertyWatch {
    /// Never read after construction. Dropping it runs `AUListenerDispose`,
    /// which is the only thing that stops further deliveries.
    _listener: tutti_au_host::listener::AuParameterListener,
    flags: std::sync::Arc<PropertyFlags>,
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PropertyWatch {
    /// Register on `unit`, watching latency, tail and the parameter list.
    ///
    /// Returns `None` if AudioToolbox refuses the listener, which leaves the
    /// load-time figures as the whole answer — the behaviour before any of this
    /// existed. A refusal is not fatal: an AU whose latency cannot be watched is
    /// still a usable plugin, exactly as one that refuses to report latency at
    /// all is.
    ///
    /// The three `watch_property` calls are individually tolerant for a
    /// different reason: `AUEventListenerAddEventType` accepts every property id
    /// without validating it (pinned by
    /// `au_api_surface.rs::registration_never_refuses_a_property_id`), so a
    /// refusal here would be AudioToolbox failing rather than the AU declining a
    /// property — but a failure on one must not cost the other two.
    ///
    /// # Safety
    /// `unit` must be the live `AudioUnit` of the instance that stores the
    /// returned watch, so the field-order invariant above makes it outlive the
    /// registration.
    unsafe fn install(unit: tutti_au_host::types::AudioUnit) -> Option<Self> {
        use std::sync::atomic::Ordering;

        use tutti_au_host::listener::{AuEvent, AuParameterListener, EventAddress};
        use tutti_au_host::types::{
            K_AUDIO_UNIT_PROPERTY_LATENCY, K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
            K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
        };

        let flags = std::sync::Arc::new(PropertyFlags::default());
        let sink = std::sync::Arc::clone(&flags);
        // SAFETY: the caller guarantees `unit` outlives the returned watch.
        let listener = unsafe {
            AuParameterListener::new(unit, move |ev| {
                // Only property changes are subscribed, so a parameter or
                // gesture event here would mean the registration went to the
                // wrong event type; ignore rather than mapping it onto a flag it
                // does not mean.
                if let AuEvent::PropertyChanged { id, .. } = ev {
                    let flag = match id {
                        K_AUDIO_UNIT_PROPERTY_LATENCY => &sink.latency,
                        K_AUDIO_UNIT_PROPERTY_TAIL_TIME => &sink.tail,
                        K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST => &sink.param_list,
                        _ => return,
                    };
                    // `Release` pairs with the `AcqRel` swap in
                    // `PropertyFlags::take`, so the server thread that observes
                    // the flag is ordered after everything the AU did before
                    // posting it.
                    flag.store(true, Ordering::Release);
                }
            })
        }
        .ok()?;

        for id in [
            K_AUDIO_UNIT_PROPERTY_LATENCY,
            K_AUDIO_UNIT_PROPERTY_TAIL_TIME,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
        ] {
            let _ = listener.watch_property(id, EventAddress::GLOBAL);
        }

        Some(Self {
            _listener: listener,
            flags,
        })
    }
}

unsafe impl Send for AuInstance {}

impl AuInstance {
    /// Lightweight probe: read AU component info without instantiation.
    pub fn probe(path: &Path) -> Result<PluginDescriptor> {
        #[cfg(all(target_os = "macos", feature = "au"))]
        {
            let bundle_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let components = component::enumerate_components();
            let component_info = components
                .iter()
                .find(|c| {
                    c.name.contains(&bundle_name)
                        || c.name.ends_with(&bundle_name)
                        || c.name.split(": ").last().is_some_and(|n| n == bundle_name)
                })
                .ok_or_else(|| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!("No Audio Unit component matching '{}'", bundle_name),
                })?;

            Ok(PluginDescriptor {
                id: format!(
                    "au.{}.{}",
                    tutti_au_host::types::fourcc_to_string(component_info.manufacturer_code),
                    tutti_au_host::types::fourcc_to_string(component_info.sub_type),
                ),
                name: component_info.name.clone(),
                vendor: component_info.manufacturer.clone(),
                version: component_info.version.clone(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                // A probe reads the registry entry without instantiating, and
                // `AuEditor::has_editor` needs a live unit. The load path asks.
                editor: EditorPresence::Unknown,
            })
        }
        #[cfg(not(all(target_os = "macos", feature = "au")))]
        Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "AU support not compiled".to_string(),
        })
    }

    /// Load an Audio Unit from a `.component` bundle path.
    ///
    /// AU plugins are registered system-wide; the path is used for identification
    /// but the actual loading goes through AudioComponentFindNext.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        #[cfg(all(target_os = "macos", feature = "au"))]
        {
            // Strategy: enumerate all components, find one whose name matches
            // the bundle's file stem, or scan by known bundle structure.
            let bundle_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            // Try to find a matching AU component by name
            let components = component::enumerate_components();
            let matching = components.iter().find(|c| {
                // AU names are typically "Manufacturer: PluginName"
                // Match if the name contains this plugin's bundle name
                c.name.contains(&bundle_name)
                    || c.name.ends_with(&bundle_name)
                    // Also try exact match on the part after ": "
                    || c.name
                        .split(": ")
                        .last()
                        .is_some_and(|n| n == bundle_name)
            });

            let component_handle = if let Some(info) = matching {
                info.component
            } else {
                // Fallback: try all effect and instrument types
                return Err(BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!(
                        "No Audio Unit component found matching '{}'. \
                         Available AUs: {}",
                        bundle_name,
                        components
                            .iter()
                            .take(10)
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            };

            let component_info = matching.unwrap();

            // Safety: component_handle was obtained from AudioComponentFindNext
            let mut inner =
                unsafe { AuHostInstance::new(component_handle, sample_rate, block_size as u32) }
                    .map_err(|e| BridgeError::LoadFailed {
                        path: path.to_path_buf(),
                        stage: LoadStage::Instantiation,
                        reason: e.to_string(),
                    })?;

            inner.initialize().map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: e.to_string(),
            })?;

            let name = inner.get_name().unwrap_or_else(|_| bundle_name.clone());
            let has_editor = AuEditor::has_editor(inner.raw_unit());
            // A refusal is compensated as zero rather than failing the load: an
            // AU that will not say how far it delays audio is still a usable
            // plugin, and under-compensating it costs alignment, not audio.
            // This is now a decision on a reachable `Err` — `get_latency` used
            // to swallow the refusal internally, so this arm never ran.
            //
            // No AU registered on macOS 15.6 takes this path (29 of 29 answer),
            // so it is the third-party case, unmeasured by construction.
            let latency = inner.get_latency().unwrap_or(Samples::ZERO);

            let tail = read_tail(&inner, sample_rate);

            let descriptor = PluginDescriptor {
                id: format!(
                    "au.{}.{}",
                    tutti_au_host::types::fourcc_to_string(component_info.manufacturer_code),
                    tutti_au_host::types::fourcc_to_string(component_info.sub_type),
                ),
                name,
                vendor: component_info.manufacturer.clone(),
                version: component_info.version.clone(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                editor: EditorPresence::measured(has_editor),
            };
            // This AUv2 host is f32-only, single-bus, with a Cocoa editor,
            // latency read-back and MIDI *input*. MIDI output, transport/host-
            // callbacks, sample-accurate automation, note-expression, sequencer
            // context, f64, and host-driven editor resize are not implemented
            // (latency presence is derived from `latency_samples`).
            //
            // MIDI output is the one gap that is a wiring job rather than an
            // absent API: `AuInstance::install_midi_output` exists, but nothing
            // here plumbs a callback to it, so the bit stays unprobed.
            let mut features = Features::empty();
            features.set(Features::EDITOR, has_editor);
            // The process path routes MIDI to any AU whose component type
            // `receives_midi()` — instruments, music effects, MIDI processors.
            // Reporting the same predicate here keeps one fact from being
            // answered twice; a second, hand-maintained answer is how an AU
            // instrument ends up declaring no MIDI_IN while being sent MIDI on
            // every block.
            features.set(
                Features::MIDI_IN,
                component_info.component_type.receives_midi(),
            );
            // One property backs both bits: a unit that lists factory presets
            // can be asked to load any of them. An empty list is a genuine
            // "none", not a failed read — `factory_presets` absorbs the
            // OSStatus error several working Apple units return.
            let has_presets = !inner.factory_presets().is_empty();
            features.set(Features::PRESET_LIST, has_presets);
            features.set(Features::PRESET_LOAD, has_presets);
            let probed = tutti_plugin::server::probed::AU;

            // AU exposes a single main bus per direction here.
            // One main bus per direction, so one topology entry each. The tag
            // is the AU's own answer; `topology_of` declines the tags that name
            // a processing relationship rather than speaker placement (MidSide,
            // MatrixStereo, ambisonics), which arrive here as `None`.
            let bus_topology = |direction: tutti_au_host::BusDirection| {
                let tag = inner.layout_tag(direction, 0).ok()?;
                tutti_au_host::topology_of(tag)
            };
            let input_topology =
                core::iter::once(bus_topology(tutti_au_host::BusDirection::Input)).collect();
            let output_topology =
                core::iter::once(bus_topology(tutti_au_host::BusDirection::Output)).collect();

            let loaded = LoadedPlugin {
                // `num_inputs`/`num_outputs` are `u32` off the AU element
                // count; `From<u32>` canonicalizes them, so the `as usize`
                // hop is gone.
                inputs: single_bus(inner.num_inputs()),
                outputs: single_bus(inner.num_outputs()),
                input_topology,
                output_topology,
                latency_samples: latency,
                tail,
                features,
                probed,
            };

            // Capture the declared plain ranges once, while still on the load
            // thread — the RT path denormalizes against these. `poll_changes`
            // re-reads them when the AU says its parameter list moved.
            let param_ranges = read_param_ranges(inner.raw_unit());

            // Start watching the three properties the figures above are frozen
            // copies of, so a change reaches `poll_changes` instead of being
            // invisible until the next load.
            //
            // SAFETY: the watch is stored in the same struct as `inner` and
            // declared before it, so `inner`'s `AudioUnit` outlives the
            // registration.
            let watch = unsafe { PropertyWatch::install(inner.raw_unit()) };

            Ok(Self {
                watch,
                inner,
                editor: None,
                param_ranges,
                meta: Meta { descriptor, loaded },
            })
        }

        #[cfg(not(all(target_os = "macos", feature = "au")))]
        {
            let _ = (path, sample_rate, block_size);
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: "Audio Unit support not available (requires macOS + 'au' feature)"
                    .to_string(),
            })
        }
    }
}

/// What a property-change poll found, in the shape
/// `crate::plugin::Plugin::poll_async_events` needs to emit from.
///
/// `Option`/`bool` rather than always-present values so "nothing changed" — the
/// answer on every block but the rare one — costs no property reads at all. The
/// AU is only asked when it said it had something new to say.
#[cfg(all(target_os = "macos", feature = "au"))]
#[derive(Default)]
pub(crate) struct AuPropertyChanges {
    /// Re-read latency. Push to PDC.
    pub latency: Option<Samples>,
    /// Re-read tail.
    pub tail: Option<PluginTail>,
    /// The AU's parameter list moved; the client should re-pull it.
    pub param_list_changed: bool,
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl AuInstance {
    /// Drain the property-change flags the listener raised since the last poll,
    /// re-reading each changed property and refreshing the cached copy.
    ///
    /// Polled between audio blocks by `crate::plugin::Plugin::poll_async_events`;
    /// it must not be called concurrently with `PluginAudio::process`, which is
    /// what makes the property reads here safe to do on a live unit.
    ///
    /// Both cached figures are refreshed alongside the returned change, so
    /// `loaded()` and the event carried across IPC cannot disagree about what
    /// the AU reports — the failure the load-time-only read had in permanent
    /// form.
    ///
    /// No deactivate/reactivate cycle, which VST3's equivalent needs: AU's
    /// latency and tail are plain global-scope property reads with no
    /// documented requirement that the unit be uninitialized, and the property
    /// listener fires *after* the AU has already changed the value. VST3's
    /// cycle exists because `IComponentHandler::restartComponent` is a
    /// *request* the plugin makes before the new figure is readable.
    pub(crate) fn poll_changes(&mut self) -> AuPropertyChanges {
        let mut changes = AuPropertyChanges::default();
        let Some(watch) = &self.watch else {
            return changes;
        };

        if PropertyFlags::take(&watch.flags.latency) {
            // Same refusal handling as the load path: an AU that stops
            // answering keeps whatever it last reported rather than being
            // silently recompensated to zero, which would move audio.
            if let Ok(samples) = self.inner.get_latency() {
                self.meta.loaded.latency_samples = samples;
                changes.latency = Some(samples);
            }
        }

        if PropertyFlags::take(&watch.flags.tail) {
            // Unconditional, unlike latency: `read_tail` maps a refusal to
            // `PluginTail::Unknown`, which is a real answer here — an AU that
            // dropped its tail concept is telling the host to stop sizing a
            // bounce from a figure it no longer stands behind.
            let tail = read_tail(&self.inner, self.inner.sample_rate());
            self.meta.loaded.tail = tail;
            changes.tail = Some(tail);
        }

        if PropertyFlags::take(&watch.flags.param_list) {
            // The range table is what `process` denormalizes automation
            // against, so a list that grew leaves new parameters unwritable
            // (`lookup_bounds` misses them and the write is skipped) and one
            // that changed bounds leaves every write scaled by the old span.
            // Re-read it before telling the client, so a client that
            // immediately re-pulls the list gets a table that already agrees.
            self.param_ranges = read_param_ranges(self.inner.raw_unit());
            changes.param_list_changed = true;
        }

        changes
    }

    /// The property-change flags the listener raises, for a test that needs to
    /// drive a change no installed AU will make on request.
    ///
    /// `None` when the listener could not be created, which a caller must treat
    /// as a failed fixture rather than a passing test — see
    /// `plugin.rs::an_au_property_change_reaches_the_event_list`.
    #[cfg(test)]
    pub(crate) fn property_flags(&self) -> Option<&std::sync::Arc<PropertyFlags>> {
        self.watch.as_ref().map(|w| &w.flags)
    }

    /// Overwrite the cached latency, so a test can prove `poll_changes` replaces
    /// it rather than leaves it.
    ///
    /// Without this the assertion "`loaded()` and the event agree" is satisfied
    /// by *any* AU whose latency does not change during the test — which is
    /// every AU, since nothing installed changes latency on request. Both values
    /// would read the load-time figure and match, refresh or no refresh.
    /// Deleting the refresh line was mutation-tested and survived for exactly
    /// this reason. Poisoning the cache first is what makes the two figures
    /// differ when the refresh is missing.
    #[cfg(test)]
    pub(crate) fn poison_cached_latency(&mut self, samples: Samples) {
        self.meta.loaded.latency_samples = samples;
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginMeta for AuInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginAudio for AuInstance {
    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        // Automation arrives normalized `0..=1` (the host's authoring
        // convention, shared with VST2/VST3), but `AudioUnitSetParameter` takes
        // NATIVE PLAIN UNITS — AU has no normalization concept at all. Writing
        // the normalized value straight through set Apple AUDelay's Lowpass
        // Cutoff (declared `[10, 22050]` Hz) to 1 Hz at full scale and clamped
        // every plain value above 1.0 away, making the entire usable range of
        // every Hz/dB/percent/seconds parameter unreachable.
        //
        // Denormalize against the range the AU itself declared. A parameter
        // missing from the table (the AU grew a parameter after load, or
        // refused `ParameterInfo`) is skipped rather than written blind: a
        // guessed range would be the same class of bug.
        if let Some(changes) = ctx.param_changes {
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    // `AudioUnitParameterID` is opaque; a VST2 positional index
                    // addresses nothing here.
                    let Some(id) = queue.param_id.opaque().map(|i| i.get()) else {
                        continue;
                    };
                    if let Some(bounds) = lookup_bounds(&self.param_ranges, id) {
                        let _ = self
                            .inner
                            .set_parameter(id, bounds.to_plain(point.value.get()));
                    }
                }
            }
        }

        // Deliver MIDI to instrument / music-effect AUs before rendering, so
        // note-ons scheduled this block sound in it. Plain effects don't
        // consume MIDI (`receives_midi()` is false) — skip the decode for them.
        if !ctx.midi_events.is_empty() && self.inner.au_type().receives_midi() {
            self.inner.send_midi(ctx.midi_events);
        }

        match buffer {
            tutti_plugin::server::AudioBufferMut::F32(buf) => {
                self.inner
                    .process(buf.inputs, buf.outputs, buf.num_samples as u32)
                    .map_err(|e| BridgeError::ProcessError(format!("[au] {e}")))?;
            }
            tutti_plugin::server::AudioBufferMut::F64(buf) => {
                // AUv2 doesn't support f64 natively. Convert f32 -> process -> convert back.
                //
                // TODO(rt-alloc): this branch allocates four `Vec`s per block on
                // the audio thread — two buffer sets and two pointer tables.
                // Unlike the sibling loaders' conversions it is ungated: any AU
                // running on the f64 path pays it every block.
                //
                // The fix is the one VST2 already uses — a resident
                // `RenderScratch` on the instance, sized at `load` (which
                // already receives `block_size`) and cleared per block, plus a
                // `Vec<&[f32]>` / `Vec<&mut [f32]>` pair reused the same way.
                // See `tutti-vst2-host/src/scratch.rs`, whose doc states the
                // rule: "Call once at load time, never on the audio thread —
                // this is the crate's only render-path allocation."
                //
                // Left undone deliberately: this module is
                // `#[cfg(all(target_os = "macos", feature = "au"))]`, and the
                // change was authored on Linux where it cannot be compiled,
                // borrow-checked, or tested. Writing it blind would put
                // unverified code on an audio path. Whoever picks this up needs
                // a macOS box and an AU that negotiates f64.
                let input_f32: Vec<Vec<f32>> = buf
                    .inputs
                    .iter()
                    .map(|ch| ch.iter().map(|&s| s as f32).collect())
                    .collect();
                let mut output_f32: Vec<Vec<f32>> = buf
                    .outputs
                    .iter()
                    .map(|ch| vec![0.0f32; ch.len()])
                    .collect();

                let in_slices: Vec<&[f32]> = input_f32.iter().map(|v| v.as_slice()).collect();
                let mut out_slices: Vec<&mut [f32]> =
                    output_f32.iter_mut().map(|v| v.as_mut_slice()).collect();

                self.inner
                    .process(&in_slices, &mut out_slices, buf.num_samples as u32)
                    .map_err(|e| BridgeError::ProcessError(format!("[au] {e}")))?;

                for (ch, out_ch) in buf.outputs.iter_mut().enumerate() {
                    if ch < output_f32.len() {
                        for (i, s) in out_ch.iter_mut().enumerate() {
                            if i < output_f32[ch].len() {
                                *s = output_f32[ch][i] as f64;
                            }
                        }
                    }
                }
            }
        }

        Ok(ProcessOutput::default())
    }

    fn set_sample_rate(&mut self, rate: f64) {
        let _ = self.inner.set_sample_rate(rate);
    }

    /// Write `kAudioUnitProperty_OfflineRender`, bracketed by an
    /// uninitialize/re-initialize cycle.
    ///
    /// The bracket is not optional: a unit that sizes an oversampling or
    /// look-ahead buffer from this flag can only do so at
    /// `AudioUnitInitialize`, so writing it to a live unit is accepted and then
    /// has no effect.
    ///
    /// Returns whether *this unit* accepted the property — the AU half of the
    /// live probe behind [`Features::RENDER_MODE`]. A re-initialization failure
    /// also reports `false`: the caller asked for a mode and did not get it,
    /// and that is the question this bool answers.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        self.inner
            .set_offline_render_bracketed(mode.is_offline())
            .unwrap_or(false)
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginParams for AuInstance {
    /// Normalized `0..=1`, per the [`PluginParams`] contract — `AudioUnitGet`
    /// answers in plain units, so the declared range maps it back.
    ///
    /// Uses the same `param_ranges` table the automation path denormalizes
    /// against, so the direct path and `process` cannot disagree about what a
    /// parameter's bounds are.
    ///
    /// A parameter absent from the table has no declared range to normalize
    /// against; its plain value is reported unchanged rather than scaled by a
    /// guess. `read_param_ranges` lists every parameter the AU declares, so an
    /// absence means the AU did not declare it.
    fn get_parameter(&self, id: ParamAddress) -> f64 {
        // A VST2 index addresses nothing here; `AudioUnitParameterID` is opaque.
        let Some(id) = id.opaque() else { return 0.0 };
        let Ok(plain) = parameters::get(self.inner.raw_unit(), id.get()) else {
            return 0.0;
        };
        match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_normalized(plain),
            None => f64::from(plain),
        }
    }

    /// Normalized `0..=1` in, matching [`get_parameter`](Self::get_parameter) —
    /// so this pair round-trips. `AudioUnitSetParameter` takes plain units, so
    /// the value is denormalized against the same table.
    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        let Some(id) = id.opaque() else { return };
        let plain = match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_plain(value.get()),
            None => value.get() as f32,
        };
        let _ = parameters::set(self.inner.raw_unit(), id.get(), plain);
    }

    /// The AU's own display string, denormalized on the way in.
    ///
    /// `AudioUnitParameterStringFromValue` takes the value in **plain** units,
    /// so the same `param_ranges` table `set_parameter` writes through converts
    /// first. Skipping that asks a `[10, 22050]` Hz cutoff to describe `1.0` and
    /// gets `"1 Hz"` back — a plausible-looking string for a value the caller
    /// never named, which is worse than no string at all.
    ///
    /// Measured on macOS 15.6: **no Apple AU implements this property**, so this
    /// returns `None` across the whole system corpus. It is wired anyway because
    /// third-party AUs do implement it, and the alternative is a host that
    /// cannot display their values. See
    /// `tutti-au-host`'s `au_param_display` suite, which pins the absence.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let id = id.opaque()?;
        let plain = match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_plain(value.get()),
            None => value.get() as f32,
        };
        parameters::string_from_value(self.inner.raw_unit(), id.get(), plain)
    }

    /// The inverse, re-normalized on the way out so the result can be handed
    /// straight to [`set_parameter`](Self::set_parameter).
    ///
    /// As with [`parameter_text`](Self::parameter_text), no Apple AU implements
    /// the underlying property on macOS 15.6.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let id = id.opaque()?;
        let plain = parameters::value_from_string(self.inner.raw_unit(), id.get(), text)?;
        let normalized = match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_normalized(plain),
            None => f64::from(plain),
        };
        Some(Normalized::new(normalized))
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        let unit = self.inner.raw_unit();
        let params = parameters::list(unit);

        // Resolve each distinct clump once. `clump_name` is a property read
        // into the AU per call, and a synth with 400 parameters across 7 clumps
        // would otherwise pay 400 round trips for 7 answers.
        let mut clump_names: HashMap<u32, String> = HashMap::new();
        for clump in params.iter().filter_map(|p| p.clump) {
            clump_names
                .entry(clump)
                .or_insert_with(|| parameters::clump_name(unit, clump).unwrap_or_default());
        }

        params
            .into_iter()
            .map(|p| {
                // Indexed params are a choice list whose `[min, max]` are the
                // first and last index, so the position count comes from the
                // span — AUTimePitch's "Overlap" is 0..10, eleven positions.
                let steps = match p.unit {
                    parameters::ParameterUnit::Boolean => ParamSteps::Toggle,
                    parameters::ParameterUnit::Indexed => {
                        ParamSteps::from_span((p.range.max - p.range.min) as f64)
                    }
                    _ => ParamSteps::Continuous,
                };
                // AUv2 advertises IsWritable and nothing else. Writability is
                // not automatability — a host may write a parameter the plugin
                // never meant to be automated — so only READ_ONLY is known.
                ParameterInfo {
                    // `AudioUnitParameterID` — AU's opaque plugin-chosen handle,
                    // the same concept as VST3's `ParamID` and CLAP's `clap_id`.
                    id: ParamAddress::Opaque(p.id.into()),
                    name: p.name,
                    unit: p.unit.to_string(),
                    range: ParamRange::Plain {
                        min: p.range.min as f64,
                        max: p.range.max as f64,
                        default: p.range.default as f64,
                    },
                    steps,
                    flags: if p.writable {
                        ParamFlags::empty()
                    } else {
                        ParamFlags::READ_ONLY
                    },
                    known: ParamFlags::READ_ONLY,
                    // `p.clump` is `None` unless the AU set
                    // `kAudioUnitParameterFlag_HasClump`, so an AU that
                    // declared no clump cannot arrive here as clump 0 — the
                    // gate is in the decoder. A clump the AU declines to name
                    // yields no group rather than a numeric placeholder.
                    group: p
                        .clump
                        .and_then(|c| clump_names.get(&c))
                        .cloned()
                        .unwrap_or_default(),
                }
            })
            .collect()
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginEditorHost for AuInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> PluginResult<EditorSize> {
        let parent_handle = unsafe { tutti_au_host::WindowHandle::from_raw(parent.as_ptr()) };
        // No size to offer: `PluginEditorHost::open_editor` carries only a
        // parent handle, so the host has not told this layer how big the
        // window is. 800×600 is the request; the plugin is free to ignore it,
        // and `editor_size()` below reads back what it actually made.
        let preferred = EditorSize {
            width: 800,
            height: 600,
        };
        let editor =
            unsafe { AuEditor::open(self.inner.raw_unit(), Some(parent_handle), preferred) }
                .map_err(|e| BridgeError::EditorError(e.to_string()))?;
        let size = editor.editor_size();
        self.editor = Some(editor);
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        if let Some(mut ed) = self.editor.take() {
            ed.close();
        }
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginState for AuInstance {
    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        self.inner
            .save_state()
            .map_err(|e| BridgeError::StateSaveError(e.to_string()).into())
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        self.inner
            .load_state(data)
            .map_err(|e| BridgeError::StateRestoreError(e.to_string()).into())
    }
}

/// The AU selector an id names, or `None` when it names none.
///
/// A free function so the decision is testable without a live unit: no AU
/// loads through `AuInstance::load` in a headless test run on this machine
/// (the component registry lists only codecs), so an inline `match` inside
/// `load_preset` could not be exercised at all.
///
/// Only [`PresetId::Number`] addresses an AU preset. A VST3 `(list, index)`
/// pair and a CLAP path name nothing in AU's selector space, and coercing
/// either — taking the `index`, say — would load a real preset the caller
/// never asked for. That is silent, and worse than a refusal.
fn au_selector(id: &PresetId) -> Option<i32> {
    match id {
        PresetId::Number(n) => Some(*n),
        PresetId::Program { .. } | PresetId::Location(_) => None,
    }
}

impl PluginPresets for AuInstance {
    /// The AU's factory presets.
    ///
    /// `AuPreset::number` is a **unit-assigned selector**, not a position: a
    /// unit may number sparsely, and `load_factory_preset` takes the number the
    /// unit reported. So the id is built from `p.number` and never from the
    /// enumeration index — that is the whole reason `PresetId` is opaque.
    ///
    /// `bank` is `None`: AU exposes one flat factory set, and inventing a bank
    /// name would be a claim the format never made.
    fn get_presets(&mut self) -> Vec<Preset> {
        self.inner
            .factory_presets()
            .into_iter()
            .map(|p| Preset::new(PresetId::Number(p.number), p.name))
            .collect()
    }

    /// `false` for an id this format cannot address — a `Program` or `Location`
    /// belongs to another format and names no AU preset.
    ///
    /// A rejected load is a refusal, not an error: `load_factory_preset`
    /// answers `kAudioUnitErr_InvalidPropertyValue` for a number the unit does
    /// not advertise, and the AU is still renderable afterwards with its
    /// parameters untouched.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        match au_selector(id) {
            Some(number) => self.inner.load_factory_preset(number).is_ok(),
            None => false,
        }
    }

    /// `None` when the unit does not implement
    /// `kAudioUnitProperty_PresentPreset`, which `current_preset` reports as an
    /// error precisely because there is no honest value to fabricate — a
    /// "preset 0" would be a claim about the unit's state the host cannot back.
    fn get_current_preset(&mut self) -> Option<PresetId> {
        self.inner
            .current_preset()
            .ok()
            .map(|p| PresetId::Number(p.number))
    }
}

#[cfg(test)]
#[cfg(all(target_os = "macos", feature = "au"))]
mod tests {
    use super::*;

    // Apple's built-in AUDelay should always be available on macOS.
    // Note: AU loading by path requires the component name to match the bundle name.
    // For system AUs, they live in /System/Library/Components/ or
    // /Library/Audio/Plug-Ins/Components/.

    /// Build an initialized AU by four-char code, or `None` when absent.
    fn open_au(ty: u32, sub: &[u8; 4]) -> Option<tutti_au_host::AuInstance> {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;

        let desc = AudioComponentDescription {
            componentType: ty,
            componentSubType: u32::from_be_bytes(*sub),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = component::find_component(&desc)?;
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }.ok()?;
        inner.initialize().ok()?;
        Some(inner)
    }

    /// An AU's clumps reach the shared `ParameterInfo` as group labels.
    ///
    /// AUDistortion is the fixture because it is measurably grouped: macOS 15.6
    /// reports its 22 parameters across 7 named clumps ("Delay", "Ring
    /// Modulation", "Decimation", …). Before this mapping every one of them
    /// arrived ungrouped, so the whole unit rendered as one flat list.
    ///
    /// Two halves, and the second is the one worth having: parameters that
    /// declare a clump get its **name**, and the group is never the clump
    /// *number*. A mapping that stringified the id would satisfy "non-empty"
    /// while showing the user "3".
    #[test]
    fn an_au_clump_becomes_a_group_label() {
        // Loaded through the real entry point, so this exercises
        // `get_parameter_list` itself rather than a copy of its mapping. AU
        // resolves by matching the file stem against the component registry, so
        // the path need not exist on disk.
        let instance = match AuInstance::load(Path::new("AUDistortion"), 44_100.0, 512) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("AUDistortion unavailable ({e:?}); skipping");
                return;
            }
        };

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "AUDistortion declares parameters");

        let groups: std::collections::BTreeSet<&str> = params
            .iter()
            .map(|p| p.group.as_str())
            .filter(|g| !g.is_empty())
            .collect();
        assert!(
            groups.len() > 1,
            "AUDistortion groups its parameters into several named clumps; got {groups:?}"
        );

        // A label, not a stringified id — the failure a bare `!is_empty()`
        // would wave through.
        for group in &groups {
            assert!(
                group.parse::<u32>().is_err(),
                "a group must be the clump's name, not its number: {group:?}"
            );
        }

        // And a grouped parameter qualifies, which is what a consumer renders.
        let grouped = params
            .iter()
            .find(|p| !p.group.is_empty())
            .expect("at least one grouped parameter");
        assert_eq!(
            grouped.qualified_name(),
            format!("{} / {}", grouped.group, grouped.name)
        );
    }

    /// The `AuPreset` -> `PresetId` mapping round-trips against a real unit.
    ///
    /// The mapping is the only new logic here: `factory_presets()` and
    /// `load_factory_preset()` are covered by `tutti-au-host`'s own suite. What
    /// this pins is that the id carries the unit's **selector**, and that
    /// handing it straight back loads the preset it named.
    ///
    /// **Corpus limit, stated rather than papered over.** `AuPreset::number` is
    /// a unit-assigned selector and a unit may number sparsely — that is why
    /// `PresetId` carries the number rather than a position. Measured on this
    /// machine, all four preset-bearing Apple effects (AUDistortion,
    /// AUMatrixReverb, AUReverb2, AUDynamicsProcessor) number densely `0..n`,
    /// so **no available input here distinguishes "carries the selector" from
    /// "uses the vec index"**. Verified by mutation: replacing `p.number` with
    /// `.enumerate()`'s index leaves this test green, and no fixture on this
    /// machine can make it fail. That half is covered where the value *can* be
    /// constructed — `presets::tests::a_sparse_selector_is_carried_verbatim` —
    /// and the reason the mapping must use `p.number` anyway is in
    /// `AuPreset`'s own doc, which the AU layer pins.
    #[test]
    fn a_listed_preset_loads_the_preset_it_names() {
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let Some(mut inner) = open_au(K_AUDIO_UNIT_TYPE_EFFECT, b"dist") else {
            eprintln!("AUDistortion not installed; skipping");
            return;
        };

        // The same mapping `<AuInstance as PluginPresets>::get_presets` does.
        let presets: Vec<Preset> = inner
            .factory_presets()
            .into_iter()
            .map(|p| Preset::new(PresetId::Number(p.number), p.name))
            .collect();
        assert!(
            presets.len() > 2,
            "AUDistortion ships a factory preset table; got {}",
            presets.len()
        );

        // Taken from the listing, never constructed — what a caller does.
        let wanted = presets[2].id.clone();
        let number = wanted.number().expect("an AU preset id is a number");
        assert!(
            inner.load_factory_preset(number).is_ok(),
            "a preset the unit listed must load"
        );
        assert_eq!(
            inner
                .current_preset()
                .ok()
                .map(|p| PresetId::Number(p.number)),
            Some(wanted),
            "the unit must report the preset just loaded"
        );
    }

    /// An id from another format addresses no AU preset.
    ///
    /// `au_selector` is the guard `load_preset` consults. A VST3 `(list,
    /// index)` pair and a CLAP path both answer `None`, so the load is refused
    /// rather than coerced — taking the `index` would load a real preset the
    /// caller never asked for, which is silent and worse than a `false`.
    ///
    /// Tested through the free function rather than through `load_preset`
    /// because no AU loads in a headless run here; see `au_selector`.
    #[test]
    fn an_id_from_another_format_addresses_no_au_preset() {
        assert_eq!(au_selector(&PresetId::Number(7)), Some(7));
        assert_eq!(
            au_selector(&PresetId::Program {
                list_id: 0,
                index: 1
            }),
            None,
            "a VST3 program id must not yield an AU selector"
        );
        assert_eq!(
            au_selector(&PresetId::Location("/x.clap-preset".into())),
            None,
            "a CLAP preset path must not yield an AU selector"
        );
    }

    /// An AU that declines `kAudioUnitProperty_TailTime` is `Unknown`, and one
    /// that reports an unbounded tail is `Unbounded` — never `None`, which is
    /// what a plain sample count would have collapsed both to.
    ///
    /// The Apple corpus covers the first half: every Apple instrument, mixer
    /// and generator rejects the property, and every Apple effect answers it.
    /// The second half needs a plugin that reports `f64::INFINITY` — TAL
    /// Reverb 4 does, and no Apple unit exceeds ~21 s — so it runs only when
    /// that unit is installed and says so when it is skipped.
    #[test]
    fn a_declined_tail_is_unknown_and_an_infinite_one_is_unbounded() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::{K_AUDIO_UNIT_TYPE_EFFECT, K_AUDIO_UNIT_TYPE_MUSIC_DEVICE};

        // An effect answers the property, so it must not be `Unknown`.
        let effect = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = component::find_component(&effect).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let tail = match inner.get_tail_time() {
            Err(_) => PluginTail::Unknown,
            Ok(sec) if !sec.get().is_finite() => PluginTail::Unbounded,
            Ok(sec) => match sec.to_samples_ceil(44_100.0) {
                s if s == Samples::ZERO => PluginTail::None,
                s => PluginTail::Finite(s),
            },
        };
        assert_ne!(
            tail,
            PluginTail::Unknown,
            "AUDelay answers kAudioUnitProperty_TailTime, so its tail is known"
        );

        // An instrument rejects the property outright — that is "no tail
        // concept", which must read as `Unknown` rather than as a zero tail.
        let instrument = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_MUSIC_DEVICE,
            componentSubType: u32::from_be_bytes(*b"dls "),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        if let Some(comp) = component::find_component(&instrument) {
            let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
                .expect("Should create instance");
            inner.initialize().expect("Should initialize");
            let mapped = match inner.get_tail_time() {
                Err(_) => PluginTail::Unknown,
                Ok(sec) if !sec.get().is_finite() => PluginTail::Unbounded,
                Ok(sec) => match sec.to_samples_ceil(44_100.0) {
                    s if s == Samples::ZERO => PluginTail::None,
                    s => PluginTail::Finite(s),
                },
            };
            // Whatever this unit answers, a *refusal* must never surface as a
            // zero tail — that is the mapping this test exists for.
            if inner.get_tail_time().is_err() {
                assert_eq!(
                    mapped,
                    PluginTail::Unknown,
                    "a refused tail property must be Unknown, not None"
                );
                assert_ne!(mapped.samples(), Some(Samples::ZERO));
            }
        }

        // The unbounded half. TAL Reverb 4 reports `f64::INFINITY`; when it is
        // not installed this leg is skipped, and says so rather than passing
        // silently — a skipped assertion is not a satisfied one.
        let infinite = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"reV4"),
            componentManufacturer: u32::from_be_bytes(*b"TOGU"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        match component::find_component(&infinite) {
            None => {
                eprintln!("SKIP: TAL Reverb 4 not installed; the Unbounded arm is unexercised here")
            }
            Some(comp) => {
                let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
                    .expect("Should create instance");
                inner.initialize().expect("Should initialize");
                let seconds = inner
                    .get_tail_time()
                    .expect("TAL Reverb 4 answers the tail property");
                assert!(
                    !seconds.get().is_finite(),
                    "TAL Reverb 4 is the corpus's infinite-tail unit; it reported {seconds:?}"
                );

                let mapped = match () {
                    _ if !seconds.get().is_finite() => PluginTail::Unbounded,
                    _ => match seconds.to_samples_ceil(44_100.0) {
                        s if s == Samples::ZERO => PluginTail::None,
                        s => PluginTail::Finite(s),
                    },
                };
                assert_eq!(mapped, PluginTail::Unbounded);
                // The point of the whole type: converting first would have made
                // this `None`, and a bounce would truncate the reverb entirely.
                assert_ne!(mapped, PluginTail::None);
                assert_eq!(seconds.to_samples_ceil(44_100.0), Samples::ZERO);
            }
        }
    }

    #[test]
    fn test_au_enumerate_and_load() {
        use tutti_au_host::component;

        let effects =
            component::enumerate_components_of_type(tutti_au_host::component::AuType::Effect);
        assert!(
            !effects.is_empty(),
            "Should find at least one AU effect on macOS"
        );

        eprintln!("Found {} AU effects", effects.len());
        for (i, info) in effects.iter().take(5).enumerate() {
            eprintln!("  [{}] {} ({})", i, info.name, info.manufacturer);
        }
    }

    #[test]
    fn test_au_parameter_list_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        // Use Apple's AUDelay directly
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let params = au.get_parameter_list();
        assert!(
            !params.is_empty(),
            "AUDelay should have parameters via PluginInstance trait"
        );
    }

    #[test]
    fn test_au_process_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;
        use tutti_plugin::server::{AudioBuffer as TuttiAudioBuffer, AudioBufferMut};

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = TuttiAudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        let _output = au.process(AudioBufferMut::F32(buffer), &ctx);
        // No crash is the assertion
    }

    /// The live half of the direct path: `PluginParams` is normalized for every
    /// format, so a write of `1.0` must reach AUDelay's cutoff as 22050 Hz and
    /// read back as `1.0` — not as the 1 Hz that writing the normalized value
    /// straight through would set.
    ///
    /// Goes through the trait rather than `ParamBounds` so it covers the
    /// range-table lookup too: a `get`/`set` pair that agreed with each other
    /// but used the wrong bounds would pass a pure-unit test and fail here.
    #[test]
    fn direct_parameter_path_is_normalized_end_to_end() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        // The cutoff parameter is the one whose range makes the bug audible.
        let &(cutoff_id, bounds) = param_ranges
            .iter()
            .find(|(_, b)| b.min >= 10.0 && b.max >= 20_000.0)
            .expect("AUDelay declares a wide-range cutoff");

        let mut au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };
        let addr = ParamAddress::Opaque(cutoff_id.into());

        au.set_parameter(addr, Normalized::new(1.0));
        let plain = parameters::get(au.inner.raw_unit(), cutoff_id).expect("cutoff is readable");
        assert!(
            (plain - bounds.max).abs() < 1.0,
            "normalized 1.0 must set the top of {:?}, got {plain}",
            (bounds.min, bounds.max)
        );
        assert!(
            plain > 2.0,
            "a plain {plain} means the normalized value went through unscaled — \
             the inaudible-filter bug"
        );
        assert!((au.get_parameter(addr) - 1.0).abs() < 1e-3);

        au.set_parameter(addr, Normalized::new(0.0));
        assert!((au.get_parameter(addr)).abs() < 1e-3);
    }

    #[test]
    fn test_au_state_roundtrip_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let state = au.get_state().expect("save should succeed");
        assert!(!state.is_empty(), "State should not be empty");

        au.set_state(&state).expect("restore should succeed");
    }

    /// Pure unit half: the normalized→plain map itself.
    ///
    /// The bug was writing the normalized value straight through, which is
    /// equivalent to `to_plain` being the identity. These endpoints are exactly
    /// where identity and the correct map differ, and they use AUDelay's real
    /// declared ranges.
    #[test]
    fn to_plain_maps_onto_the_declared_range_not_identity() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.to_plain(0.0), 10.0);
        assert_eq!(cutoff.to_plain(1.0), 22_050.0);
        assert_eq!(cutoff.to_plain(0.5), 11_030.0);
        // The old code sent 1.0 here — 1 Hz, an inaudible filter.
        assert_ne!(cutoff.to_plain(1.0), 1.0);

        // Negative minima (AUDelay Feedback is [-99.9, 99.9]) must map too; the
        // old clamp to [0,1] made the entire negative half unreachable.
        let feedback = ParamBounds {
            min: -99.9,
            max: 99.9,
        };
        assert!((feedback.to_plain(0.0) - -99.9).abs() < 1e-3);
        assert!(feedback.to_plain(0.5).abs() < 1e-3);
        assert!((feedback.to_plain(1.0) - 99.9).abs() < 1e-3);

        // Out-of-range input is clamped to the declared endpoints, never past.
        assert_eq!(cutoff.to_plain(-5.0), 10.0);
        assert_eq!(cutoff.to_plain(9.0), 22_050.0);

        // A degenerate range yields `min` rather than NaN/inf.
        let degenerate = ParamBounds { min: 3.0, max: 3.0 };
        assert_eq!(degenerate.to_plain(0.5), 3.0);
    }

    /// The live-path half: this `to_plain`'s return value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so a NaN escaping here is a NaN
    /// in a live filter coefficient.
    ///
    /// `normalized` comes over IPC and the bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so neither is trusted. Clamping does not
    /// substitute for the check: `f32::clamp` returns NaN for NaN, and `max <= min`
    /// is `false` when either bound is NaN.
    #[test]
    fn nan_never_reaches_a_live_au_parameter() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };

        assert!(
            cutoff.to_plain(f64::NAN).is_finite(),
            "a NaN automation point must not reach AudioUnitSetParameter"
        );
        assert_eq!(cutoff.to_plain(f64::INFINITY), 22_050.0);
        assert_eq!(cutoff.to_plain(f64::NEG_INFINITY), 10.0);

        // Bounds the AU itself reported as non-finite.
        for (min, max) in [
            (f32::NAN, 1.0),
            (0.0, f32::NAN),
            (f32::NEG_INFINITY, 1.0),
            (0.0, f32::INFINITY),
        ] {
            let broken = ParamBounds { min, max };
            for v in [0.0, 0.5, 1.0, f64::NAN] {
                assert!(
                    broken.to_plain(v).is_finite(),
                    "to_plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
        }
    }

    #[test]
    fn lookup_bounds_finds_ids_in_a_sorted_table() {
        let table = vec![
            (2u32, ParamBounds { min: 0.0, max: 1.0 }),
            (
                7u32,
                ParamBounds {
                    min: 10.0,
                    max: 22_050.0,
                },
            ),
        ];
        assert_eq!(lookup_bounds(&table, 7).map(|b| b.max), Some(22_050.0));
        assert_eq!(lookup_bounds(&table, 2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(lookup_bounds(&table, 3).is_none());
    }

    /// Live half: drive a real AU's automation path, assert the unit holds the
    /// native value, then assert the read path inverts it.
    ///
    /// Full-scale automation must land on the parameter's declared MAXIMUM, not
    /// on `1.0` — that is the denormalization. And `get_parameter` must answer
    /// `1.0` rather than the maximum — that is the `PluginParams` contract,
    /// which is normalized for every format. Both directions in one test
    /// because either alone can pass while the pair is inconsistent.
    #[test]
    fn param_automation_denormalizes_and_reads_back_normalized() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;
        use tutti_plugin::server::{
            AudioBuffer as TuttiAudioBuffer, AudioBufferMut, ParameterChanges, ParameterQueue,
        };

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        // Pick a writable parameter with a genuinely wide plain range — one
        // whose max is far from 1.0, so identity-vs-denormalized is decidable.
        let target = au
            .get_parameter_list()
            .into_iter()
            .find(|p| {
                p.range.bounds().is_some_and(|(_, max)| max > 2.0)
                    && p.flag(ParamFlags::READ_ONLY) == Some(false)
            })
            .expect("AUDelay should expose a wide-range writable parameter");
        let (min, max) = target
            .range
            .bounds()
            .expect("AU declares a plain range for every parameter");

        let num_samples = 64;
        for (normalized, expected) in [
            (1.0f64, max),
            (0.0f64, min),
            (0.5f64, min + 0.5 * (max - min)),
        ] {
            let mut changes = ParameterChanges::new();
            // The queue is keyed by the same `ParamAddress` the descriptor
            // carries, so nothing here has to restate that AU is opaque.
            let mut queue = ParameterQueue::new(target.id);
            queue.add_point(0, normalized);
            changes.add_queue(queue);

            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
            let buffer = TuttiAudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };
            let mut ctx = ProcessContext::new();
            ctx.param_changes = Some(&changes);
            au.process(AudioBufferMut::F32(buffer), &ctx)
                .expect("process should succeed");

            // Tolerance scales with the range: AU stores parameters as f32, so
            // a 22 kHz range round-trips to ~1e-4 relative precision.
            let tolerance = (max - min).abs() * 1e-4;

            // The subject of this test: the automation point was normalized,
            // and the AU must hold the *native* value. Read straight off the
            // unit rather than through `get_parameter`, which now normalizes —
            // going through it would assert the identity of two conversions and
            // pass even if both were wrong.
            let opaque = target.id.opaque().expect("AU ids are opaque").get();
            let native =
                f64::from(parameters::get(au.inner.raw_unit(), opaque).expect("param is readable"));
            assert!(
                (native - expected).abs() <= tolerance,
                "param {} ('{}'): normalized {normalized} should reach the AU as \
                 {expected} in native units, got {native} (range [{min}, {max}])",
                target.id,
                target.name,
            );

            // And the read path inverts it: `PluginParams` is normalized for
            // every format, so what went in comes back out.
            let read_back = au.get_parameter(target.id);
            assert!(
                (read_back - normalized).abs() <= 1e-4,
                "param {} ('{}'): should read back as the normalized {normalized}, \
                 got {read_back}",
                target.id,
                target.name,
            );
        }
    }

    /// The display seam denormalizes before asking the AU, and reports the AU's
    /// silence as `None` rather than inventing a label.
    ///
    /// **This asserts an absence, and that is the honest measurement.** No Apple
    /// AU on macOS 15.6 implements `ParameterStringFromValue` — probed across 15
    /// units × every parameter × several values in `tutti-au-host`'s
    /// `au_param_display` suite, which pins the same thing one layer down. So
    /// there is no Apple fixture on this machine that can demonstrate a *string*
    /// coming back, and a test asserting `"Hz"` appears would fail against every
    /// unit macOS ships.
    ///
    /// What is still worth pinning here, and what would otherwise be untested:
    ///
    /// - The seam **asks at all** — it reaches the AU rather than short-circuiting.
    /// - It asks about the **right value**. The conversion is the half most
    ///   likely to be wrong and the half that fails silently: a third-party AU
    ///   answering for a `[10, 22050]` Hz cutoff would describe `1 Hz` where the
    ///   caller named full scale, and the string would look perfectly plausible.
    ///   Pinned below by driving the identical conversion the impl uses and
    ///   checking it lands where `set_parameter` puts the same input — so if the
    ///   two ever diverge, this fails even while the AU keeps answering `None`.
    /// **Not covered here, deliberately:** that an address of the wrong model
    /// (a VST2 `Index`) addresses nothing. It cannot be — since every AU answers
    /// `None`, no input distinguishes "refused the address" from "the AU
    /// declined", and an assertion on it passes even with the guard replaced by
    /// a raw `Index → ParamId` coercion. That mutation was run and survived, so
    /// the assertion was removed rather than left claiming coverage it does not
    /// have. The guard is covered where it is decidable: on VST3 and CLAP, whose
    /// plugins do answer.
    #[test]
    fn au_parameter_text_denormalizes_and_reports_absence_honestly() {
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let Some(inner) = open_au(K_AUDIO_UNIT_TYPE_EFFECT, b"dely") else {
            eprintln!("AUDelay unavailable; skipping");
            return;
        };
        let param_ranges = read_param_ranges(inner.raw_unit());
        let mut au = AuInstance {
            watch: None,
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let target = au
            .get_parameter_list()
            .into_iter()
            .find(|p| p.range.bounds().is_some_and(|(_, max)| max > 2.0))
            .expect("AUDelay should expose a wide-range parameter");
        let (min, max) = target.range.bounds().expect("AU declares a plain range");
        let opaque = target.id.opaque().expect("AU ids are opaque").get();

        // The impl's conversion, driven here against the same table. Full scale
        // must reach the AU as `max`, not as the raw `1.0` — that is the bug
        // this guards, and the one a returned string could not reveal.
        let bounds = lookup_bounds(&au.param_ranges, opaque).expect("the range table holds it");
        let plain_at_full = bounds.to_plain(1.0);
        let tolerance = ((max - min).abs() * 1e-4) as f32;
        assert!(
            (f64::from(plain_at_full) - max).abs() <= f64::from(tolerance),
            "param {} ('{}'): normalized 1.0 must denormalize to {max}, got \
             {plain_at_full} — a text query would then describe the wrong value",
            target.id,
            target.name,
        );

        // And that is the same value `set_parameter` writes, so the string a
        // third-party AU returns describes the value the parameter is at.
        au.set_parameter(target.id, Normalized::new(1.0));
        let after = parameters::get(au.inner.raw_unit(), opaque).expect("param is readable");
        assert!(
            (after - plain_at_full).abs() <= tolerance,
            "the text path and the write path must denormalize identically: \
             write landed at {after}, text would ask about {plain_at_full}",
        );

        // The AU is asked, and declines — see the doc comment.
        assert_eq!(
            au.parameter_text(target.id, Normalized::new(1.0)),
            None,
            "param {} ('{}'): no Apple AU implements ParameterStringFromValue. \
             A Some() here means macOS gained the property — re-measure and \
             tighten this into a real round-trip rather than relaxing it.",
            target.id,
            target.name,
        );
        assert_eq!(
            au.parameter_value_from_text(target.id, "1000 Hz"),
            None,
            "nor ParameterValueFromString — a fabricated number here would be \
             written into the user's preset",
        );
    }
}

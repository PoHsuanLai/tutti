//! Loading, initialization and the plugin's requests and notifications.

use super::*;
use tutti_plugin_types::MainThread;

impl Vst3Loaded {
    /// Reads a plugin's metadata without initializing it.
    ///
    /// Loads the library and reads the factory's class info, but never calls
    /// `initialize()` or `setActive()`. Safe for plugins that would otherwise
    /// pop license dialogs or hit the network during a full load.
    ///
    /// The bus counts, the MIDI buses and the 64-bit answer are not read: the
    /// SDK allows those queries (`getBusCount`, `canProcessSampleSize`) only on
    /// an initialized component, so the result carries none, no MIDI and
    /// 32-bit only, whatever the plugin has. A loaded instance's
    /// [`info`](tutti_plugin_types::Control::info) is the answer for them.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`] if the
    /// file is missing, the DSO can't be opened, the factory is empty, no
    /// audio class is found, or the component cannot be created.
    pub fn probe(path: impl AsRef<Path>) -> Result<PluginInfo> {
        let path = path.as_ref();
        check_exists(path)?;
        let library = Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = find_audio_class(&library, path)?;
        library
            .module()
            .check_instantiable(&class.cid)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Instantiation,
                code: e.code(),
            })?;
        Ok(build_plugin_info_raw(&library, &class))
    }

    /// Loads a VST3 plugin: the bundle's first audio class, or the one
    /// [`LoadOptions::class`] names. It stays loaded (not activated) until
    /// [`Loaded::activate`](tutti_plugin_types::Loaded::activate), and a GUI-only
    /// host never activates it.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`] if the file is missing, the DSO can't
    /// be opened, the factory is empty, no audio class is found (the message
    /// lists what the bundle does export when a named class is missing, since
    /// a near-miss on a display name is the likely cause), or the component
    /// cannot be created; returns [`Vst3Error::PluginError`] if
    /// `IPluginBase::initialize` fails.
    pub fn load(main: &MainThread, path: impl AsRef<Path>, options: LoadOptions) -> Result<Self> {
        Self::reap_retired();
        let path = path.as_ref();
        let class_name = options.class.as_deref();
        check_exists(path)?;
        let library = Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = match class_name {
            Some(wanted) => find_audio_class_named(&library, path, wanted)?,
            None => find_audio_class(&library, path)?,
        };
        let plugin = library
            .module()
            .create_plugin(&class.cid)
            .map_err(|e| match e {
                CreateError::NoAudioProcessor => Vst3Error::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Instantiation,
                    cause: LoadCause::NoAudioProcessor,
                },
                CreateError::Create(e) => Vst3Error::PluginError {
                    stage: LoadStage::Instantiation,
                    code: e.code(),
                },
                _ => Vst3Error::PluginError {
                    stage: LoadStage::Instantiation,
                    code: 0,
                },
            })?;
        // The buses and the 64-bit answer are read once the component is
        // initialized (`reconcile_bus_counts`).
        let info = build_plugin_info_raw(&library, &class);

        let load = match options.class {
            Some(class) => tutti_plugin_types::Load::new().plugin(class),
            None => tutti_plugin_types::Load::new(),
        };
        let origin = tutti_plugin_types::Origin::new(path, load);
        let mut loaded = Self::assemble(library, plugin, info, origin);
        loaded.initialize(main)?;
        // Whether there is an editor is asked of the plugin here, on the main
        // thread; `Control::info` has no token and reads the answer.
        let _ = loaded.descriptor.set(loaded.measured_descriptor(main));
        Ok(loaded)
    }

    /// Builds `Self` from an already-created plugin. No side effects — the
    /// caller runs [`initialize`](Self::initialize).
    pub(super) fn assemble(
        library: Arc<Library>,
        plugin: Plugin,
        info: PluginInfo,
        origin: tutti_plugin_types::Origin,
    ) -> Self {
        Self {
            _library: library,
            info,
            midi_learn: MidiLearnConsumer::new(plugin.midi_learn()),
            plugin,
            origin,
            descriptor: OnceLock::new(),
            active: Activation::default(),
            carried: Vec::new(),
            midi_cc: Arc::new(ArcLend::new(MidiCcCell(MidiCcMapping::empty()))),
        }
    }

    /// Metadata snapshot (id, name, vendor, bus counts, MIDI and f64 support).
    pub fn plugin_info(&self) -> &PluginInfo {
        &self.info
    }

    /// Hands every queued plugin notification to `on_event`, oldest first,
    /// and returns the restart requests received since the last look.
    ///
    /// This is the single polling entry point for everything the plugin reports
    /// asynchronously between process calls:
    /// - **the return value** — the coalesced [`RestartOutcome`] of every
    ///   `restartComponent` since the last poll (or
    ///   [`take_restart`](Self::take_restart)). Nothing in it has been acted on:
    ///   `kIoChanged` and `kLatencyChanged` both need a deactivate/reactivate
    ///   cycle this `&mut Vst3Loaded` cannot run, so the caller does it (the
    ///   core plans it from the same flags, as
    ///   [`Control::take_requests`](tutti_plugin_types::Control::take_requests) gives them), then reads
    ///   [`latency_samples`](Self::latency_samples).
    /// - **`on_event`** — each [`PluginNotification`] in arrival order across
    ///   all three kinds: `BeginEdit`/`PerformEdit`/`EndEdit`/… edits, `IProgress`
    ///   reports for a host-drawn progress bar, and `IUnitHandler` unit /
    ///   program-list / unit-by-bus changes.
    ///
    /// Restart requests carry no position among the events: they are cumulative
    /// and stored as one flag word, taken before the queue is drained, so an
    /// edit and a restart from the same gesture arrive in the same poll or the
    /// next, in no guaranteed order.
    ///
    /// Collects nothing: a caller that keeps events supplies its own storage.
    /// Notifications past [`NOTIFICATION_CAPACITY`](crate::NOTIFICATION_CAPACITY)
    /// unread are dropped and counted in
    /// [`dropped_notifications`](Self::dropped_notifications), and the next
    /// poll reports
    /// [`notifications_overflowed`](RestartOutcome::notifications_overflowed).
    ///
    /// Also forwards any MIDI-learn CCs the audio thread captured to the
    /// plugin's `IMidiLearn`, which the SDK requires on this thread.
    ///
    /// Must be called on the main thread; not while inside
    /// [`Vst3Processor::process`](crate::Vst3Processor::process).
    pub fn poll_plugin_notifications(
        &mut self,
        _main: &MainThread,
        mut on_event: impl FnMut(PluginNotification),
    ) -> RestartOutcome {
        // `kIoChanged` is deliberately NOT acted on here. The spec requires
        // deactivate → re-ask → reactivate (`ivsteditcontroller.h:125-127`),
        // and this method holds a `&mut Vst3Loaded`, which cannot deactivate
        // anything — the same constraint `reload_requested` is surfaced for.
        //
        // Do not call `reconcile_bus_counts()` from here. It reads fine until
        // you notice the Controller `DerefMut`s to this type: the production
        // caller polls on a live active instance, so the re-enumeration would
        // run mid-activation. The activation sequence reconciles.
        let mut restart = self.take_restart();
        while let Some(event) = self.plugin.handler().pop_notification() {
            on_event(event);
        }
        // After the drain, so a drop made before it is reported now rather
        // than one poll late.
        restart.notifications_overflowed = self.plugin.handler().take_overflowed();
        // Forward any live CCs the audio thread captured for MIDI-learn to the
        // plugin's IMidiLearn on this (main) thread, as the SDK requires. No-op
        // unless learn is armed and the plugin implements IMidiLearn.
        self.midi_learn.forward_pending();
        restart
    }

    /// The coalesced [`RestartOutcome`] of every `restartComponent` since the
    /// last take (or poll), leaving none pending. Reads one atomic word:
    /// wait-free, allocation-free, and touches neither the notification queue
    /// nor the plugin, so it is the restart poll for a caller on an audio
    /// thread (the plugin server polls it between blocks).
    ///
    /// Queued edits, progress and unit news stay queued for
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications).
    pub fn take_restart(&self) -> RestartOutcome {
        RestartOutcome::from_flags(RestartFlags::from_bits(
            self.plugin.handler().take_restart_bits(),
        ))
    }

    /// Drops every queued notification unread, and clears the overflow flag
    /// with them, returning how many there were. Lock-free and
    /// allocation-free (no notification owns heap memory), so a caller on an
    /// audio thread that has no use for edits, progress or unit news — the
    /// plugin server — keeps the queue from filling with a backlog.
    ///
    /// Leaves the restart flags and pending MIDI-learn CCs alone.
    pub fn discard_notifications(&self) -> usize {
        let mut n = 0;
        while self.plugin.handler().pop_notification().is_some() {
            n += 1;
        }
        self.plugin.handler().take_overflowed();
        n
    }

    /// How many notifications are queued, unread.
    pub fn pending_notifications(&self) -> usize {
        self.plugin.handler().pending_notifications()
    }

    /// Notifications the plugin sent that were dropped because
    /// [`NOTIFICATION_CAPACITY`](crate::NOTIFICATION_CAPACITY) were already
    /// unread. Cumulative over the instance's life; a rising count means the
    /// host polls too rarely (or not at all) for this plugin.
    pub fn dropped_notifications(&self) -> u64 {
        self.plugin.handler().dropped_notifications()
    }

    /// Arms or disarms VST3 MIDI learn (`IMidiLearn`).
    ///
    /// While armed, the realtime path captures incoming MIDI CCs and the next
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) forwards
    /// them to the plugin's `IMidiLearn::onLiveMIDIControllerInput` — letting the
    /// plugin bind the moved controller to whatever parameter the user is
    /// editing in its own UI. Typically: arm on right-click-knob → "MIDI learn",
    /// let the user move a controller, observe the resulting
    /// `kMidiCCAssignmentChanged` restart, then disarm.
    ///
    /// No observable effect if the plugin doesn't implement `IMidiLearn`.
    pub fn arm_midi_learn(&mut self, _main: &MainThread, armed: bool) {
        self.midi_learn.arm(armed);
    }

    /// Whether VST3 MIDI learn is currently armed. See
    /// [`arm_midi_learn`](Self::arm_midi_learn).
    #[must_use]
    pub fn is_midi_learn_armed(&self) -> bool {
        self.midi_learn.is_armed()
    }

    pub(super) fn initialize(&mut self, main: &MainThread) -> Result<()> {
        self.plugin
            .initialize_component(main)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: e.code(),
            })?;

        for (direction, name) in [
            (BusDirection::Input, "input"),
            (BusDirection::Output, "output"),
        ] {
            if let Some((index, channels)) =
                self.plugin.first_unrepresentable_audio_bus(main, direction)
            {
                return Err(Vst3Error::BusTooWide {
                    direction: name,
                    index,
                    channels,
                });
            }
        }

        self.reconcile_bus_counts(main);

        self.plugin.read_process_context_requirements();

        let separate_controller = self.plugin.has_separate_controller();

        // A separate controller is initialized with the same host context, and
        // wired to the component. Neither answer is fatal: the plugin simply
        // runs unconnected.
        if separate_controller {
            let _ = self.plugin.initialize_controller(main);
            let _connected = self.plugin.connect_controller(main);
        }

        self.plugin.attach_component_handler(main);

        if separate_controller && let Ok(state) = self.read_component_state(main) {
            self.push_controller_state(main, &state);
        }

        Ok(())
    }

    /// The live reference count on the host-context object. Test-only seam for
    /// the load-path refcount assertion in `tests/vst3_conformance.rs`: the
    /// hand-off contract below is otherwise invisible from outside, and a leak
    /// of one reference per load is not observable any other way.
    ///
    /// Reads the count without moving it — `add_ref` returns the value *after*
    /// incrementing, so the matching `release` restores it and the count before
    /// the pair is one less.
    #[cfg(feature = "conformance")]
    pub fn host_context_refcount(&self) -> usize {
        self.plugin.host_context_refcount()
    }

    /// Destroys, on the calling thread, the plugins whose last holder was a
    /// [`Vst3Processor`](crate::Vst3Processor) dropped after its Controller, and
    /// says how many. A Processor does not tear a plugin down on the thread it
    /// renders on; it retires the plugin here instead. [`load`](Self::load),
    /// [`activate_typed`](Self::activate_typed) and the Controller's
    /// [`flush_edits`](tutti_plugin_types::Control::flush_edits) reap too, so a
    /// host that ticks never needs to call it.
    ///
    /// Control thread.
    pub fn reap_retired() -> usize {
        crate::host::shell::reap()
    }

    /// Whether the plugin is activated: `setActive(1)` has run and
    /// `setActive(0)` has not.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_active()
    }

    /// Re-queries the `IMidiMapping` CC to parameter table from the controller
    /// and publishes it, so the Processor routes against it from its next
    /// block. Call it when the plugin raised
    /// [`Request::MidiCcAssignmentChanged`](tutti_plugin_types::Request::MidiCcAssignmentChanged). Allocates.
    pub fn rebuild_midi_cc_mapping(&mut self, main: &MainThread) {
        self.midi_cc
            .publish(MidiCcCell(MidiCcMapping::query(main, &self.plugin)));
        self.midi_cc.reclaim();
    }
}

pub(super) fn check_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Scanning,
            cause: LoadCause::FileNotFound,
        });
    }
    Ok(())
}

/// One factory class — the handful of fields that must stay together when
/// walking the `IPluginFactory`. Returned by [`find_audio_class`].
pub(in crate::host) struct AudioClass {
    /// The class id, as `IPluginFactory::createInstance` takes it.
    pub cid: [i8; 16],
    /// Display name from `PClassInfo::name`.
    pub name: String,
    /// Per-class vendor, when declared. `None` falls back to the factory's.
    pub vendor: Option<String>,
    /// Per-class version string, when declared.
    pub version: Option<String>,
    /// `|`-delimited subcategories (`"Fx|Reverb"`), when declared. `None` when
    /// the factory is v1-only, which cannot report them at all.
    pub sub_categories: Option<String>,
}

pub(super) fn ensure_has_classes(library: &Library, path: &Path) -> Result<()> {
    if library.count_classes() == 0 {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            cause: LoadCause::NoClasses,
        });
    }
    Ok(())
}

/// Assembles `PluginInfo` from a class descriptor and the factory. Used by
/// both [`Vst3Loaded::probe`] (which never creates the processor) and
/// [`Vst3Loaded::load`].
///
/// The bus counts, the MIDI buses and the 64-bit answer are left at their
/// empty defaults: the SDK allows those queries only on an initialized
/// component. `probe` never initializes, and the load path reads them after
/// `initialize` (`Vst3Loaded::reconcile_bus_counts`).
pub(super) fn build_plugin_info_raw(library: &Library, class: &AudioClass) -> PluginInfo {
    // The class's own vendor wins over the factory's: `ipluginbase.h:357`
    // documents the field as "overwrite vendor information from factory info".
    // They differ on distributor-published bundles, where the factory names the
    // distributor and the class names the maker — reading only the factory
    // credits the wrong one.
    let vendor = class.vendor.clone().unwrap_or_else(|| {
        library
            .factory_info()
            .map(|info| info.vendor)
            .unwrap_or_default()
    });
    // The real version string lives on the class (e.g. "1.0.0.512",
    // Major.Minor.Subversion.Build). A plugin that declares none falls back to
    // the "1.0.0" placeholder rather than showing an empty version field.
    let version = class.version.clone().unwrap_or_else(|| "1.0.0".to_string());

    PluginInfo::new(
        format!("vst3.{}", cid_to_string(&class.cid.map(|b| b as u8))),
        class.name.clone(),
    )
    .vendor(vendor)
    .version(version)
    .audio_io(0, 0)
    .bus_channels(Vec::new(), Vec::new())
    .midi(false)
    .midi_output(false)
    .f64_support(false)
    .sub_categories(class.sub_categories.clone())
}

/// The audio class named `wanted`, or a `LoadFailed` naming what is on offer.
pub(super) fn find_audio_class_named(
    library: &Library,
    path: &Path,
    wanted: &str,
) -> Result<AudioClass> {
    let audio: Vec<_> = (0..library.count_classes())
        .filter_map(|i| library.class_info(i).ok())
        .filter(|info| info.category.contains("Audio"))
        .collect();

    audio
        .iter()
        .find(|info| info.name == wanted)
        .map(|info| AudioClass {
            cid: info.cid,
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: info.version.clone(),
            sub_categories: info.sub_categories.clone(),
        })
        .ok_or_else(|| Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            cause: LoadCause::ClassNotFound {
                wanted: wanted.to_string(),
                offered: audio.iter().map(|i| i.name.clone()).collect(),
            },
        })
}

pub(super) fn find_audio_class(library: &Library, path: &Path) -> Result<AudioClass> {
    let count = library.count_classes();
    (0..count)
        .find_map(|i| {
            let info = library.class_info(i).ok()?;
            if !info.category.contains("Audio") {
                return None;
            }
            Some(AudioClass {
                cid: info.cid,
                name: info.name,
                vendor: info.vendor,
                version: info.version,
                sub_categories: info.sub_categories,
            })
        })
        .ok_or_else(|| Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            cause: LoadCause::NoAudioClass,
        })
}

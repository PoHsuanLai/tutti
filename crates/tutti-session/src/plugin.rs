//! Hosted plugins in the session (feature `plugin`; design 015 §4.2-§4.4):
//! the load as a job ([`PluginLoad`]), the add ([`Session::add_plugin`]),
//! the view of the record the session keeps for each one ([`Plugin`]), and the
//! work of [`Phase::Plugins`](crate::Phase::Plugins) and
//! [`Phase::RenderMode`](crate::Phase::RenderMode).
//!
//! # The record
//!
//! Each plugin the graph holds (`Graph::insert_plugin`,
//! [`Session::add_plugin`], [`Graph::host_plugin`](crate::Graph::host_plugin)) has a record,
//! keyed by its [`NodeKey`] and lent as a [`Plugin`] ([`Session::plugin`]): the [`PluginRef`] it is, its handle, its
//! controls (an out-of-process plugin's latency and meter cells), its
//! liveness, whether it was told the render mode, its last good state
//! ([`state`](Plugin::state)), and its parameter mirror
//! ([`params`](Plugin::params)). A crossfade to another plugin (`Graph::crossfade_plugin`,
//! `replace_plugin`) carries the record over to the incoming plugin, with
//! nothing of the outgoing plugin's state (its last good state, an in-flight
//! snapshot job, its mirror); replacing it with a native node
//! (`Graph::replace`, `crossfade`) or removing the node drops it.
//!
//! # The main thread
//!
//! An in-process plugin's handle (`PluginHandle`, VST2 today) works from any
//! thread, which is what the snapshot and parameter-read jobs below need: a
//! call off the main thread is posted to it (`tutti_plugin_types::defer_to_main`)
//! and the caller waits, for at most two seconds, for the main thread's next
//! tick to run it ([`MainSession::update`](crate::MainSession::update), or
//! `Phase::Collect` on the claiming thread, calls `run_deferred`). A host
//! that does not call either on its main thread must call
//! `tutti_plugin_types::run_deferred` from its main-thread loop, or those calls
//! answer "unavailable" at the deadline (`StateError::Unavailable`, `None`,
//! `false`). A host blocking its main thread on such a job (joining it there)
//! gets the same.
//!
//! What retires a record (a removal, a crossfade to another plugin or to a
//! native node, a reload's dead plugin) parks it in the graveyard instead of
//! dropping it, since the drop of a plugin's last owner may tear down an
//! editor. `MainSession::update` empties it; [`Session::retired_plugins`] counts
//! what waits, and `Phase::Plugins` warns once when more than a handful do.
//!
//! # `Phase::Plugins`
//!
//! Per plugin, in node order:
//!
//! - **Health.** `tutti_plugin_host::Liveness` observes the handle's status at
//!   `PhaseCx::now`: dead after at least `Liveness::DEATHS` consecutive
//!   unhealthy observations spanning at least the window. A dead plugin's
//!   node is removed ([`Session::remove`]) and reported once,
//!   [`PluginChange::Died`].
//! - **Reports.** What the plugin reported since the last update (its own edit
//!   of a parameter, a rescan, a new tail, a bus change) is queued where it
//!   was heard and becomes [`PluginEvent`]s here, in the order the plugin made
//!   them.
//! - **Latency.** The figure its node declares now
//!   (`PluginControls::declared_latency`) is handed to the graph when it
//!   moved (`Graph::refresh_node_latency`):
//!   [`PluginChange::LatencyChanged`].
//! - **Parameters.** The values the plugin reported (or the host set) go into
//!   its mirror, and (`modulation`) into the knob of the lane on that
//!   parameter when it has one (doc 014 §8.2). A mirror rebuilt by a crossfade
//!   to another plugin, or by a rescan the plugin announced, is read as a job
//!   the host places:
//!   [`SessionEvent::PluginParamsDue`],
//!   whose [`run`](PluginParamRead::run) makes one round trip per
//!   parameter, landed with [`Session::land_params`]. Until then the
//!   mirror is not ready, and a lane on the plugin waits for it.
//! - **Snapshots.** A healthy plugin's state is captured every
//!   [`SNAPSHOT_INTERVAL`] of `now` (`SessionBuilder::snapshot_interval`),
//!   as a job the host places: [`SessionEvent::PluginSnapshotDue`],
//!   whose [`run`](PluginSnapshot::run) is a blocking round trip, landed
//!   with [`Session::land_snapshot`]. One is in flight per plugin at most:
//!   the job holds the plugin's snapshot token, and a dropped job frees it.
//!
//! # Recovery
//!
//! With a [`RecoveryPolicy`] (the default; `SessionBuilder::plugin_recovery`),
//! an out-of-process plugin that dies, or hangs (stops answering blocks), is
//! reloaded at its own node instead of removed. The policy is the pure
//! [`Recovery`]; this module carries out what it decides:
//!
//! - **Hang.** `PluginHandle::block_counts` is observed each phase; a
//!   verdict ends the server (`PluginHandle::terminate`), so a hung plugin
//!   dies like a crashed one, and the load below never races a live server.
//! - **Death.** The node stays in the graph, silent, with its wiring and
//!   every lane fed to it; [`PluginChange::Reloading`].
//! - **Load.** When the backoff passes, [`SessionEvent::PluginReloadDue`]
//!   hands the host a [`PluginReload`] job: the existing [`PluginLoad`] with
//!   the last good state as its state. [`Session::land_reload`] swaps the
//!   loaded plugin in at the same node (`Graph::replace_plugin`) and keeps
//!   the record: the last good state, its age, the failure history.
//! - **Reported.** [`PluginChange::Recovered`] says whether the state
//!   came back and how old it was; a spent budget is
//!   [`PluginChange::GaveUp`] and the node is removed.
//!
//! An in-process plugin (VST2) is not recovered: a crash in it takes the
//! process, and a hang in it blocks the audio thread itself, which nothing
//! here observes (`block_counts` is zeros for it).
//!
//! # `Phase::RenderMode` (with `export`)
//!
//! Offline while an export token lives (§4.3), Realtime otherwise; each
//! plugin is told per `tutti_plugin_host::needs_announcement`: on a change of
//! mode, or if it was never told.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tutti_plugin::MainThread;

use tutti_graph::NodeKey;
use tutti_plugin::catalog::{
    AudioConfig, Catalog, Hosting, Plugin as HostPlugin, PluginRef, PluginState,
};
use tutti_plugin::error::StateError;
use tutti_plugin_host::{Liveness, Verdict};
use tutti_plugin_host::{
    NodeControls, Notice, PluginControls, PluginHandle, PluginStatus, Subscription,
};
use tutti_plugin_types::{
    BridgeError, EditorSize, Normalized, ParamEdit, ParamId, PluginFailure, PluginInvalidation,
    PluginRefresh, PluginTail,
};
use tutti_types::{SampleRate, Samples, Seconds};

use crate::plugin_diagnostics::Figures;
use crate::{
    Diagnostics, GiveUp, Key, Params, PluginBuses, PortCounts, Recovery, RecoveryAction,
    RecoveryEvent, RecoveryFailure, RecoveryPolicy, ReloadCause, Session, SessionEvent,
};

/// How often a healthy plugin's state is captured, by default: 8 s, the
/// 512 frames at 60 Hz `bevy-tutti` counted before (design 015 §4.4).
/// [`SessionBuilder::snapshot_interval`](crate::SessionBuilder::snapshot_interval)
/// sets another.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(8);

/// Which plugin to load, and the state to restore into it.
#[derive(Debug, Clone)]
pub struct PluginLoadRequest {
    plugin: PluginRef,
    state: Option<PluginState>,
}

impl PluginLoadRequest {
    /// A load of `plugin`: a scanned one (`PluginRef::from(&entry)`, which
    /// picks the plugin in a bundle that exports several) or a file's first
    /// ([`PluginRef::file`]).
    pub fn new(plugin: PluginRef) -> Self {
        Self {
            plugin,
            state: None,
        }
    }

    /// With a saved state restored once the plugin loads
    /// ([`StateView::save`] or [`StateView::last_good`], or a project's). A
    /// refusal by the plugin is logged, and the plugin loads at its defaults.
    ///
    /// # Errors
    ///
    /// [`StateError::OtherPlugin`] when `state` was saved from another plugin
    /// than the request's.
    pub fn restoring(mut self, state: PluginState) -> Result<Self, StateError> {
        state.check_for(&self.plugin)?;
        self.state = Some(state);
        Ok(self)
    }

    /// The plugin to load.
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The state to restore into it, if one was given.
    pub fn state(&self) -> Option<&PluginState> {
        self.state.as_ref()
    }
}

/// A plugin load, as a job (design 015 §4.2): `Send`, and
/// [`run`](Self::run) blocks (a `plugin-server` launch, half a second to
/// fifteen), so the host runs it off its control thread and hands the
/// result to [`Session::add_plugin`].
///
/// Made by [`Session::plugin_load`], at the session's rate.
#[must_use = "a load does nothing until it is run and added"]
pub struct PluginLoad {
    plugin: PluginRef,
    rate: SampleRate,
    config: AudioConfig,
    state: Option<PluginState>,
    /// A catalog's refusal ([`with_catalog`](Self::with_catalog)), landed by
    /// [`run`](Self::run) as the load's failure.
    refusal: Option<Box<BridgeError>>,
}

impl std::fmt::Debug for PluginLoad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoad")
            .field("plugin", &self.plugin)
            .field("rate", &self.rate)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PluginLoad {
    /// Bridge settings other than the defaults.
    pub fn with_config(mut self, config: AudioConfig) -> Self {
        self.config = config;
        self
    }

    /// This load under `catalog`: its audio settings
    /// ([`Catalog::audio_config`]) and its blacklist ([`Catalog::check`]),
    /// asked now and answered by [`run`](Self::run) (and so by
    /// [`add_plugin`](Session::add_plugin)) as the load's failure,
    /// [`BridgeError::Blacklisted`] with the recorded reason, before any
    /// server starts or library loads. The catalog is not borrowed past this
    /// call, so the load stays `Send`; a load that does not name a catalog
    /// consults no blacklist (a host offering to load a plugin anyway).
    pub fn with_catalog(mut self, catalog: &Catalog) -> Self {
        self.config = catalog.audio_config().clone();
        self.refusal = catalog.check(&self.plugin).err().map(Box::new);
        self
    }

    /// Instantiates at `rate` instead of the session's.
    pub fn at_rate(mut self, rate: SampleRate) -> Self {
        self.rate = rate;
        self
    }

    /// The plugin being loaded.
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's file.
    pub fn path(&self) -> &Path {
        self.plugin.path()
    }

    /// Whether the plugin is hosted in process (its ref's [`Hosting`]
    /// resolved by the format, `HostPlugin::hosting_of`: VST2 by default, any
    /// format built in on request): [`run`](Self::run) leaves it unopened, and
    /// [`Session::add_plugin`] opens it on the main thread, stalling that call
    /// for the library's init, the state restore and the parameter read. A
    /// hosting the format refuses is `false` here and the refusal is
    /// [`run`](Self::run)'s.
    pub fn opens_in_process(&self) -> bool {
        matches!(HostPlugin::hosting_of(&self.plugin), Ok(Hosting::InProcess))
    }

    /// Opens the plugin, restores the request's state, and (`modulation`)
    /// reads its parameter values, after the state: those are what a
    /// modulation lane's knob starts at. Any thread; it blocks.
    ///
    /// A plugin hosted in process ([`opens_in_process`](Self::opens_in_process))
    /// is not opened here: an in-process load is confined to the main thread,
    /// so the job returns it unopened and [`Session::add_plugin`] opens it
    /// there (a library load and an instance's init, with no subprocess to
    /// wait for).
    ///
    /// A load the catalog refuses, or whose hosting the format cannot have
    /// ([`BridgeError::NotOutOfProcess`] for VST2 asked to be served), is
    /// refused here, before anything is opened.
    pub fn run(mut self) -> PluginLoaded {
        let plugin = self.plugin.clone();
        let config = self.config.clone();
        let refused = self
            .refusal
            .take()
            .map(|refusal| *refusal)
            .or_else(|| HostPlugin::hosting_of(&self.plugin).err());
        let opened = if let Some(refusal) = refused {
            Opened::Done(Box::new(Open::refused(refusal)))
        } else if self.opens_in_process() {
            Opened::OnMain(self)
        } else {
            Opened::Done(Box::new(self.open(None)))
        };
        PluginLoaded {
            plugin,
            config,
            opened,
        }
    }

    /// [`run`](Self::run)'s open, on this thread. `main` is the proof that it is
    /// the main thread, which a plugin hosted in process needs.
    fn open(self, main: Option<&MainThread>) -> Open {
        // The state goes in **before the plugin is activated** (`open_with_state`):
        // a plugin may take its mode from the state at activation. A refusal
        // does not abort the load: a plugin at its defaults beats no plugin. But
        // it is not silent.
        let (plugin, restore) = match HostPlugin::open_with_state(
            main,
            &self.config,
            &self.plugin,
            self.rate,
            self.state.as_ref().map(PluginState::bytes),
        ) {
            Ok((plugin, restore)) => (Ok(plugin), restore),
            // The server died, or stopped answering, inside the step that
            // restores the state it was staged (`StateRestoreError`; a failure
            // before that step is a load failure and blames nothing): the state
            // is what the load is held against (`land_reload` withholds it).
            Err(e @ tutti_plugin_types::BridgeError::StateRestoreError(_))
                if self.state.is_some() =>
            {
                let reason = e.to_string();
                (Err(e), Some(Err(reason)))
            }
            Err(e) => (Err(e), None),
        };
        if let (Ok(plugin), Some(Err(e))) = (&plugin, &restore) {
            tracing::warn!(
                "{}: saved state was not restored ({e}); the plugin is loaded at its \
                 defaults",
                plugin.handle().name()
            );
        }
        let mirror = plugin.as_ref().ok().map(|plugin| {
            let (mut values, read) = tutti_plugin_host::PluginParamValues::start(plugin.handle());
            // A read the plugin did not answer (its list unavailable) leaves
            // the mirror not ready and goes to the hosted record, to be handed
            // out as a job.
            let unread = match read.run() {
                Ok(read) => {
                    values.land(read);
                    None
                }
                Err(read) => Some(*read),
            };
            Mirror { values, unread }
        });
        Open {
            plugin,
            restore,
            mirror,
        }
    }
}

/// A new plugin's parameter mirror, and the read that still has to fill it
/// when the load-time read was not answered.
struct Mirror {
    values: tutti_plugin_host::PluginParamValues,
    /// Becomes the hosted record's `param_read`, to be run as a job.
    unread: Option<tutti_plugin_host::ParamRead>,
}

/// An opened plugin (or why it did not open) and its parameter mirror.
struct Open {
    plugin: Result<HostPlugin, BridgeError>,
    /// The state restore, if one was asked for: the plugin's refusal, as text.
    restore: Option<Result<(), String>>,
    mirror: Option<Mirror>,
}

impl Open {
    /// A load that did not open: `why`.
    fn refused(why: BridgeError) -> Self {
        Self {
            plugin: Err(why),
            restore: None,
            mirror: None,
        }
    }
}

/// What a [`PluginLoad`]'s run did.
enum Opened {
    /// Opened (or failed to) on the job's thread. Boxed: a plugin is far
    /// larger than an unopened load.
    Done(Box<Open>),
    /// Hosted in process: opened by `add_plugin`, on the main thread.
    OnMain(PluginLoad),
}

/// What became of the state a reload was to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateRestore {
    /// The plugin took the snapshot, captured `age` before the failure was
    /// handled. Anything it changed since is lost.
    Restored {
        /// How old the snapshot was when the replacement loaded.
        age: Duration,
    },
    /// The plugin refused the snapshot and loads at its defaults.
    Refused {
        /// How old the snapshot was.
        age: Duration,
        /// The plugin's refusal.
        reason: String,
    },
    /// No snapshot had been captured yet: the replacement loads at its
    /// defaults.
    NoSnapshot,
    /// The snapshot was left out: the plugin died within
    /// [`RecoveryPolicy::state_probation`] of loading it last time (or while
    /// loading it), so it is taken for the cause. The replacement loads at
    /// its defaults, and the snapshot is gone from the record.
    Withheld {
        /// How old the snapshot was.
        age: Duration,
    },
}

/// A finished [`PluginLoad`]: the plugin, or why it did not load. `Send`;
/// [`Session::add_plugin`] takes it on the control thread.
#[must_use = "a loaded plugin goes into the graph with `Session::add_plugin`"]
pub struct PluginLoaded {
    plugin: PluginRef,
    config: AudioConfig,
    opened: Opened,
}

impl std::fmt::Debug for PluginLoaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginLoaded")
            .field("plugin", &self.plugin)
            .finish_non_exhaustive()
    }
}

impl PluginLoaded {
    /// The plugin that was loaded.
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's file.
    pub fn path(&self) -> &Path {
        self.plugin.path()
    }

    /// Why the load failed, if it did. `None` for a plugin hosted in
    /// process too, which [`Session::add_plugin`] opens: its error is
    /// `add_plugin`'s.
    pub fn error(&self) -> Option<&BridgeError> {
        match &self.opened {
            Opened::Done(open) => open.plugin.as_ref().err(),
            Opened::OnMain(_) => None,
        }
    }
}

/// What the session believes about a plugin's liveness: the debounce
/// between the engine's latched `PluginStatus` and a node removed. The public
/// reading is [`PluginHealth`] ([`Plugin::health`]).
#[derive(Debug, Clone, Default)]
pub(crate) enum PluginLiveness {
    /// Answering normally.
    #[default]
    Healthy,
    /// Recently unhealthy, not yet written off (`tutti_plugin_host::Liveness`).
    Failing,
    /// Dead or hung, and being replaced at its node (recovery on): the node
    /// stays, silent, until [`PluginChange::Recovered`] or
    /// [`PluginChange::GaveUp`].
    Recovering {
        /// Why it needs a replacement.
        cause: ReloadCause,
    },
    /// Written off: its node is being removed (recovery off, or its budget
    /// spent).
    Dead {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
}

/// How a hosted plugin is doing ([`Plugin::health`], design 029 section 2.6):
/// one reading of what the engine latched, the session's debounce and its
/// recovery.
///
/// The verdicts that remove the node (written off with recovery off, or
/// recovery's budget spent) are not here: the node and its view are gone in the
/// update that reaches them, and the events say so ([`PluginChange::Died`],
/// [`PluginChange::GaveUp`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PluginHealth {
    /// Answering normally.
    Running,
    /// Alive, but its blocks go unanswered: the bridge is abandoning them and
    /// has not yet declared a hang. With recovery off the session does not
    /// watch for this, and it is never read.
    Stalled,
    /// The engine reports it dead, and the session has not yet written it off
    /// (it waits out a debounce first). It then becomes
    /// [`Reloading`](Self::Reloading), or, with recovery off, its node is
    /// removed.
    Died {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
    /// Dead or hung, and a replacement is on its way to the same node: the node
    /// stays, silent.
    Reloading {
        /// Failures within the recovery budget's window so far, this one
        /// included.
        attempt: u32,
        /// Why it needs a replacement.
        cause: ReloadCause,
    },
}

/// What happened to a hosted plugin, as the session's
/// [`SessionEvent::Plugin`] reports it (design 029
/// section 3.3): the node, and what changed.
///
/// One stream carries both what a plugin reports (its own edit of a
/// parameter, a new tail) and every transition of its life (death, reload,
/// recovery), so a host reads one place. There is no callback slot to install
/// or clear; a host that needs a push from the thread that hears the plugin
/// subscribes to its handle (`PluginHandle::subscribe`, the low layer).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PluginEvent {
    /// The plugin's node. After a [`PluginChange::Died`] with recovery off, or
    /// a [`PluginChange::GaveUp`], it is no longer in the graph.
    pub node: NodeKey,
    /// What happened.
    pub change: PluginChange,
}

/// What changed in a hosted plugin ([`PluginEvent::change`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PluginChange {
    /// The plugin took hold of a parameter (its editor, or its own gesture):
    /// the start of an edit that ends with [`GestureEnded`](Self::GestureEnded).
    GestureBegan {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
    },
    /// The plugin moved one of its own parameters (its editor, a preset load,
    /// its own automation), inside a gesture or not. Every one is reported, in
    /// the order the plugin made them.
    ParamChanged {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
        /// Its normalized value.
        value: Normalized,
        /// Whether a host that records automation should record this edit
        /// (`false` for a CLAP value the plugin flagged `DONT_RECORD`).
        recorded: bool,
    },
    /// The plugin let go of a parameter.
    GestureEnded {
        /// The parameter, by the id the plugin reports.
        id: ParamId,
    },
    /// The plugin's parameter values changed wholesale (a preset load, an
    /// internal write-back): re-read them.
    ParamsRescanned,
    /// The plugin's parameter list or its titles, units or flags changed:
    /// re-read the descriptors.
    ParamTitlesChanged,
    /// The plugin's declared latency moved, and the graph took it: the next
    /// commit compensates it.
    LatencyChanged {
        /// Its latency now.
        latency: Samples,
    },
    /// The plugin reported a new tail. The node's own cell is already
    /// updated; a bounce already in flight keeps the figure it started with.
    TailChanged {
        /// The new tail, which may be bounded, unbounded or unreported.
        tail: PluginTail,
    },
    /// The plugin's bus layout changed: re-read it and rewire.
    IoChanged,
    /// The plugin asked its editor window to take a size (a zoom control, a
    /// collapsing panel), found while [`MainSession::update`](crate::MainSession::update)
    /// serviced the editor. Answer with
    /// [`EditorView::resize`](crate::EditorView::resize) after sizing the window
    /// it lives in. The size is in the platform's own pixels, as
    /// [`EditorSize`] says.
    EditorWantsSize(EditorSize),
    /// A hosted plugin was declared dead
    /// (`tutti_plugin_host::Liveness`) with recovery off: its node was
    /// removed. Reported once. With recovery on the death is
    /// [`Reloading`](Self::Reloading), and the node's removal
    /// [`GaveUp`](Self::GaveUp).
    Died {
        /// The engine's latched reason.
        cause: PluginFailure,
    },
    /// A hosted plugin died or hung and recovery is on
    /// ([`RecoveryPolicy`]): its node stays in the
    /// graph, silent, and a replacement loads after `delay`. Reported once per
    /// attempt.
    Reloading {
        /// Why it needs a replacement: the engine's latched cause, or a failed
        /// attempt's.
        cause: ReloadCause,
        /// Failures within the budget's window so far, this one included.
        attempt: u32,
        /// The wait before the load is handed out.
        delay: Duration,
    },
    /// A replacement is in the graph at the same node, with the same wiring.
    Recovered {
        /// Whether its state came back, and from how long before.
        state: StateRestore,
        /// The node's latency before the failure. PDC follows at the next
        /// commit.
        latency_before: Samples,
        /// The node's latency now.
        latency_after: Samples,
        /// Attempts it took.
        attempts: u32,
    },
    /// Recovery stopped: the failures spent the budget. The node is removed.
    GaveUp {
        /// Why.
        why: GiveUp,
        /// The last state worth keeping, for a host that offers the user a
        /// manual reload: the state a reload withheld, else the last good one.
        state: Option<PluginState>,
    },
}

/// `change` at `node`, as the session's event.
pub(crate) fn event(node: NodeKey, change: PluginChange) -> SessionEvent {
    SessionEvent::Plugin(PluginEvent { node, change })
}

/// `m`'s contents, whether or not a panic poisoned it: the queues hold plain values.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a plugin reported, waiting for the next `Phase::Plugins`.
pub(crate) type Notices = Arc<Mutex<Vec<Notice>>>;

/// Queues `notice` as if the plugin had reported it.
pub(crate) fn queue(notices: &Notices, notice: Notice) {
    locked(notices).push(notice);
}

/// A queue for what `handle` reports and the subscription that fills it.
fn hear(handle: &PluginHandle) -> (Notices, Subscription) {
    let notices = Notices::default();
    let queue = Arc::clone(&notices);
    let hearing = handle.subscribe(move |notice| locked(&queue).push(notice));
    (notices, hearing)
}

/// What `notice` is as an event, if it is one: a latency the plugin reports
/// is reported once, by the update that hands it to the graph; a rate refusal,
/// a crash and a reload are the plugin's health.
fn change_of(notice: Notice) -> Option<PluginChange> {
    Some(match notice {
        Notice::Param(ParamEdit::Begin(id)) => PluginChange::GestureBegan { id },
        Notice::Param(ParamEdit::End(id)) => PluginChange::GestureEnded { id },
        Notice::Param(ParamEdit::Change { id, value, record }) => PluginChange::ParamChanged {
            id,
            value,
            recorded: record,
        },
        Notice::Refresh(PluginRefresh::ParamValues) => PluginChange::ParamsRescanned,
        Notice::Refresh(PluginRefresh::ParamTitles) => PluginChange::ParamTitlesChanged,
        Notice::Invalidate(PluginInvalidation::Tail { tail }) => PluginChange::TailChanged { tail },
        Notice::Invalidate(PluginInvalidation::Io) => PluginChange::IoChanged,
        Notice::Invalidate(_) | _ => return None,
    })
}

/// What a removal or a replacement took out of the session, held until the
/// main thread drops it (`MainSession::update`).
///
/// A plugin record owns the plugin's control handle, and dropping the last
/// owner of an in-process plugin tears its editor down, a main-thread call.
/// `Session` is `Send` and has no main thread of its own, so whatever
/// retires a record parks it here instead of dropping it, and the view that
/// holds the proof empties the yard. A host that never claims the main
/// thread (a headless one) never empties it: [`len`](Self::len) is what
/// `Phase::Plugins` warns on past [`Graveyard::WARN_AT`].
#[derive(Default)]
pub(crate) struct Graveyard {
    /// Held to be dropped: a removed plugin's record, or the handle a
    /// crossfade replaced.
    retired: Vec<Box<dyn std::any::Any + Send>>,
    warned: bool,
}

impl Graveyard {
    /// How many retired plugins a session holds before it says so.
    pub(crate) const WARN_AT: usize = 16;

    pub(crate) fn bury(&mut self, record: HostedPlugin) {
        record.gate.end();
        self.retired.push(Box::new(record));
    }

    pub(crate) fn bury_handle(&mut self, handle: PluginHandle) {
        self.retired.push(Box::new(handle));
    }

    pub(crate) fn len(&self) -> usize {
        self.retired.len()
    }

    /// Drops everything in the yard, on the main thread. Returns how many.
    pub(crate) fn exhume(&mut self, _main: &MainThread) -> usize {
        let n = self.retired.len();
        self.retired.clear();
        self.warned = false;
        n
    }

    /// Warns once when the yard has grown past [`WARN_AT`](Self::WARN_AT).
    pub(crate) fn warn_if_piling(&mut self) {
        if self.retired.len() > Self::WARN_AT && !self.warned {
            self.warned = true;
            tracing::warn!(
                "{} retired plugins are waiting for the main thread: nothing has called \
                 `Session::main(..).update()` (a host with no main thread never frees them)",
                self.retired.len()
            );
        }
    }
}

/// What the session keeps for one hosted plugin. See the module docs.
pub(crate) struct HostedPlugin {
    /// Whether the editor was opened through the view and not closed since:
    /// what `MainSession::update` services.
    pub(crate) editor_open: bool,
    /// The plugin this is: from the load's ref, with the id the loaded plugin
    /// reports.
    plugin: PluginRef,
    pub(crate) handle: PluginHandle,
    pub(crate) controls: Option<PluginControls>,
    /// What the node counts and holds (its meter cell and delivery counters):
    /// every plugin the session added; `None` for one the host inserted itself.
    pub(crate) node: Option<NodeControls>,
    liveness: Liveness,
    status: PluginLiveness,
    /// Told a render mode since it was added (or crossfaded in).
    pub(crate) told: bool,
    snapshot: Option<PluginState>,
    /// When the snapshot's capture was asked for.
    snapshot_from: Option<Instant>,
    /// A snapshot left out of a reload ([`StateRestore::Withheld`]), kept for
    /// the host ([`StateView::withheld`]); later
    /// snapshots go to `snapshot` and never replace it.
    withheld: Option<PluginState>,
    /// Until when a plugin that has just restored a state is suspected of
    /// dying of it.
    probation_until: Option<Instant>,
    /// How this plugin was loaded, for a reload; `None`: not recoverable.
    origin: Option<Origin>,
    recovery: Option<Recovery>,
    reload: Reload,
    /// Held by the snapshot job in flight, if one is.
    snapshot_token: Arc<()>,
    /// When the last snapshot was asked for, or (until the first) when the
    /// plugin was first observed.
    snapshot_at: Option<Instant>,
    /// What the plugin reported since the last `Phase::Plugins`, heard on
    /// whichever thread the plugin reports from.
    pub(crate) notices: Notices,
    /// The handle's subscription that fills `notices`; replaced with the handle.
    hearing: Subscription,
    /// What the plugin declares and the value of each parameter
    /// ([`Plugin::params`]).
    pub(crate) mirror: tutti_plugin_host::PluginParamValues,
    /// What a gesture asks before it sends: is this still the plugin, and does
    /// it still declare the parameter. Ended when the record retires or the
    /// plugin is replaced.
    pub(crate) gate: Gate,
    /// The read that fills the mirror (a crossfade rebuilt it, the load's read
    /// was not answered, or the plugin announced a rescan), until
    /// `Phase::Plugins` hands it to the host as a job.
    param_read: Option<tutti_plugin_host::ParamRead>,
    /// Held by that job: a later read mints a new one, and the earlier read
    /// lands nowhere.
    mirror_token: Arc<()>,
}

/// The part of a plugin's record a [`ParamGesture`](crate::ParamGesture) keeps:
/// whether the plugin it began on is still the one at the node, and which
/// parameters it declares now. Shared (`Arc`) because a gesture spans frames
/// and cannot borrow the session; the session ends it when the record retires or
/// the plugin is replaced, and refreshes the ids when a read lands.
#[derive(Debug, Clone, Default)]
pub(crate) struct Gate(Arc<Mutex<GateState>>);

#[derive(Debug, Default)]
struct GateState {
    ended: bool,
    declared: std::collections::HashSet<ParamId>,
}

impl Gate {
    /// The plugin is gone: removed or replaced.
    pub(crate) fn end(&self) {
        locked(&self.0).ended = true;
    }

    /// The ids the mirror declares now (a read landed).
    pub(crate) fn declare(&self, mirror: &tutti_plugin_host::PluginParamValues) {
        locked(&self.0).declared = mirror.descriptors().iter().map(|p| p.id()).collect();
    }

    /// Whether the plugin is still there and declares `id`.
    pub(crate) fn holds(&self, id: ParamId) -> bool {
        let state = locked(&self.0);
        !state.ended && state.declared.contains(&id)
    }
}

/// How a plugin was loaded, besides which plugin it is: what a reload needs.
#[derive(Debug, Clone)]
struct Origin {
    config: AudioConfig,
}

/// A recovering plugin's reload, between its death and its replacement.
#[derive(Default)]
struct Reload {
    /// When to hand the load out.
    due: Option<Instant>,
    /// Held by the job in flight; a new one per job, so a late landing of an
    /// earlier job is recognised.
    token: Arc<()>,
    in_flight: bool,
    /// The node's latency before the failure.
    latency_before: Samples,
    /// The age of a snapshot left out of the reload (see
    /// [`StateRestore::Withheld`]).
    withheld_age: Option<Duration>,
    /// A loaded replacement waiting for the graph to stop re-preparing.
    ready: Option<Ready>,
}

struct Ready {
    client: Box<tutti_plugin::handles::PluginClient>,
    restore: StateRestore,
    mirror: Option<Mirror>,
}

impl std::fmt::Debug for HostedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("HostedPlugin");
        d.field("plugin", &self.plugin)
            .field("handle", &self.handle)
            .field("controls", &self.controls)
            .field("node", &self.node)
            .field("liveness", &self.liveness)
            .field("status", &self.status)
            .field("told", &self.told)
            .field("snapshot_token", &self.snapshot_token)
            .field("snapshot_at", &self.snapshot_at)
            .field("hearing", &self.hearing)
            .field("recovery", &self.recovery)
            .field("reload_due", &self.reload.due);
        d.field("mirror", &self.mirror)
            .field("param_read", &self.param_read)
            .field("mirror_token", &self.mirror_token);
        d.finish_non_exhaustive()
    }
}

impl HostedPlugin {
    pub(crate) fn new(handle: PluginHandle, plugin: PluginRef) -> Self {
        let (notices, hearing) = hear(&handle);
        let (mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
        Self {
            editor_open: false,
            plugin,
            handle,
            controls: None,
            node: None,
            liveness: Liveness::new(),
            status: PluginLiveness::Healthy,
            told: false,
            snapshot: None,
            snapshot_from: None,
            withheld: None,
            probation_until: None,
            origin: None,
            recovery: None,
            reload: Reload::default(),
            snapshot_token: Arc::new(()),
            snapshot_at: None,
            notices,
            hearing,
            mirror,
            gate: Gate::default(),
            param_read: Some(read),
            mirror_token: Arc::new(()),
        }
    }

    pub(crate) fn with_controls(mut self, controls: PluginControls) -> Self {
        self.node = Some((*controls).clone());
        self.controls = Some(controls);
        self
    }

    /// The incoming plugin of a crossfade at the same node: its handle and
    /// controls, a fresh debounce, and not yet told the render mode. Nothing
    /// of the outgoing plugin's state carries over: its last good state is
    /// cleared (its bytes would restore the wrong plugin), a fresh snapshot token
    /// makes a job of the outgoing plugin still in flight land nowhere
    /// ([`Session::land_snapshot`]), and the snapshot clock restarts.
    ///
    /// (`modulation`) A record that kept a parameter mirror gets one for
    /// the incoming plugin, from its handle (where its reports now arrive),
    /// not yet ready: its values are read by a job (one round trip per
    /// parameter, so not on the calling thread), which the next
    /// `Phase::Plugins` hands to the host
    /// ([`SessionEvent::PluginParamsDue`]).
    /// The outgoing mirror's report callback was on the outgoing handle,
    /// which reports nothing here once it is gone.
    ///
    /// Returns the outgoing handle, for the graveyard: its drop may close the
    /// outgoing plugin's editor.
    #[must_use = "the outgoing handle goes to the graveyard"]
    pub(crate) fn replace(
        &mut self,
        handle: PluginHandle,
        controls: PluginControls,
        plugin: PluginRef,
    ) -> PluginHandle {
        self.plugin = plugin;
        let outgoing = self.adopt(handle);
        self.node = Some((*controls).clone());
        self.controls = Some(controls);
        outgoing
    }

    /// [`replace`](Self::replace)'s handle half: everything but the
    /// controls.
    fn adopt(&mut self, handle: PluginHandle) -> PluginHandle {
        let (mirror, read) = tutti_plugin_host::PluginParamValues::start(&handle);
        self.mirror = mirror;
        self.param_read = Some(read);
        self.mirror_token = Arc::new(());
        // A gesture on the outgoing plugin ends here: it is not this one.
        self.gate.end();
        self.gate = Gate::default();
        // What the outgoing plugin reported and nobody drained is its own.
        (self.notices, self.hearing) = hear(&handle);
        let outgoing = std::mem::replace(&mut self.handle, handle);
        self.editor_open = false;
        self.liveness = Liveness::new();
        self.status = PluginLiveness::Healthy;
        self.told = false;
        self.snapshot = None;
        self.snapshot_from = None;
        self.withheld = None;
        self.probation_until = None;
        self.recovery = self.recovery.as_ref().map(|r| Recovery::new(r.policy()));
        self.reload = Reload::default();
        self.snapshot_token = Arc::new(());
        self.snapshot_at = None;
        outgoing
    }

    /// Takes the snapshot for the cause of a failure: the next load goes
    /// without it, and it is not offered again.
    fn withhold_snapshot(&mut self, now: Instant) {
        if let Some(state) = self.snapshot.take() {
            self.withheld = Some(state);
            self.reload.withheld_age = Some(
                self.snapshot_from
                    .map_or(Duration::ZERO, |t| now.saturating_duration_since(t)),
            );
        }
        self.snapshot_from = None;
        self.probation_until = None;
    }

    /// Records a scheduled reload and tells the host. The node stays.
    fn begin_reload(
        &mut self,
        cause: ReloadCause,
        now: Instant,
        delay: Duration,
        attempt: u32,
        node: NodeKey,
        out: &mut Vec<SessionEvent>,
    ) {
        self.status = PluginLiveness::Recovering {
            cause: cause.clone(),
        };
        self.reload.due = Some(now + delay);
        self.reload.in_flight = false;
        // A job of an earlier attempt lands nowhere.
        self.reload.token = Arc::new(());
        out.push(event(
            node,
            PluginChange::Reloading {
                cause,
                attempt,
                delay,
            },
        ));
    }

    /// Parameter `id`'s value, as a modulation lane's knob starts at it:
    /// `None` while not read, `Some(None)` for one it does not list.
    #[cfg(any(test, feature = "modulation"))]
    pub(crate) fn param_value(&self, id: u32) -> Option<Option<f32>> {
        self.mirror
            .get(tutti_plugin_types::ParamId::new(id))
            .map(|v| v.map(|v| v.get() as f32))
    }

    /// Asks for the parameters to be read again (the plugin announced a
    /// rescan): a fresh read, handed to the host at the next
    /// `Phase::Plugins`, and a fresh token, so a read still in flight lands
    /// nowhere.
    fn reread_params(&mut self) {
        self.param_read = Some(tutti_plugin_host::ParamRead::of(&self.handle));
        self.mirror_token = Arc::new(());
    }
}

/// The type of a hosted plugin's [`Key`]: what [`Session::add_plugin`],
/// [`Graph::insert_plugin`](crate::Graph::insert_plugin) and
/// [`Graph::host_plugin`](crate::Graph::host_plugin) return, so a plugin's
/// ports and parameters are methods of its key as a native node's are.
///
/// A marker only: no value of it exists, and every plugin format, in or out
/// of process, is a `Key<PluginNode>`. It names the *kind* of node, as
/// [`Key::erase`] names none; the view of the plugin is [`Plugin`].
/// It is the session's name for a hosted plugin's node; the node itself is
/// `tutti_plugin_host::PluginNode`, below the session.
/// With `modulation`, `key.param(id)` is a plugin parameter's
/// `ParamRef` (see `Key::param`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginNode {}

#[cfg(feature = "modulation")]
impl Key<PluginNode> {
    /// Parameter `id` of the plugin, as the reference a modulation route or an
    /// automation lane targets (feature `modulation`): the same one
    /// [`Param::target`](crate::Param::target) gives and a native node's param
    /// method does, so a route names a plugin's parameter and a native one
    /// through one type.
    ///
    /// A plain value, like a native param's: it borrows no session and checks
    /// nothing. An id the plugin does not declare is refused where the
    /// reference is resolved (a route's resolution, `Mods`'s
    /// [`Problem`](crate::Problem)), as [`ParamRef::plugin`](crate::ParamRef::plugin)'s
    /// is; [`Plugin::params`] resolves one now.
    #[must_use]
    pub fn param(self, id: ParamId) -> crate::ParamRef {
        crate::ParamRef::plugin(self, id.get())
    }
}

/// A hosted plugin, lent by [`Session::plugin`]: its identity, and the
/// surfaces the session holds for it (design 029 section 3.1).
///
/// A view borrows the session, so it cannot outlive the plugin it names: a
/// node that is removed or replaced while one is held is a borrow error, and
/// what is kept across updates is the node's key, looked up again.
#[derive(Debug, Clone, Copy)]
pub struct Plugin<'a> {
    node: NodeKey,
    hosted: &'a HostedPlugin,
    figures: Figures,
}

impl<'a> Plugin<'a> {
    pub(crate) fn new(node: NodeKey, hosted: &'a HostedPlugin, figures: Figures) -> Self {
        Self {
            node,
            hosted,
            figures,
        }
    }

    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The plugin's name, as it declares it.
    pub fn name(&self) -> &'a str {
        self.hosted.handle.name()
    }

    /// What the plugin declares about itself.
    pub fn descriptor(&self) -> &'a tutti_plugin_types::PluginDescriptor {
        self.hosted.handle.descriptor()
    }

    /// The plugin this is: its file, its format and its id.
    pub fn plugin(&self) -> &'a PluginRef {
        &self.hosted.plugin
    }

    /// The plugin's saved states: the last good one, the withheld one, a save
    /// now, and a restore.
    pub fn state(&self) -> StateView<'a> {
        StateView {
            hosted: self.hosted,
        }
    }

    /// How the plugin is doing: running, stalled, dead and about to be
    /// written off, or being reloaded (design 029 section 2.6).
    pub fn health(&self) -> PluginHealth {
        let hosted = self.hosted;
        match &hosted.status {
            PluginLiveness::Recovering { cause } => PluginHealth::Reloading {
                attempt: hosted.recovery.as_ref().map_or(1, Recovery::failures),
                cause: cause.clone(),
            },
            PluginLiveness::Dead { cause } => PluginHealth::Died {
                cause: cause.clone(),
            },
            PluginLiveness::Healthy | PluginLiveness::Failing => match hosted.handle.status() {
                PluginStatus::Dead { cause } => PluginHealth::Died { cause },
                _ if hosted.recovery.as_ref().is_some_and(Recovery::is_stalled) => {
                    PluginHealth::Stalled
                }
                _ => PluginHealth::Running,
            },
        }
    }

    /// The plugin's diagnostics: its server, its dropouts and losses, and the
    /// latency and tail the graph holds for it (design 029 section 2.7).
    pub fn diagnostics(&self) -> Diagnostics<'a> {
        Diagnostics::new(self.hosted, self.figures)
    }

    /// The plugin's buses as the graph's bus handles, to wire with
    /// `Graph::connect` (design 029 section 3.5).
    pub fn buses(&self) -> PluginBuses<'a> {
        PluginBuses::new(self.node, self.hosted)
    }

    /// The plugin's control handle, the low layer the views are built on
    /// (feature `testing`: a host uses the views).
    #[cfg(feature = "testing")]
    pub fn handle(&self) -> &'a PluginHandle {
        &self.hosted.handle
    }

    /// The plugin's parameters: what it declares, read at load and again when
    /// it announces a rescan, and the value of each, kept current from what the
    /// plugin reports and the host sets (design 029 section 2.3).
    pub fn params(&self) -> Params<'a> {
        Params::new(self.node, self.hosted)
    }
}

/// A hosted plugin's states ([`Plugin::state`]): what the session holds
/// of them, a save now, and a restore.
#[derive(Debug, Clone, Copy)]
pub struct StateView<'a> {
    hosted: &'a HostedPlugin,
}

impl<'a> StateView<'a> {
    /// The last state captured while the plugin was healthy: the
    /// crash-recovery copy a reload restores, up to a snapshot interval
    /// stale. A project is saved from [`save`](Self::save), not from this.
    pub fn last_good(&self) -> Option<&'a PluginState> {
        self.hosted.snapshot.as_ref()
    }

    /// The state a reload left out because the plugin died loading it, or
    /// soon after ([`StateRestore::Withheld`]): the user's last state, which a
    /// host may still offer to load by hand. Kept until the plugin is
    /// withheld from again or replaced by the host; the states captured since
    /// (of a plugin at its defaults) do not replace it.
    pub fn withheld(&self) -> Option<&'a PluginState> {
        self.hosted.withheld.as_ref()
    }

    /// Saves the plugin's state now, as a blocking round trip to it (ten
    /// seconds at worst; an in-process plugin's is run by the main thread's
    /// next tick, see the module docs).
    ///
    /// # Errors
    ///
    /// The plugin's [`StateError`]: it is gone, busy, refused, or has no state
    /// route.
    pub fn save(&self) -> Result<PluginState, StateError> {
        let bytes = self.hosted.handle.any_thread().save_state()?;
        Ok(PluginState::new(self.hosted.plugin.clone(), bytes))
    }

    /// Restores `state` into the plugin.
    ///
    /// # Errors
    ///
    /// [`StateError::OtherPlugin`] when `state` was saved from another plugin
    /// (nothing is sent to the plugin); otherwise the plugin's own
    /// [`StateError`].
    pub fn restore(&self, main: &MainThread, state: &PluginState) -> Result<(), StateError> {
        state.check_for(&self.hosted.plugin)?;
        self.hosted.handle.load_state(main, state.bytes())
    }
}

/// A healthy plugin's state capture, as a job: `Send`, and
/// [`run`](Self::run) is a blocking round trip (ten seconds at worst), so
/// the host runs it off its control thread and lands the result with
/// [`Session::land_snapshot`]. Dropping it unrun frees the plugin for the
/// next one.
#[must_use = "a snapshot does nothing until it is run and landed"]
pub struct PluginSnapshot {
    node: NodeKey,
    /// Boxed: a `SessionEvent` carries the job, and a handle is large.
    handle: Box<PluginHandle>,
    token: Arc<()>,
    asked: Instant,
}

impl PluginSnapshot {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// Asks the plugin for its state. Any thread; it blocks.
    pub fn run(self) -> PluginSnapshotTaken {
        PluginSnapshotTaken {
            node: self.node,
            state: self.handle.any_thread().save_state(),
            token: self.token,
            asked: self.asked,
        }
    }
}

impl std::fmt::Debug for PluginSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSnapshot")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// A finished [`PluginSnapshot`], for [`Session::land_snapshot`].
#[must_use = "land it with `Session::land_snapshot`"]
pub struct PluginSnapshotTaken {
    node: NodeKey,
    state: Result<Vec<u8>, StateError>,
    /// The record's token when the job was asked for: a crossfade to
    /// another plugin mints a new one, and the job no longer lands.
    token: Arc<()>,
    asked: Instant,
}

impl std::fmt::Debug for PluginSnapshotTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginSnapshotTaken")
            .field("node", &self.node)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl PluginSnapshotTaken {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }
}

/// A replacement for a dead or hung plugin, as a job: `Send`, and
/// [`run`](Self::run) blocks like [`PluginLoad::run`] (it is one), so the host
/// runs it off its control thread and lands the result with
/// [`Session::land_reload`]. Dropped unrun, the attempt counts as failed.
#[must_use = "a reload does nothing until it is run and landed"]
pub struct PluginReload {
    node: NodeKey,
    load: PluginLoad,
    token: Arc<()>,
    /// How old the snapshot it restores is, if it restores one.
    snapshot_age: Option<Duration>,
}

impl PluginReload {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// Launches the replacement and restores the snapshot. Any thread; it
    /// blocks.
    pub fn run(self) -> PluginReloaded {
        PluginReloaded {
            node: self.node,
            loaded: self.load.run(),
            token: self.token,
            snapshot_age: self.snapshot_age,
        }
    }
}

impl std::fmt::Debug for PluginReload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginReload")
            .field("node", &self.node)
            .field("load", &self.load)
            .finish_non_exhaustive()
    }
}

/// A finished [`PluginReload`], for [`Session::land_reload`].
#[must_use = "land it with `Session::land_reload`"]
pub struct PluginReloaded {
    node: NodeKey,
    loaded: PluginLoaded,
    token: Arc<()>,
    snapshot_age: Option<Duration>,
}

impl std::fmt::Debug for PluginReloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginReloaded")
            .field("node", &self.node)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

impl PluginReloaded {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }
}

/// The read of a parameter mirror a crossfade rebuilt, or a load's read that
/// was not answered, as a job (`modulation`): `Send`, and [`run`](Self::run)
/// makes one round trip per parameter, so the host runs it off its control
/// thread and lands the result with [`Session::land_params`]. Dropped unrun,
/// the mirror stays not ready (a lane on the plugin keeps waiting) until the
/// next crossfade.
#[must_use = "a parameter read does nothing until it is run and landed"]
pub struct PluginParamRead {
    node: NodeKey,
    /// Boxed: a `SessionEvent` carries the job, and a handle is large.
    read: Box<tutti_plugin_host::ParamRead>,
    token: Arc<()>,
}

impl PluginParamRead {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// Reads every parameter the plugin lists. Any thread; it blocks. If the
    /// plugin's list is unavailable (an in-process plugin's main thread did not
    /// answer in time) the finished job carries the read back:
    /// [`Session::land_params`] does not land it, and the next
    /// [`Phase::Plugins`](crate::Phase::Plugins) hands it out again.
    pub fn run(self) -> PluginParamsRead {
        PluginParamsRead {
            node: self.node,
            values: self.read.run(),
            token: self.token,
        }
    }
}

impl std::fmt::Debug for PluginParamRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginParamRead")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

/// A finished [`PluginParamRead`], for [`Session::land_params`].
#[must_use = "land it with `Session::land_params`"]
#[derive(Debug)]
pub struct PluginParamsRead {
    node: NodeKey,
    /// The values read, or the read itself given back to be run again.
    values: Result<tutti_plugin_host::ParamSnapshot, Box<tutti_plugin_host::ParamRead>>,
    /// The record's mirror token when the job was asked for.
    token: Arc<()>,
}

impl PluginParamsRead {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }
}

/// `Phase::RenderMode`'s record: the mode last announced.
#[derive(Debug)]
pub(crate) struct RenderModeState {
    pub(crate) announced: tutti_plugin::RenderMode,
}

impl Default for RenderModeState {
    fn default() -> Self {
        Self {
            announced: tutti_plugin::RenderMode::Realtime,
        }
    }
}

impl Session {
    /// A load of `request`'s plugin at the session's rate, as a job:
    /// [`run`](PluginLoad::run) it on any thread (it blocks), then hand the
    /// result to [`add_plugin`](Self::add_plugin).
    pub fn plugin_load(&self, request: PluginLoadRequest) -> PluginLoad {
        PluginLoad {
            plugin: request.plugin,
            rate: self.graph.prepared().sample_rate(),
            config: AudioConfig::default(),
            state: request.state,
            refusal: None,
        }
    }

    /// [`plugin_load`](Self::plugin_load) and its run, on this thread: it
    /// blocks for the whole launch.
    ///
    /// # Errors
    ///
    /// As [`add_plugin`](Self::add_plugin).
    pub fn add_plugin_blocking(
        &mut self,
        main: &MainThread,
        request: PluginLoadRequest,
    ) -> Result<Key<PluginNode>, BridgeError> {
        let done = self.plugin_load(request).run();
        self.add_plugin(main, done)
    }

    /// Inserts a loaded plugin into the graph (unwired) and hosts it: its
    /// controls and meter are captured as it goes in, the meter is the
    /// metronome's (time signature and bar), and its health, latency,
    /// parameter mirror and render mode are the session's from here
    /// ([`Plugin`]). An out-of-process plugin is bound as it goes in
    /// (`PluginClient::bind`); an in-process VST2 plugin is opened here (its
    /// job left it unopened: VST2 confines the load to the main thread) and
    /// goes in as its own node. That open blocks this call for the
    /// library's init, the state restore and (`modulation`) the parameter
    /// read: a frame-driven host opens at most one per frame.
    ///
    /// A main-thread call (design 015 §4.2).
    ///
    /// # Errors
    ///
    /// Why the load failed.
    pub fn add_plugin(
        &mut self,
        main: &MainThread,
        loaded: PluginLoaded,
    ) -> Result<Key<PluginNode>, BridgeError> {
        let requested = loaded.plugin;
        let config = loaded.config;
        let Open {
            plugin,
            restore: _,
            mirror,
        } = match loaded.opened {
            Opened::Done(open) => *open,
            // A plugin hosted in process opens here, on the main thread.
            Opened::OnMain(load) => load.open(Some(main)),
        };
        let plugin = plugin?;
        let handle = plugin.handle().clone();
        let loaded_ref = plugin.plugin().clone();
        let meter = self.click_settings.project_meter();
        let key = match plugin.into_client() {
            Ok(client) => {
                client.controls().set_meter(meter);
                let key = self.graph.insert_plugin(client);
                if let Some(hosted) = self.graph.plugins.get_mut(&key.erase()) {
                    hosted.origin = Some(Origin { config });
                    hosted.recovery = self.recovery.map(Recovery::new);
                }
                key
            }
            Err(plugin) => {
                let node_controls = plugin.node_controls();
                node_controls.set_meter(meter);
                let (node, _) = self.graph.place((*plugin).bind(self.graph.tempo.clone()));
                let mut hosted = HostedPlugin::new(handle, loaded_ref);
                hosted.node = Some(node_controls);
                self.graph.plugins.insert(node.erase(), hosted);
                Key::new(node.erase())
            }
        };
        let node = key.erase();
        if let Some(hosted) = self.graph.plugins.get_mut(&node)
            && let Some(Mirror { values, unread }) = mirror
        {
            hosted.mirror = values;
            hosted.param_read = unread;
            hosted.gate.declare(&hosted.mirror);
        }
        tracing::info!("plugin '{}' added as {node:?}", requested.path().display());
        Ok(key)
    }

    /// The plugin hosted at `node`.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`](crate::NoSuchNode) when no plugin is hosted at `node`.
    pub fn plugin(&self, node: impl Into<NodeKey>) -> Result<Plugin<'_>, crate::NoSuchNode> {
        let node = node.into();
        self.graph
            .plugins
            .get(&node)
            .map(|hosted| Plugin::new(node, hosted, self.graph.plugin_figures(node)))
            .ok_or_else(|| crate::NoSuchNode::new(node))
    }

    /// How many retired plugins wait for the main thread to drop them: what a
    /// removal or a replacement parked and no [`MainSession::update`](crate::MainSession::update)
    /// has freed yet. Nonzero for good means no one calls it; the session
    /// warns once past a few.
    pub fn retired_plugins(&self) -> usize {
        self.graph.graveyard.len()
    }

    /// Every hosted plugin, in node order.
    pub fn plugins(&self) -> impl Iterator<Item = Plugin<'_>> {
        self.graph
            .plugins
            .iter()
            .map(|(&node, hosted)| Plugin::new(node, hosted, self.graph.plugin_figures(node)))
    }

    /// Lands a finished snapshot: the plugin's last snapshot becomes it. A
    /// refused one is logged and keeps the previous snapshot (a good
    /// recovery point beats none). Returns whether it was stored. Frees the
    /// plugin for its next snapshot.
    ///
    /// A snapshot of a plugin no longer at its node (removed, or crossfaded
    /// to another plugin since the job was asked for) lands nowhere: it is
    /// the outgoing plugin's state, not a recovery point for the node.
    pub fn land_snapshot(&mut self, taken: PluginSnapshotTaken) -> bool {
        let PluginSnapshotTaken {
            node,
            state,
            token,
            asked,
        } = taken;
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(&token, &hosted.snapshot_token) {
            tracing::debug!("plugin {node:?}: a snapshot of its previous plugin was discarded");
            return false;
        }
        match state {
            Ok(bytes) => {
                hosted.snapshot = Some(PluginState::new(hosted.plugin.clone(), bytes));
                hosted.snapshot_from = Some(asked);
                true
            }
            Err(e) => {
                tracing::warn!("plugin {node:?}: periodic state snapshot failed: {e}");
                false
            }
        }
    }

    /// Lands a finished parameter read: the plugin's mirror holds its
    /// values and is ready. Returns whether it landed. A read of a plugin no
    /// longer at its node (removed, or crossfaded again since) lands
    /// nowhere. A read the plugin did not answer (its list unavailable) does
    /// not land either: the mirror stays not ready, the read goes back to the
    /// plugin's record, and the next `Phase::Plugins` hands it out again as
    /// [`SessionEvent::PluginParamsDue`].
    pub fn land_params(&mut self, read: PluginParamsRead) -> bool {
        let PluginParamsRead {
            node,
            values,
            token,
        } = read;
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return false;
        };
        if !Arc::ptr_eq(&token, &hosted.mirror_token) {
            tracing::debug!(
                "plugin {node:?}: a parameter read of its previous plugin was discarded"
            );
            return false;
        }
        match values {
            Ok(values) => {
                hosted.mirror.land(values);
                hosted.gate.declare(&hosted.mirror);
                true
            }
            Err(read) => {
                tracing::debug!(
                    "plugin {node:?}: its parameter list was unavailable; the read is kept to run again"
                );
                hosted.param_read = Some(*read);
                false
            }
        }
    }

    /// What a hosted plugin's death or hang does from now on
    /// ([`SessionBuilder::plugin_recovery`](crate::SessionBuilder::plugin_recovery),
    /// which says which plugins it covers: out-of-process ones loaded through
    /// [`add_plugin`](Self::add_plugin), not in-process VST2 or
    /// `insert_plugin`/`host_plugin`).
    /// Plugins already hosted start over with the new policy (their failure
    /// history is dropped); a plugin already being replaced finishes that
    /// first. `None` removes a dead plugin's node.
    pub fn set_plugin_recovery(&mut self, policy: Option<RecoveryPolicy>) {
        self.recovery = policy;
        for hosted in self.graph.plugins.values_mut() {
            if hosted.origin.is_none() || matches!(hosted.status, PluginLiveness::Recovering { .. })
            {
                continue;
            }
            hosted.recovery = policy.map(Recovery::new);
        }
    }

    /// Lands a finished reload, at the time of the last `Phase::Plugins`: the replacement takes the dead
    /// plugin's node, with its wiring (`Graph::replace_plugin`), its snapshot
    /// and failure history. Returns what happened, as the events
    /// `Phase::Plugins` would have reported.
    ///
    /// A reload of a node no longer being recovered (removed, replaced by
    /// another plugin, or an earlier attempt's) lands nowhere and returns
    /// nothing; the loaded plugin is dropped, which ends its server. A
    /// failed load is the next attempt's backoff, or
    /// [`PluginChange::GaveUp`] and the node's removal.
    ///
    /// A main-thread call.
    pub fn land_reload(&mut self, main: &MainThread, done: PluginReloaded) -> Vec<SessionEvent> {
        self.land_reload_inner(main, done)
    }

    fn land_reload_inner(&mut self, main: &MainThread, done: PluginReloaded) -> Vec<SessionEvent> {
        let now = self.plugins_now;
        let PluginReloaded {
            node,
            loaded,
            token,
            snapshot_age,
        } = done;
        let mut out = Vec::new();
        let mut gave_up = Vec::new();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return out;
        };
        if !Arc::ptr_eq(&token, &hosted.reload.token)
            || !matches!(hosted.status, PluginLiveness::Recovering { .. })
        {
            tracing::debug!("plugin {node:?}: a reload that no longer applies was discarded");
            return out;
        }
        hosted.reload.in_flight = false;
        let opened = match loaded.opened {
            Opened::Done(open) => *open,
            Opened::OnMain(load) => load.open(Some(main)),
        };
        let Open {
            plugin,
            restore,
            mirror,
        } = opened;
        let client = match plugin.map(HostPlugin::into_client) {
            Ok(Ok(client)) => client,
            Ok(Err(_in_process)) => {
                self.fail_reload(
                    node,
                    RecoveryFailure::ReplacementInProcess,
                    now,
                    &mut out,
                    &mut gave_up,
                );
                self.finish_gave_up(gave_up, &mut out);
                return out;
            }
            Err(e) => {
                // A load that died restoring the snapshot is held against the
                // snapshot, as a replacement that dies once installed is
                // (`install_reload`): the next attempt goes without it.
                if matches!(restore, Some(Err(_)))
                    && let Some(hosted) = self.graph.plugins.get_mut(&node)
                {
                    hosted.withhold_snapshot(now);
                }
                self.fail_reload(
                    node,
                    RecoveryFailure::ReloadFailed {
                        source: Arc::new(e),
                    },
                    now,
                    &mut out,
                    &mut gave_up,
                );
                self.finish_gave_up(gave_up, &mut out);
                return out;
            }
        };
        let restore = match (restore, snapshot_age) {
            (Some(Ok(())), Some(age)) => StateRestore::Restored { age },
            (Some(Err(reason)), Some(age)) => StateRestore::Refused { age, reason },
            _ => StateRestore::NoSnapshot,
        };
        let ready = Ready {
            client,
            restore,
            mirror,
        };
        self.install_reload(node, ready, now, &mut out);
        out
    }

    /// Puts a loaded replacement into the graph at `node`, or keeps it until
    /// the graph stops re-preparing.
    fn install_reload(
        &mut self,
        node: NodeKey,
        ready: Ready,
        now: Instant,
        out: &mut Vec<SessionEvent>,
    ) {
        let Ready {
            client,
            restore,
            mirror,
        } = ready;
        // Dead already (a state that kills it, say): not recovered.
        if client.is_crashed() {
            if !matches!(
                restore,
                StateRestore::NoSnapshot | StateRestore::Withheld { .. }
            ) && let Some(hosted) = self.graph.plugins.get_mut(&node)
            {
                hosted.withhold_snapshot(now);
            }
            let cause = RecoveryFailure::ReplacementDied {
                cause: client
                    .handle()
                    .status()
                    .cause()
                    .cloned()
                    .unwrap_or(PluginFailure::Unreported),
            };
            let mut gave_up = Vec::new();
            self.fail_reload(node, cause, now, out, &mut gave_up);
            self.finish_gave_up(gave_up, out);
            return;
        }
        let meter = self.click_settings.project_meter();
        client.controls().set_meter(meter);
        // The node's edges stay as they are, so a replacement with other
        // ports would leave the topology invalid and refuse every later
        // commit: it is a failed attempt instead, and dropping it ends its
        // server.
        let got = client.declared_shape();
        if let Some(was) = self.graph.shape(node) {
            let counts = |s: &tutti_graph::Shape| PortCounts {
                audio_in: usize::from(s.audio_in().count()),
                audio_out: usize::from(s.audio_out().count()),
                event_in: usize::from(s.event_in),
                event_out: usize::from(s.event_out),
            };
            if counts(&was) != counts(&got) {
                let cause = RecoveryFailure::ReplacementPorts {
                    got: counts(&got),
                    was: counts(&was),
                };
                let mut gave_up = Vec::new();
                self.fail_reload(node, cause, now, out, &mut gave_up);
                self.finish_gave_up(gave_up, out);
                return;
            }
        }
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        // What the replacement carries over from the record, which a swap
        // resets (`HostedPlugin::replace`).
        let snapshot = hosted.snapshot.take();
        let snapshot_from = hosted.snapshot_from;
        let withheld_bytes = hosted.withheld.take();
        let recovery = hosted.recovery.take();
        let latency_before = hosted.reload.latency_before;
        let withheld = hosted.reload.withheld_age;
        let attempts = recovery.as_ref().map_or(1, Recovery::failures);
        match self.graph.replace_plugin(
            node,
            client,
            Seconds(0.01),
            tutti_graph::CrossfadeCurve::default(),
        ) {
            Ok(()) => {}
            Err(crate::ReplaceRefused::Busy(client)) => {
                // Between a re-prepare's two commits: try again next phase.
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.recovery = recovery;
                    hosted.reload.ready = Some(Ready {
                        client,
                        restore,
                        mirror,
                    });
                }
                return;
            }
            Err(crate::ReplaceRefused::Failed(refused)) => {
                tracing::error!("plugin {node:?}: the replacement was refused: {refused}");
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.snapshot = snapshot;
                    hosted.withheld = withheld_bytes;
                    hosted.recovery = recovery;
                }
                let mut gave_up = Vec::new();
                let cause = RecoveryFailure::ReplacementRefused { source: refused };
                self.fail_reload(node, cause, now, out, &mut gave_up);
                self.finish_gave_up(gave_up, out);
                return;
            }
        }
        let latency_after = self.graph.node_latency(node).unwrap_or_default();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        hosted.snapshot = snapshot;
        hosted.withheld = withheld_bytes;
        hosted.snapshot_from = snapshot_from;
        hosted.recovery = recovery;
        if let Some(recovery) = hosted.recovery.as_mut() {
            recovery.step(RecoveryEvent::Loaded, now);
        }
        // The replacement's own read, after the restore, is the mirror: the
        // swap built an unread one, and nothing of the old plugin's values
        // carries over (they are the values read at load, which the restored
        // state has replaced; writing them back would undo the restore and
        // bypass the lanes that own those parameters).
        //
        // A modulation lane on a parameter needs no seeding here: the swap
        // moved the node's generation, so the reconciler rebuilds the lane (a
        // new node, its knob starting at this mirror's value) and the old
        // lane, with whatever GUI value its knob held, goes with the old unit.
        // The new lane's first value is the restored one.
        if let Some(Mirror { values, unread }) = mirror {
            hosted.mirror = values;
            hosted.param_read = unread;
            hosted.mirror_token = Arc::new(());
            hosted.gate.declare(&hosted.mirror);
        }
        let restore = match (restore, withheld) {
            (StateRestore::NoSnapshot, Some(age)) => StateRestore::Withheld { age },
            (restore, _) => restore,
        };
        hosted.probation_until = match (&restore, hosted.recovery.as_ref()) {
            (StateRestore::Restored { .. }, Some(r)) => Some(now + r.policy().state_probation),
            _ => None,
        };
        hosted.status = PluginLiveness::Healthy;
        hosted.reload = Reload::default();
        out.push(event(
            node,
            PluginChange::Recovered {
                state: restore,
                latency_before,
                latency_after,
                attempts,
            },
        ));
    }

    /// One reload attempt failed (or its job was dropped): the next backoff,
    /// or the end of trying.
    fn fail_reload(
        &mut self,
        node: NodeKey,
        cause: RecoveryFailure,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        hosted.reload.in_flight = false;
        let Some(recovery) = hosted.recovery.as_mut() else {
            return;
        };
        let action = recovery.step(
            RecoveryEvent::LoadFailed {
                cause: cause.clone(),
            },
            now,
        );
        self.apply_failure(node, action, ReloadCause::Attempt(cause), now, out, gave_up);
    }

    /// Carries out the answer to a failed attempt.
    fn apply_failure(
        &mut self,
        node: NodeKey,
        action: RecoveryAction,
        reason: ReloadCause,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        match action {
            RecoveryAction::Reload { delay, attempt } => {
                tracing::warn!("plugin {node:?}: reload failed ({reason}); again in {delay:?}");
                if let Some(hosted) = self.graph.plugins.get_mut(&node) {
                    hosted.begin_reload(reason, now, delay, attempt, node, out);
                }
            }
            RecoveryAction::GiveUp(why) => {
                tracing::error!("plugin {node:?}: reload failed ({reason}); giving up: {why}");
                gave_up.push((node, why));
            }
            RecoveryAction::Wait | RecoveryAction::Terminate { .. } => {}
        }
    }

    fn finish_gave_up(&mut self, gave_up: Vec<(NodeKey, GiveUp)>, out: &mut Vec<SessionEvent>) {
        for (node, why) in gave_up {
            // The last state worth offering a user: what was withheld, else
            // the last snapshot. The record goes with the node.
            let state = self
                .graph
                .plugins
                .get(&node)
                .and_then(|h| h.withheld.clone().or_else(|| h.snapshot.clone()));
            let _ = self.remove(node); // gone already is as good as removed
            out.push(event(node, PluginChange::GaveUp { why, state }));
        }
    }

    /// One phase's work on a plugin being replaced: install a replacement
    /// that was waiting, count a dropped or overdue job as failed, or hand
    /// the load out when its backoff has passed.
    fn drive_reload(
        &mut self,
        node: NodeKey,
        now: Instant,
        out: &mut Vec<SessionEvent>,
        gave_up: &mut Vec<(NodeKey, GiveUp)>,
    ) {
        let rate = self.graph.prepared().sample_rate();
        let Some(hosted) = self.graph.plugins.get_mut(&node) else {
            return;
        };
        if let Some(ready) = hosted.reload.ready.take() {
            self.install_reload(node, ready, now, out);
            return;
        }
        if hosted.reload.in_flight {
            // The token is the job's alone while it is out: sole owner means
            // the job was dropped.
            if Arc::strong_count(&hosted.reload.token) == 1 {
                self.fail_reload(node, RecoveryFailure::ReloadDropped, now, out, gave_up);
                return;
            }
        }
        let (tick, load_timeout) = hosted
            .recovery
            .as_mut()
            .map_or((RecoveryAction::Wait, Duration::ZERO), |r| {
                (r.step(RecoveryEvent::Tick, now), r.policy().load_timeout)
            });
        if !matches!(tick, RecoveryAction::Wait) {
            // The attempt ran out of time.
            let reason = ReloadCause::Attempt(RecoveryFailure::ReloadTimedOut {
                after: load_timeout,
            });
            self.apply_failure(node, tick, reason, now, out, gave_up);
            return;
        }
        let (Some(origin), Some(due)) = (hosted.origin.clone(), hosted.reload.due) else {
            return;
        };
        if hosted.reload.in_flight || now < due {
            return;
        }
        hosted.reload.due = None;
        hosted.reload.in_flight = true;
        let load = PluginLoad {
            plugin: hosted.plugin.clone(),
            rate,
            config: origin.config,
            state: hosted.snapshot.clone(),
            refusal: None,
        };
        out.push(SessionEvent::PluginReloadDue(PluginReload {
            node,
            load,
            token: Arc::clone(&hosted.reload.token),
            snapshot_age: hosted
                .snapshot_from
                .map(|t| now.saturating_duration_since(t)),
        }));
    }

    /// The render mode last announced to the hosted plugins.
    pub fn render_mode(&self) -> tutti_plugin::RenderMode {
        self.render_mode.announced
    }

    /// `Phase::Plugins`. See the module docs.
    pub(crate) fn plugins_phase(&mut self, cx: crate::PhaseCx<'_>, out: &mut Vec<SessionEvent>) {
        self.graph.graveyard.warn_if_piling();
        let now = cx.now;
        self.plugins_now = now;
        let interval = self.snapshot_interval;
        let mut dead = Vec::new();
        let mut gave_up = Vec::new();
        let nodes: Vec<NodeKey> = self.graph.plugins.keys().copied().collect();
        for node in nodes {
            let Some(hosted) = self.graph.plugins.get_mut(&node) else {
                continue;
            };
            if matches!(hosted.status, PluginLiveness::Dead { .. }) {
                continue;
            }
            // What the plugin reported since the last update, in the order
            // it reported it.
            let heard = std::mem::take(&mut *locked(&hosted.notices));
            // A rescan the plugin announced re-reads what it declares and the
            // value of each, as a job below.
            if heard.iter().any(|notice| {
                matches!(
                    notice,
                    Notice::Refresh(PluginRefresh::ParamValues | PluginRefresh::ParamTitles)
                )
            }) {
                hosted.reread_params();
            }
            out.extend(
                heard
                    .into_iter()
                    .filter_map(change_of)
                    .map(|change| event(node, change)),
            );
            let cause = match hosted.handle.status() {
                PluginStatus::Dead { cause } => Some(cause),
                _ => None,
            };
            if matches!(hosted.status, PluginLiveness::Recovering { .. }) {
                self.drive_reload(node, now, out, &mut gave_up);
                continue;
            }
            match hosted.liveness.observe(cause.is_none(), now) {
                Verdict::Healthy => hosted.status = PluginLiveness::Healthy,
                Verdict::Failing { .. } => hosted.status = PluginLiveness::Failing,
                Verdict::Dead => {
                    // `Liveness` declares a death only on an unhealthy
                    // observation, and every unhealthy one carries the
                    // engine's latched cause.
                    let cause = cause.unwrap_or(PluginFailure::Unreported);
                    let latency = self.graph.node_latency(node).unwrap_or_default();
                    let hosted = self
                        .graph
                        .plugins
                        .get_mut(&node)
                        .expect("looked up above, and nothing between removes it");
                    if hosted.probation_until.is_some_and(|t| now < t) {
                        tracing::warn!(
                            "plugin {node:?} died soon after restoring its snapshot; the next \
                             load goes without it"
                        );
                        hosted.withhold_snapshot(now);
                    }
                    let action = hosted.recovery.as_mut().map(|r| {
                        r.step(
                            RecoveryEvent::Died {
                                cause: cause.clone(),
                            },
                            now,
                        )
                    });
                    match action {
                        Some(RecoveryAction::Reload { delay, attempt }) => {
                            tracing::error!(
                                "plugin '{}' declared dead ({cause}); reloading {node:?} in \
                                 {delay:?}",
                                hosted.handle.name()
                            );
                            hosted.reload.latency_before = latency;
                            hosted.begin_reload(
                                ReloadCause::Died(cause),
                                now,
                                delay,
                                attempt,
                                node,
                                out,
                            );
                            continue;
                        }
                        Some(RecoveryAction::GiveUp(why)) => {
                            tracing::error!(
                                "plugin '{}' declared dead ({cause}); giving up on {node:?}: {why}",
                                hosted.handle.name()
                            );
                            hosted.status = PluginLiveness::Dead { cause };
                            gave_up.push((node, why));
                            continue;
                        }
                        _ => {}
                    }
                    tracing::error!(
                        "plugin '{}' declared dead ({cause}); removing {node:?}",
                        hosted.handle.name()
                    );
                    hosted.status = PluginLiveness::Dead {
                        cause: cause.clone(),
                    };
                    dead.push((node, cause));
                    continue;
                }
            }
            // A plugin that is alive but never returns from `process()` is
            // ended here, and dies by the path above.
            if cause.is_none()
                && let Some(recovery) = hosted.recovery.as_mut()
                && let RecoveryAction::Terminate { reason } = recovery.step(
                    {
                        let counts = hosted.handle.block_counts();
                        RecoveryEvent::Starved {
                            abandoned: counts.abandoned,
                            replies: counts.replies,
                        }
                    },
                    now,
                )
            {
                tracing::error!(
                    "plugin '{}' ({node:?}): {reason}; ending its server",
                    hosted.handle.name()
                );
                hosted.handle.terminate(reason);
            }

            // The figure its node declares now, onto the graph when it moved.
            let declared = hosted
                .controls
                .as_ref()
                .map(PluginControls::declared_latency);
            let healthy = matches!(hosted.status, PluginLiveness::Healthy);
            let due = healthy && Arc::strong_count(&hosted.snapshot_token) == 1 && {
                let since = *hosted.snapshot_at.get_or_insert(now);
                now.saturating_duration_since(since) >= interval
            };
            let snapshot = due.then(|| {
                hosted.snapshot_at = Some(now);
                PluginSnapshot {
                    node,
                    handle: Box::new(hosted.handle.clone()),
                    token: Arc::clone(&hosted.snapshot_token),
                    asked: now,
                }
            });

            if let Some(read) = hosted.param_read.take() {
                out.push(SessionEvent::PluginParamsDue(PluginParamRead {
                    node,
                    read: Box::new(read),
                    token: Arc::clone(&hosted.mirror_token),
                }));
            }
            #[cfg(feature = "modulation")]
            let state = &self.mods.state;
            hosted.mirror.drain_edits(|_id, _value| {
                #[cfg(feature = "modulation")]
                if let Some(target) = state.plugin_target(node, _id.get()) {
                    state.set_knob(target, _value.get() as f32);
                }
            });

            if let Some(declared) = declared
                && self.graph.refresh_node_latency(node, declared)
            {
                out.push(event(
                    node,
                    PluginChange::LatencyChanged {
                        latency: self.graph.node_latency(node).unwrap_or_default(),
                    },
                ));
            }
            if let Some(snapshot) = snapshot {
                out.push(SessionEvent::PluginSnapshotDue(snapshot));
            }
        }
        for (node, cause) in dead {
            let _ = self.remove(node); // gone already is as good as removed
            out.push(event(node, PluginChange::Died { cause }));
        }
        self.finish_gave_up(gave_up, out);
    }

    /// `Phase::RenderMode`: Offline while an export token lives, Realtime
    /// otherwise, told per `needs_announcement`. Without `export` nothing
    /// is ever announced: there is no bounce to announce.
    pub(crate) fn render_mode_phase(&mut self) {
        #[cfg(feature = "export")]
        {
            let wanted = if self.bounce.in_flight() {
                tutti_plugin::RenderMode::Offline
            } else {
                tutti_plugin::RenderMode::Realtime
            };
            let changed = self.render_mode.announced != wanted;
            for hosted in self.graph.plugins.values_mut() {
                if !tutti_plugin_host::needs_announcement(changed, hosted.told) {
                    continue;
                }
                // `false` means the plugin declined (a CLAP plugin without
                // `clap.render`): it renders the same either way, so there is
                // nothing to retry.
                let _ = hosted.handle.any_thread().set_render_mode(wanted);
                hosted.told = true;
            }
            self.render_mode.announced = wanted;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The record's own rules over a fake backend (no `plugin-server`):
    //! the snapshot cadence and its one-in-flight token, a declined save,
    //! and a failing plugin's skipped snapshot. The death, the render mode
    //! and the export are pinned end to end over the CLAP test plugin, in
    //! `tests/plugin/plugin.rs`.

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tutti_plugin_types::{Normalized, ParamId, ParameterInfo};

    /// A backend that answers `state` (or declines), counts its saves, and
    /// is dead while `dead` is set.
    struct Fake {
        state: Option<Vec<u8>>,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        /// The parameters it lists, and their values.
        params: Vec<(ParamId, f32)>,
        /// While set, its list is unavailable (`parameter_descriptors` is
        /// `None`), as an in-process plugin's is when its main thread does
        /// not answer.
        unlisted: Arc<AtomicBool>,
    }

    impl tutti_plugin_types::AnyThread for Fake {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            if self.unlisted.load(Ordering::SeqCst) {
                return None;
            }
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect(),
            )
        }
        fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
            self.params
                .iter()
                .find(|(a, _)| *a == id)
                .map(|&(_, v)| Normalized::clamped(f64::from(v)))
        }
        fn set_parameter_value(&self, _id: ParamId, _value: Normalized) {}
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Option<Normalized> {
            None
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.state
                .clone()
                .ok_or(tutti_plugin::error::StateError::NoStateRoute)
        }
        fn set_render_mode(&self, _mode: tutti_plugin_types::RenderMode) -> bool {
            false
        }
        fn set_automation_mode(
            &self,
            _mode: tutti_plugin_types::AutomationMode,
        ) -> Result<(), tutti_plugin_types::AutomationModeError> {
            Err(tutti_plugin_types::AutomationModeError::PluginDead)
        }
    }

    impl tutti_plugin_types::Health for Fake {
        fn is_crashed(&self) -> bool {
            self.dead.load(Ordering::SeqCst)
        }
        fn crash_cause(&self) -> Option<PluginFailure> {
            self.is_crashed().then_some(PluginFailure::StreamClosed)
        }
        fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
            None
        }
        fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
            tutti_plugin_types::BlockCounts::default()
        }
        fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
    }

    struct Rig {
        session: Session,
        node: NodeKey,
        saves: Arc<AtomicUsize>,
        dead: Arc<AtomicBool>,
        t0: Instant,
    }

    /// A detached session snapshotting every second, hosting a fake that
    /// answers `state` at a dc node.
    fn rig(state: Option<Vec<u8>>) -> Rig {
        let mut session = Session::builder()
            .snapshot_interval(Duration::from_secs(1))
            .build_detached(SampleRate(48_000.0))
            .unwrap();
        let saves = Arc::new(AtomicUsize::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                state,
                saves: Arc::clone(&saves),
                dead: Arc::clone(&dead),
                params: Vec::new(),
                unlisted: Arc::default(),
            }),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let (node, _) =
            session
                .graph()
                .add(tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(
                    0.0,
                )));
        session
            .graph()
            .host_plugin(
                node,
                handle,
                PluginRef::file("fake.clap").expect("a plugin file"),
            )
            .expect("the node is in the graph");
        let node = node.erase();
        Rig {
            session,
            node,
            saves,
            dead,
            t0: Instant::now(),
        }
    }

    /// A fake plugin's handle listing `params`, answering `state`, and the
    /// sink its reports go through (firing it is the plugin reporting).
    fn fake_handle(
        state: Option<Vec<u8>>,
        params: Vec<(ParamId, f32)>,
    ) -> (PluginHandle, tutti_plugin_host::ParameterChangeSink) {
        let sink = tutti_plugin_host::ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(Fake {
                state,
                saves: Arc::default(),
                dead: Arc::default(),
                params,
                unlisted: Arc::default(),
            }),
            None,
            Default::default(),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    impl Rig {
        /// `Phase::Plugins` at `t0 + ms`: every event it reported.
        fn events_at(&mut self, ms: u64) -> Vec<SessionEvent> {
            let mut out = Vec::new();
            let cx = crate::PhaseCx::at(self.t0 + Duration::from_millis(ms));
            self.session.run_phase(crate::Phase::Plugins, cx, &mut out);
            out
        }

        /// `Phase::Plugins` at `t0 + ms`: the snapshot jobs it asked for.
        fn plugins_at(&mut self, ms: u64) -> Vec<PluginSnapshot> {
            self.events_at(ms)
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::PluginSnapshotDue(job) => Some(job),
                    _ => None,
                })
                .collect()
        }
    }

    /// **Snapshots are taken on the interval of `now`, not every update,
    /// and one at a time.** The first update starts the clock; updates
    /// inside the interval ask for nothing; the first at or past it asks
    /// once; while that job is unlanded nothing more is asked, however late
    /// (the job is held, not run); landing it stores the bytes and
    /// restarts the cadence; a dropped job frees the plugin too.
    #[test]
    fn snapshots_are_taken_on_the_interval_one_at_a_time() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        assert!(
            rig.plugins_at(0).is_empty(),
            "the first update starts the clock"
        );
        assert!(rig.plugins_at(500).is_empty(), "inside the interval");
        let mut jobs = rig.plugins_at(1_000);
        assert_eq!(jobs.len(), 1, "one job at the interval");
        let job = jobs.pop().unwrap();
        assert_eq!(job.node(), rig.node);
        assert!(
            rig.plugins_at(5_000).is_empty(),
            "nothing more while the job is in flight"
        );
        let taken = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(rig.session.land_snapshot(taken));
        assert_eq!(
            rig.session
                .plugin(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PluginState::bytes),
            Some(&[1u8, 2, 3][..])
        );
        assert_eq!(rig.saves.load(Ordering::SeqCst), 1, "one job is one save");
        assert_eq!(
            rig.plugins_at(5_500).len(),
            1,
            "the cadence restarted when the last job was asked for (at 1 s)"
        );
        // That job is dropped unrun (a cancelled task): the plugin is free
        // again, on the interval.
        assert!(rig.plugins_at(6_000).is_empty(), "inside the interval");
        assert_eq!(rig.plugins_at(6_500).len(), 1, "a dropped job frees it");
    }

    /// **A declined save keeps the previous snapshot.** A refusal (the
    /// plugin declines, or the bridge dropped mid-call) replacing a good
    /// recovery point with nothing is worse than a stale one.
    #[test]
    fn a_declined_save_keeps_the_previous_snapshot() {
        let mut rig = rig(None);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.snapshot = Some(PluginState::new(hosted.plugin.clone(), vec![4, 2]));
        rig.plugins_at(0);
        let job = rig.plugins_at(1_000).pop().expect("a job at the interval");
        assert!(!rig.session.land_snapshot(job.run()), "declined");
        assert_eq!(
            rig.session
                .plugin(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PluginState::bytes),
            Some(&[4u8, 2][..]),
            "a declined save must keep the last good snapshot"
        );
    }

    /// **The update frees a hosted plugin's project-meter cell cleared while a
    /// block held it**: once cleared the node's meter slot follows no project
    /// meter, so only the session's sweep of its plugins' meters reaches what the
    /// clear retired, on the next update with nothing published again.
    #[test]
    fn an_update_frees_a_plugin_meter_cleared_while_a_block_held_it() {
        let mut rig = rig(None);
        let controls = PluginControls::new(Samples(0), PluginTail::default(), SampleRate(48_000.0));
        let project = rig.session.click_settings().project_meter();
        controls.set_meter(project);
        let meter = controls.meter();
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .expect("hosted")
            .node = Some((*controls).clone());
        assert!(meter.has_meter(), "the project's meter is followed");

        let mut block = meter.probe_reader();
        {
            let _held = block.lend();
            meter.clear_meter();
            let _ = rig.session.update();
            assert_eq!(meter.retired_len(), 1, "held across the clear");
        }
        let _ = rig.session.update();
        assert_eq!(meter.retired_len(), 0, "the update freed it");
    }

    /// **A latency past what PDC compensates is clamped, not refused**: a plugin
    /// that reports more than `MAX_NODE_LATENCY` (the reference plugin cannot: its
    /// delay line is short) reaches the graph as that figure at the next
    /// `Phase::Plugins`.
    #[test]
    fn a_reported_latency_past_what_pdc_compensates_is_clamped() {
        let mut rig = rig(None);
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .expect("hosted")
            .controls = Some(PluginControls::new(
            Samples(10_000_000),
            PluginTail::default(),
            SampleRate(48_000.0),
        ));
        rig.plugins_at(0);
        assert_eq!(
            rig.session.graph_ref().node_latency(rig.node).unwrap(),
            tutti_graph::MAX_NODE_LATENCY
        );
    }

    /// **A failing plugin is not asked for its state**: the call would
    /// block against a peer that is not answering, and return nothing.
    #[test]
    fn a_failing_plugin_is_not_snapshotted() {
        let mut rig = rig(Some(vec![1]));
        rig.plugins_at(0);
        rig.dead.store(true, Ordering::SeqCst);
        assert!(rig.plugins_at(1_000).is_empty(), "failing: no snapshot");
        assert!(matches!(
            rig.session.plugin(rig.node).unwrap().health(),
            PluginHealth::Died { .. }
        ));
    }

    /// **A hosted plugin replaced by a native node is no longer hosted**
    /// (`Graph::replace`, and `Graph::crossfade` through it): its record
    /// goes, so a later death of the outgoing plugin removes nothing (the
    /// node is the native one's now) and reports nothing, and the record's
    /// handle no longer keeps the outgoing plugin alive.
    #[test]
    fn a_plugin_replaced_by_a_native_node_is_no_longer_hosted() {
        for crossfade in [false, true] {
            let mut rig = rig(None);
            let replacement = tutti_graph::ForkByClone(tutti_nodes::testing::Const::mono(1.0));
            let fade = tutti_types::Seconds(0.0);
            let curve = tutti_graph::CrossfadeCurve::EqualAmplitude;
            if crossfade {
                assert!(matches!(
                    rig.session
                        .graph()
                        .crossfade(rig.node, replacement, fade, curve),
                    Ok(crate::Crossfade::Landed(_))
                ));
            } else {
                assert!(
                    rig.session
                        .graph()
                        .replace(rig.node, replacement, fade, curve)
                        .is_ok()
                );
            }
            assert!(rig.session.plugin(rig.node).is_err(), "the record went");
            // The outgoing plugin dies, past the debounce's count and window.
            rig.dead.store(true, Ordering::SeqCst);
            for ms in [0, 100, 200, 300, 400] {
                let died = rig.events_at(ms).into_iter().any(|e| {
                    matches!(
                        e,
                        SessionEvent::Plugin(PluginEvent {
                            change: PluginChange::Died { .. },
                            ..
                        })
                    )
                });
                assert!(
                    !died,
                    "a native node is reported dead (crossfade: {crossfade})"
                );
            }
            assert!(
                rig.session.graph_ref().contains(rig.node),
                "the native replacement stays in the graph (crossfade: {crossfade})"
            );
        }
    }

    /// **A crossfade to another plugin does not inherit the outgoing
    /// plugin's recovery state.** Its last snapshot is cleared, and a
    /// snapshot job of the outgoing plugin still in flight lands nowhere
    /// (its token is not the record's any more); the incoming plugin gets
    /// its own on the interval from the crossfade. Driven through
    /// `HostedPlugin::adopt`, `replace`'s handle half (a crossfade to
    /// another plugin needs a `PluginClient`, a real `plugin-server`).
    #[test]
    fn a_crossfade_to_another_plugin_drops_the_old_snapshot() {
        let mut rig = rig(Some(vec![1, 2, 3]));
        rig.plugins_at(0);
        let first = rig.plugins_at(1_000).pop().expect("a job at the interval");
        assert!(rig.session.land_snapshot(first.run()));
        let in_flight = rig.plugins_at(2_000).pop().expect("the next job");

        let (incoming, _) = fake_handle(Some(vec![9]), Vec::new());
        rig.session
            .graph
            .plugins
            .get_mut(&rig.node)
            .unwrap()
            .adopt(incoming);
        assert_eq!(
            rig.session
                .plugin(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PluginState::bytes),
            None,
            "the outgoing plugin's snapshot is not the incoming one's"
        );
        assert!(
            !rig.session.land_snapshot(in_flight.run()),
            "the outgoing plugin's job must not land on the incoming plugin"
        );
        assert_eq!(
            rig.session
                .plugin(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PluginState::bytes),
            None
        );

        assert!(rig.plugins_at(2_000).is_empty(), "its clock starts here");
        let own = rig.plugins_at(3_000).pop().expect("its own job");
        assert!(rig.session.land_snapshot(own.run()));
        assert_eq!(
            rig.session
                .plugin(rig.node)
                .unwrap()
                .state()
                .last_good()
                .map(PluginState::bytes),
            Some(&[9u8][..])
        );
    }

    /// **A crossfade to another plugin rebuilds the parameter mirror from
    /// the incoming plugin, read off the control thread.** Right after the
    /// crossfade the mirror is not ready (a lane waits rather than start
    /// from the outgoing plugin's value); the next `Phase::Plugins` hands
    /// the read to the host as a job, once; the job runs on another thread
    /// and lands the incoming plugin's values. A read asked for before a
    /// second crossfade lands nowhere. The read goes through the incoming
    /// handle, which `PluginParamValues::start` also gives the mirror's
    /// report callback; a fake's reports cannot be fired from outside
    /// `tutti-plugin`, so the callback is not asserted here.
    #[test]
    fn a_crossfade_to_another_plugin_rebuilds_the_mirror() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&outgoing);
        mirror.land(read.run().expect("the read lands"));
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = mirror;
        assert_eq!(hosted.param_value(3), Some(Some(0.25)), "precondition");

        let (incoming, _) = fake_handle(None, vec![(param, 0.75)]);
        hosted.adopt(incoming);
        assert_eq!(
            hosted.param_value(3),
            None,
            "not ready until the read lands: nothing was read on this thread"
        );

        let reads = |events: Vec<SessionEvent>| -> Vec<PluginParamRead> {
            events
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::PluginParamsDue(job) => Some(job),
                    _ => None,
                })
                .collect()
        };
        let mut jobs = reads(rig.events_at(0));
        assert_eq!(jobs.len(), 1, "the read is handed out as a job");
        assert!(reads(rig.events_at(10)).is_empty(), "once");
        let job = jobs.pop().unwrap();
        assert_eq!(job.node(), rig.node);
        let done = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(rig.session.land_params(done));
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(3),
            Some(Some(0.75)),
            "the mirror holds the incoming plugin's values"
        );

        // A read asked for, then a second crossfade: the first lands nowhere.
        let (second, _) = fake_handle(None, vec![(param, 0.5)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(second);
        let stale = reads(rig.events_at(20)).pop().expect("its read");
        let (third, _) = fake_handle(None, vec![(param, 0.125)]);
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.adopt(third);
        assert!(
            !rig.session.land_params(stale.run()),
            "a read of the previous plugin must not land"
        );
        assert_eq!(rig.session.graph.plugins[&rig.node].param_value(3), None);
        let own = reads(rig.events_at(30)).pop().expect("its own read");
        assert!(rig.session.land_params(own.run()));
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(3),
            Some(Some(0.125))
        );
    }

    /// **A read whose parameter list is unavailable does not make the mirror
    /// ready, and is handed out again.** The job lands as not landed (the
    /// mirror stays not ready), the next phase emits `PluginParamsDue` for the
    /// same plugin once more, and once the list answers the re-run read lands
    /// the values.
    #[test]
    fn a_read_whose_parameter_list_is_unavailable_is_handed_out_again() {
        let param = ParamId::new(3);
        let mut rig = rig(None);
        let (outgoing, _) = fake_handle(None, vec![(param, 0.25)]);
        let (mut mirror, read) = tutti_plugin_host::PluginParamValues::start(&outgoing);
        mirror.land(read.run().expect("the read lands"));
        let hosted = rig.session.graph.plugins.get_mut(&rig.node).unwrap();
        hosted.mirror = mirror;

        let unlisted = Arc::new(AtomicBool::new(true));
        let incoming = PluginHandle::from_backend(
            Arc::new(Fake {
                state: None,
                saves: Arc::default(),
                dead: Arc::default(),
                params: vec![(param, 0.75)],
                unlisted: Arc::clone(&unlisted),
            }),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        );
        hosted.adopt(incoming);

        let reads = |events: Vec<SessionEvent>| -> Vec<PluginParamRead> {
            events
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::PluginParamsDue(job) => Some(job),
                    _ => None,
                })
                .collect()
        };
        let job = reads(rig.events_at(0))
            .pop()
            .expect("the read is handed out");
        let done = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(!rig.session.land_params(done), "nothing was read");
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(3),
            None,
            "the mirror is not ready"
        );

        let job = reads(rig.events_at(10))
            .pop()
            .expect("the read comes round again");
        unlisted.store(false, Ordering::SeqCst);
        let done = std::thread::spawn(move || job.run()).join().unwrap();
        assert!(rig.session.land_params(done));
        assert_eq!(
            rig.session.graph.plugins[&rig.node].param_value(3),
            Some(Some(0.75)),
            "the re-run read filled the mirror"
        );
        assert!(reads(rig.events_at(20)).is_empty(), "and it is done");
    }
}

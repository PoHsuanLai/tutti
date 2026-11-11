//! What a plugin tells its host went stale: the wire's [`ResyncKind`] and the
//! two consequences it maps to, [`PluginRefresh`] and [`PluginInvalidation`].

use tutti_types::SampleRate;

use crate::{PluginTail, Samples};

/// Which aspect of plugin state the plugin asked the host to re-read.
///
/// Unlike a latency or parameter change, these carry no new value; they say
/// only "your cached view of X is stale."
///
/// This is the wire vocabulary. The `PluginHandle` callbacks (in `tutti-plugin`)
/// split it *by consequence* into [`PluginRefresh`] (cosmetic, re-read a cached
/// view) and [`PluginInvalidation`] (structural, re-plan the graph) — see
/// [`ResyncKind::classify`].
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncKind {
    /// Re-read all parameter values (plugin loaded a preset / wrote them back).
    ParamValues,
    /// Re-pull the parameter list (titles, units, or flags changed).
    ParamTitles,
    /// Re-read the bus layout and rewire the audio graph.
    Io,
    /// The plugin instance was rebuilt; resync everything.
    Reloaded,
}

impl ResyncKind {
    /// Maps this signal to its host-facing consequence: a cosmetic
    /// [`PluginRefresh`] (re-read a cached view, no graph edit) or a structural
    /// [`PluginInvalidation`] (rewire and re-plan latency compensation).
    #[must_use]
    pub fn classify(self) -> ResyncClass {
        match self {
            ResyncKind::ParamValues => ResyncClass::Refresh(PluginRefresh::ParamValues),
            ResyncKind::ParamTitles => ResyncClass::Refresh(PluginRefresh::ParamTitles),
            ResyncKind::Io => ResyncClass::Invalidate(PluginInvalidation::Io),
            ResyncKind::Reloaded => ResyncClass::Invalidate(PluginInvalidation::Reloaded),
        }
    }
}

/// The consequence a [`ResyncKind`] maps to — which of the two split callbacks
/// the host should fire.
///
/// Not `Copy`, following [`PluginInvalidation`], which it wraps.
#[derive(Debug, Clone, PartialEq)]
pub enum ResyncClass {
    /// Re-read a cached view; nothing the graph holds is wrong.
    Refresh(PluginRefresh),
    /// What the graph holds of the plugin is wrong until rebuilt.
    Invalidate(PluginInvalidation),
}

/// A cosmetic notification that the host's cached view of some plugin state is
/// stale.
///
/// Re-read the state; the audio graph is unaffected. Delivered via
/// `tutti-plugin`'s `PluginHandle::on_refresh`.
/// Mirrors CLAP `params.rescan(flags)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRefresh {
    /// Re-read all parameter values (a preset load / internal write-back).
    ParamValues,
    /// Re-pull the parameter list (titles, units, or flags changed).
    ParamTitles,
}

/// A structural notification that the plugin changed in a way that invalidates
/// the audio graph's plan.
///
/// The host must rewire and re-run latency compensation (PDC). Delivered via
/// `tutti-plugin`'s `PluginHandle::on_invalidate`.
/// Mirrors CLAP `request_restart()` + `audio_ports.rescan()` + `latency.changed()`.
///
/// **Not `Copy`**, because [`Crashed`](Self::Crashed) carries an owned cause.
/// `Clone` is enough: every consumer takes this by value through
/// `Fn(PluginInvalidation)`.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginInvalidation {
    /// The plugin reported new processing latency. Carries the new value; the
    /// node's own atomic is already updated live, but compensation delays across
    /// the graph only re-plan on a commit.
    Latency {
        /// The plugin's new latency, in **frames**.
        samples: Samples,
    },
    /// The plugin reported a new tail length. Carries the new value; the node's
    /// own cell is already updated live, but an offline render sizes its length
    /// once at the start, so a bounce already in flight keeps the old figure.
    Tail {
        /// The plugin's new tail, which may be bounded, unbounded or unreported.
        tail: PluginTail,
    },
    /// The plugin's bus layout changed — re-read it and rewire the graph.
    Io,
    /// The plugin instance was rebuilt in place; re-plan everything.
    Reloaded,
    /// The plugin refused the engine's sample rate and renders silence until
    /// a rate it takes. Not terminal and not a rewire: the plugin stays
    /// loaded and keeps its declared latency, so the plan stands. Retried at
    /// every later rate change; [`RateRestored`](Self::RateRestored) says it
    /// took one.
    RateRefused {
        /// The rate that was refused.
        rate: SampleRate,
        /// The plugin's reason, formatted subprocess-side.
        reason: String,
    },
    /// The plugin took a rate after refusing one and renders again.
    RateRestored,
    /// The plugin died and is not coming back — unwire it.
    ///
    /// Structural rather than cosmetic, and terminal rather than a request to
    /// re-read: every other variant asks the host to *update* its plan, this one
    /// says there is nothing left to plan around. The engine offers no relaunch,
    /// so a host recovers by loading a replacement, carrying whatever state it
    /// captured while the plugin was healthy.
    ///
    /// A host that misses this event is not left guessing — `PluginHandle`'s
    /// status query reports the same cause, latched. See
    /// `BridgeEvent::Crashed` for why both exist.
    Crashed {
        /// Why the plugin died, latched at the detection site.
        cause: String,
    },
}

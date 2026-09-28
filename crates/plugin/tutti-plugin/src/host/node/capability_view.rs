//! Per-block input installers, reached only when the plugin can receive them.
//!
//! Every per-block input is dropped at drain time when the plugin did not
//! advertise the matching [`Features`] bit. That gate is correct and stays — but it fires
//! a block later, on the audio thread, where nothing can be reported. A caller
//! that installs a meter into a plugin which never asked for transport sees
//! the install succeed and the data silently vanish.
//!
//! These views move that answer to the call site. Each accessor on
//! [`PluginClient`](super::PluginClient) returns `None` for a plugin that
//! **declined** the capability, so the installer is not reachable at all rather
//! than reachable and inert:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use tutti_core::{meter::MeterMap, RtPublish};
//! # use tutti_plugin::handles::PluginClient;
//! # fn ex(client: &mut PluginClient, meter: Arc<RtPublish<MeterMap>>) {
//! // `None` for a plugin that declined transport, so the install is not
//! // reachable at all rather than reachable and inert.
//! if let Some(mut t) = client.transport() {
//!     t.set_meter(meter);
//! }
//! # }
//! ```
//!
//! # Declined, not merely absent
//!
//! The gate is [`LoadedPlugin::capability`], which is three-valued: `Some(true)`
//! (asked, yes), `Some(false)` (asked, no), `None` (never asked). Only
//! `Some(false)` withholds the view.
//!
//! Treating `None` as absence would be wrong, and the AU loader is why: a host
//! that does not probe a capability reports the same clear bit as a plugin that
//! refused it. Withholding on `None` would refuse installs for whole formats
//! purely because their loader does not ask, turning a reporting gap into a
//! functional failure. `None` therefore permits the install — the block-level
//! gate remains the backstop.
//!
//! What this does **not** do is catch a plugin that misreports: one declaring
//! `MIDI_OUT` and never emitting is indistinguishable from a working one. The
//! views close the "I wired it and nothing happened" gap, not the "the plugin
//! lied" gap.
//!
//! MIDI needs no installer: it travels on the node's event ports.
//! [`PluginClient::takes_midi`] and
//! [`PluginClient::sends_midi`] answer whether wiring them means anything.

use std::sync::Arc;

use super::PluginClient;
use crate::protocol::Features;

/// Whether a capability is open for installation.
///
/// `false` only when the plugin was asked and said no — see the module docs for
/// why an unprobed capability stays open.
fn declined(client: &PluginClient, f: Features) -> bool {
    is_declined(client.loaded(), f)
}

/// The gate itself, over the metadata rather than the client.
///
/// Split out so the decision can be tested without standing up a subprocess:
/// building a [`PluginClient`] needs a live bridge, but the rule being pinned
/// here is a pure function of what the plugin reported.
pub(crate) fn is_declined(loaded: &crate::protocol::LoadedPlugin, f: Features) -> bool {
    loaded.capability(f) == Some(false)
}

/// Configures the per-block transport snapshot. Reached via
/// [`PluginClient::transport`].
///
/// The transport itself is not installed: the node reads it from each block's
/// `Env` once it is in a graph. What is left to give it is the meter.
pub struct TransportView<'a>(&'a mut PluginClient);

impl TransportView<'_> {
    /// Install the meter map the snapshot's signature and bar are read from.
    ///
    /// The meter map arrives as an `RtPublish` because the audio thread reads it
    /// once per block and never holds an owning handle.
    pub fn set_meter(&mut self, meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>) {
        self.0.set_meter(meter);
    }

    /// Drop the meter; subsequent blocks tell the plugin 4/4 from bar 0.
    pub fn clear_meter(&mut self) {
        self.0.controls.clear_meter();
    }
}

impl PluginClient {
    /// Whether the plugin sends MIDI: `false` only if it declared no MIDI
    /// output. The node has a MIDI event output only when it declared one.
    pub fn sends_midi(&self) -> bool {
        !declined(self, Features::MIDI_OUT)
    }

    /// Whether the plugin takes MIDI: `false` only if it declared no MIDI
    /// input. Wire MIDI to the node's event input.
    pub fn takes_midi(&self) -> bool {
        !declined(self, Features::MIDI_IN)
    }

    /// Whether the plugin takes chord and scale context: `false` only if it
    /// declared no sequencer context. Wire a `HarmonyNode`
    /// (tutti-midi-runtime) to its event input; the node reads what arrives
    /// only when this holds.
    pub fn takes_harmony(&self) -> bool {
        !declined(self, Features::SEQUENCER_CONTEXT)
    }

    /// The transport's meter installer, or `None` if the plugin declared it
    /// does not want a transport snapshot.
    pub fn transport(&mut self) -> Option<TransportView<'_>> {
        (!declined(self, Features::TRANSPORT)).then_some(TransportView(self))
    }
}

#[cfg(test)]
mod tests {
    use super::is_declined;
    use crate::protocol::{Features, LoadedPlugin};

    /// `probed` set, bit clear — the plugin was asked and said no.
    fn answered_no(f: Features) -> LoadedPlugin {
        LoadedPlugin {
            probed: f,
            features: Features::empty(),
            ..Default::default()
        }
    }

    /// A plugin that was asked and declined does not get the installer.
    ///
    /// This is the whole point of the view: the per-block gate already drops
    /// the data, but it does so a block later on the audio thread, where the
    /// caller cannot be told.
    #[test]
    fn a_declined_capability_is_withheld() {
        for f in [
            Features::MIDI_IN,
            Features::MIDI_OUT,
            Features::TRANSPORT,
            Features::NOTE_EXPRESSION,
            Features::SEQUENCER_CONTEXT,
        ] {
            assert!(
                is_declined(&answered_no(f), f),
                "{f:?} was answered no and should be withheld"
            );
        }
    }

    /// A capability nobody probed stays open.
    ///
    /// An unprobed bit reads as clear, exactly like a declined one, so gating
    /// on the bare bit would refuse installs for a whole format purely because
    /// its loader does not ask — turning a reporting gap into a functional
    /// failure. The block-level gate remains the backstop.
    #[test]
    fn an_unprobed_capability_is_not_treated_as_declined() {
        let never_asked = LoadedPlugin::default();
        assert_eq!(never_asked.capability(Features::MIDI_IN), None);
        assert!(!is_declined(&never_asked, Features::MIDI_IN));
    }

    /// A plugin that advertised the capability gets the installer.
    #[test]
    fn an_advertised_capability_is_open() {
        let advertised = LoadedPlugin {
            probed: Features::MIDI_OUT,
            features: Features::MIDI_OUT,
            ..Default::default()
        };
        assert!(!is_declined(&advertised, Features::MIDI_OUT));
    }

    /// Declining one capability does not withhold a different one.
    #[test]
    fn each_capability_is_gated_independently() {
        let midi_only = LoadedPlugin {
            probed: Features::MIDI_IN | Features::TRANSPORT,
            features: Features::MIDI_IN,
            ..Default::default()
        };
        assert!(!is_declined(&midi_only, Features::MIDI_IN));
        assert!(is_declined(&midi_only, Features::TRANSPORT));
    }
}

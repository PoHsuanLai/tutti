//! Per-block input installers, reached only when the plugin can receive them.
//!
//! Every per-block producer ([`InputSlot`](super::input_slot::InputSlot), and the
//! MIDI-out target) is dropped at drain time when the plugin did not advertise
//! the matching [`Features`] bit. That gate is correct and stays — but it fires
//! a block later, on the audio thread, where nothing can be reported. A caller
//! that installs a transport reader into a plugin which never asked for
//! transport sees the install succeed and the data silently vanish.
//!
//! These views move that answer to the call site. Each accessor on
//! [`PluginClient`](super::PluginClient) returns `None` for a plugin that
//! **declined** the capability, so the installer is not reachable at all rather
//! than reachable and inert:
//!
//! ```ignore
//! if let Some(t) = client.transport() {
//!     t.set_source(reader, meter);
//! }
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
//! purely because their loader has not been taught to ask yet, turning a
//! reporting gap into a functional regression. `None` therefore permits the
//! install — the block-level gate remains the backstop, exactly as before.
//!
//! What this does **not** do is catch a plugin that misreports: one declaring
//! `MIDI_OUT` and never emitting is indistinguishable from a working one. The
//! views close the "I wired it and nothing happened" gap, not the "the plugin
//! lied" gap.

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

/// Installs the MIDI-out routing target. Reached via
/// [`PluginClient::midi_out`].
pub struct MidiOutView<'a>(&'a PluginClient);

impl MidiOutView<'_> {
    /// Route this plugin's MIDI-out back into the graph.
    pub fn set_target(
        &self,
        queue: Arc<dyn tutti_midi_types::MidiRouter>,
        routing: Arc<
            tutti_midi_types::tutti_types::RtPublish<tutti_midi_types::MidiRoutingSnapshot>,
        >,
    ) {
        self.0.set_midi_out(queue, routing);
    }

    /// Drop the routing target; subsequent blocks discard MIDI-out.
    pub fn clear(&self) {
        self.0.clear_midi_out();
    }
}

/// Installs a per-block MIDI producer. Reached via [`PluginClient::midi_in`].
pub struct MidiInView<'a>(&'a mut PluginClient);

impl MidiInView<'_> {
    /// Install a transport-aware source polled once per block, layered over the
    /// live mailbox.
    pub fn set_source(&mut self, source: Arc<dyn tutti_midi_types::MidiIn>) {
        self.0.set_midi_source(source);
    }
}

/// Installs the per-block chord/scale context. Reached via
/// [`PluginClient::harmony`].
pub struct HarmonyView<'a>(&'a mut PluginClient);

impl HarmonyView<'_> {
    pub fn set_source(
        &mut self,
        chords: impl IntoIterator<Item = super::TimedChord>,
        scales: impl IntoIterator<Item = super::TimedScale>,
        transport: impl tutti_core::transport::Timeline + 'static,
    ) {
        self.0.set_harmony_source(chords, scales, transport);
    }

    /// Drop the source; subsequent blocks feed empty chord/scale context.
    pub fn clear(&mut self) {
        self.0.clear_harmony_source();
    }
}

/// Installs the per-block note-expression stream. Reached via
/// [`PluginClient::note_expression`].
pub struct NoteExpressionView<'a>(&'a mut PluginClient);

impl NoteExpressionView<'_> {
    pub fn set_source(&mut self, source: Arc<super::NoteExpressionSource>) {
        self.0.set_note_expression_source(source);
    }

    /// Drop the source; subsequent blocks feed empty note-expression.
    pub fn clear(&mut self) {
        self.0.clear_note_expression_source();
    }
}

/// Installs the per-block transport snapshot. Reached via
/// [`PluginClient::transport`].
pub struct TransportView<'a>(&'a mut PluginClient);

impl TransportView<'_> {
    pub fn set_source(
        &mut self,
        reader: tutti_core::transport::Transport,
        meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>,
    ) {
        self.0.set_transport_source(reader, meter);
    }

    /// Drop the reader; subsequent blocks feed a default (stopped) snapshot.
    pub fn clear(&mut self) {
        self.0.clear_transport_source();
    }
}

impl PluginClient {
    /// The MIDI-out routing target, or `None` if the plugin declared no MIDI
    /// output.
    pub fn midi_out(&self) -> Option<MidiOutView<'_>> {
        (!declined(self, Features::MIDI_OUT)).then(|| MidiOutView(self))
    }

    /// The MIDI input installer, or `None` if the plugin declared no MIDI input.
    pub fn midi_in(&mut self) -> Option<MidiInView<'_>> {
        (!declined(self, Features::MIDI_IN)).then(move || MidiInView(self))
    }

    /// The chord/scale installer, or `None` if the plugin declared no sequencer
    /// context.
    pub fn harmony(&mut self) -> Option<HarmonyView<'_>> {
        (!declined(self, Features::SEQUENCER_CONTEXT)).then(move || HarmonyView(self))
    }

    /// The note-expression installer, or `None` if the plugin declared no
    /// note-expression support.
    pub fn note_expression(&mut self) -> Option<NoteExpressionView<'_>> {
        (!declined(self, Features::NOTE_EXPRESSION)).then(move || NoteExpressionView(self))
    }

    /// The transport installer, or `None` if the plugin declared it does not
    /// want a transport snapshot.
    pub fn transport(&mut self) -> Option<TransportView<'_>> {
        (!declined(self, Features::TRANSPORT)).then(move || TransportView(self))
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
    /// its loader has not been taught to ask — turning a reporting gap into a
    /// functional regression. The block-level gate remains the backstop.
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

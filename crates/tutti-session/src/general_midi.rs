//! A General MIDI setup of a SoundFont unit: the unit, then its chorus and
//! reverb, then the sum of the three.
//!
//! A [`SoundFontUnit`] has no effect inside it. It has five outputs (the dry
//! pair, the chorus send pair and the reverb send), and a synthesizer with the
//! sound of a General MIDI one needs what its sends were for. [`Session::add_general_midi`]
//! wires them: the chorus is a `ModDelayNode` with
//! [`ModDelayConfig::GENERAL_MIDI_CHORUS`] (wet only), the reverb a
//! [`FreeverbNode`], a [`ChannelSumNode`] sums the dry pair and both effects'
//! pairs into one stereo mix, and a [`BusStripNode`] after the sum is the
//! setup's fader, whose output is [`out`](GeneralMidi::out). The sends are
//! post-fader, as a mixer channel's are: the strip's gain and mute act on the
//! whole mix, effects included.
//! Nothing else is needed to hear it; nothing here is required either: a host
//! that wants a different reverb, two units into one chorus or no chorus wires
//! the unit's outputs itself, through [`Graph::set_source`](crate::Graph::set_source).

use tutti_graph::NodeKey;
use tutti_graph::ParamSet;
use tutti_nodes::{BusStripNode, ChannelSumNode, FreeverbNode, ModDelayConfig, ModDelayNode};
use tutti_soundfont::SoundFontUnit;
use tutti_types::{Amplitude, ChannelLayout, UnitParam};

use crate::{GraphSource, InstrumentHandle, Session};

/// A SoundFont unit wired to its chorus and reverb, and what they sum to.
///
/// What [`Session::add_general_midi`] adds. The mix is `out()`'s ports 0 and 1
/// (left and right). It is not wired to the graph's outputs: that is the host's
/// (`Graph::set_output_source`), since a second setup must not take the first's
/// place.
///
/// The [`instrument`](Self::instrument) is the unit's handle as any
/// instrument has it: MIDI is sent through it, and its gain is the **dry
/// level** (a strip on the dry pair only, before the sum). The setup's own
/// fader, [`set_gain`](Self::set_gain) and [`set_muted`](Self::set_muted), is
/// after the sum and so takes the effects with it. A send's level is the font's
/// and the channel's (CC 91 and CC 93), not a gain's.
#[derive(Clone, Debug)]
pub struct GeneralMidi {
    instrument: InstrumentHandle,
    chorus: NodeKey,
    reverb: NodeKey,
    sum: NodeKey,
    strip: NodeKey,
    strip_params: ParamSet,
}

impl GeneralMidi {
    /// The unit, played through its handle.
    #[must_use]
    pub fn instrument(&self) -> &InstrumentHandle {
        &self.instrument
    }

    /// The chorus: the unit's chorus send in, the wet chorus out.
    #[must_use]
    pub fn chorus(&self) -> NodeKey {
        self.chorus
    }

    /// The reverb: the unit's reverb send in, the wet reverb out.
    #[must_use]
    pub fn reverb(&self) -> NodeKey {
        self.reverb
    }

    /// The sum of the dry pair, the chorus and the reverb, before the fader:
    /// six inputs (three stereo sources) and two outputs.
    #[must_use]
    pub fn sum(&self) -> NodeKey {
        self.sum
    }

    /// The mix after the fader: dry pair, chorus and reverb summed, on ports 0
    /// (left) and 1 (right).
    #[must_use]
    pub fn out(&self) -> NodeKey {
        self.strip
    }

    /// Sets the gain of the whole mix, a linear amplitude: the dry pair and
    /// both effects. A store into the strip's cell, read once per block.
    pub fn set_gain(&self, gain: Amplitude) {
        self.strip_params.set(UnitParam::Volume, gain.get());
    }

    /// Mutes or unmutes the whole mix, effects included (a ramp across the next
    /// block, so there is no click).
    pub fn set_muted(&self, muted: bool) {
        self.strip_params
            .set(UnitParam::Mute, if muted { 1.0 } else { 0.0 });
    }

    /// Takes the whole setup out of the graph: the unit with its queue and
    /// strip, the chorus, the reverb, the sum and the fader.
    pub fn remove(self, session: &mut Session) {
        session.remove_instrument(&self.instrument);
        for node in [
            self.instrument.node(),
            self.chorus,
            self.reverb,
            self.sum,
            self.strip,
        ] {
            session.remove(node);
        }
    }
}

impl Session {
    /// Adds `unit` with a General MIDI chorus and reverb, the sum of the three
    /// and a fader after it.
    ///
    /// The wiring is the unit's dry pair (through the handle's strip) into the
    /// sum's source 0, its chorus send pair through a wet-only
    /// [`ModDelayConfig::GENERAL_MIDI_CHORUS`] into source 1, and its reverb
    /// send through a [`FreeverbNode`] into source 2. Each effect declares its
    /// own tail, so an export that ends at the last note-off keeps the release
    /// and then the room's ring-out.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) if the unit cannot
    /// be played as an instrument; none for a `SoundFontUnit`, which has an
    /// event input and five outputs.
    pub fn add_general_midi(&mut self, unit: SoundFontUnit) -> Result<GeneralMidi, crate::Error> {
        let (instrument, ()) = self.add_instrument(unit)?;
        let unit = instrument.node();
        let graph = self.graph();
        let (chorus, _) = graph.insert(ModDelayNode::new(
            ChannelLayout::STEREO,
            ModDelayConfig::GENERAL_MIDI_CHORUS,
        ));
        let (reverb, _) = graph.insert(FreeverbNode::general_midi());
        let (sum, ()) = graph.insert(ChannelSumNode::new(3, ChannelLayout::STEREO));
        let (strip, strip_params) = graph.add(BusStripNode::new());
        // The unit's outputs: dry L/R, chorus send L/R, reverb send.
        graph.set_source(chorus, 0, GraphSource::node(unit, 2));
        graph.set_source(chorus, 1, GraphSource::node(unit, 3));
        graph.set_source(reverb, 0, GraphSource::node(unit, 4));
        // The sum's inputs: source 0 (dry), 1 (chorus), 2 (reverb), each L/R.
        let dry = instrument.out();
        let sources = [
            (dry, 0),
            (dry, 1),
            (chorus, 0),
            (chorus, 1),
            (reverb, 0),
            (reverb, 1),
        ];
        for (port, (node, out)) in sources.into_iter().enumerate() {
            graph.set_source(sum, port, GraphSource::node(node, out));
        }
        graph.set_source(strip, 0, GraphSource::node(sum, 0));
        graph.set_source(strip, 1, GraphSource::node(sum, 1));
        Ok(GeneralMidi {
            instrument,
            chorus,
            reverb,
            sum,
            strip,
            strip_params,
        })
    }
}

//! A General MIDI setup of a SoundFont unit: the unit, then its chorus and
//! reverb, then the sum of the three.
//!
//! A [`SoundFontUnit`] has no effect inside it. It has five outputs (the dry
//! pair, the chorus send pair and the reverb send), and a synthesizer with the
//! sound of a General MIDI one needs what its sends were for. [`Session::add_general_midi`]
//! wires them: the chorus is a `ModDelayNode` with
//! [`ModDelayConfig::GENERAL_MIDI_CHORUS`] (wet only), the reverb a
//! [`ReverbNode`], and a [`ChannelSumNode`] sums the dry pair and both effects'
//! pairs into one stereo mix, which is the setup's [`out`](GeneralMidi::out).
//! Nothing else is needed to hear it; nothing here is required either: a host
//! that wants a different reverb, two units into one chorus or no chorus wires
//! the unit's outputs itself, through [`Graph::set_source`](crate::Graph::set_source).

use tutti_nodes::{ChannelSumNode, ModDelayConfig, ModDelayNode, ReverbNode};
use tutti_soundfont::SoundFontUnit;
use tutti_types::{ChannelLayout, NodeKey};

use crate::{GraphSource, InstrumentHandle, Session};

/// A SoundFont unit wired to its chorus and reverb, and what they sum to.
///
/// What [`Session::add_general_midi`] adds. The mix is `out()`'s ports 0 and 1
/// (left and right). It is not wired to the graph's outputs: that is the host's
/// (`Graph::set_output_source`), since a second setup must not take the first's
/// place.
///
/// The [`instrument`](Self::instrument) is the unit's handle as any
/// instrument has it: MIDI is sent through it, and its gain is a strip on the
/// dry pair only. A send's level is the font's and the channel's (CC 91 and
/// CC 93), not a gain's.
#[derive(Clone, Debug)]
pub struct GeneralMidi {
    instrument: InstrumentHandle,
    chorus: NodeKey,
    reverb: NodeKey,
    sum: NodeKey,
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

    /// The mix: dry pair, chorus and reverb summed, on ports 0 (left) and 1
    /// (right).
    #[must_use]
    pub fn out(&self) -> NodeKey {
        self.sum
    }
}

impl Session {
    /// Adds `unit` with a General MIDI chorus and reverb and the sum of the three.
    ///
    /// The wiring is the unit's dry pair (through the handle's strip) into the
    /// sum's source 0, its chorus send pair through a wet-only
    /// [`ModDelayConfig::GENERAL_MIDI_CHORUS`] into source 1, and its reverb
    /// send through a [`ReverbNode`] into source 2. Each effect declares its
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
        let (reverb, _) = graph.insert(ReverbNode::new());
        let (sum, ()) = graph.insert(ChannelSumNode::new(3, ChannelLayout::STEREO));
        // The unit's outputs: dry L/R, chorus send L/R, reverb send.
        graph.set_source(chorus, 0, GraphSource::node(unit, 2));
        graph.set_source(chorus, 1, GraphSource::node(unit, 3));
        graph.set_source(reverb, 0, GraphSource::node(unit, 4));
        // The sum's inputs: source 0 (dry), 1 (chorus), 2 (reverb), each L/R.
        let strip = instrument.out();
        let sources = [
            (strip, 0),
            (strip, 1),
            (chorus, 0),
            (chorus, 1),
            (reverb, 0),
            (reverb, 1),
        ];
        for (port, (node, out)) in sources.into_iter().enumerate() {
            graph.set_source(sum, port, GraphSource::node(node, out));
        }
        Ok(GeneralMidi {
            instrument,
            chorus,
            reverb,
            sum,
        })
    }
}

use crate::IntoNode;

/// An instrument names the pair of its outputs that carries the sound a host
/// plays.
///
/// What it is, beyond that, is in its shape, not in methods: MIDI arrives on
/// event input 0, and audio leaves on the outputs. A `SoundFont` after Stage 2
/// has five outputs, a plugin has the buses it declares; the main pair is the
/// two a host wires to a mix, the rest (effect sends, extra buses) stay on the
/// node's own ports.
///
/// A hosted plugin is an instrument only if its format says so, which is known
/// when it is open, not when it is typed: `tutti-session` converts one fallibly
/// (`Session::instrument_plugin`).
pub trait Instrument: IntoNode {
    /// The left and right output ports of the sound a host plays. The first two
    /// by default.
    fn main_outputs(&self) -> [u16; 2] {
        [0, 1]
    }
}

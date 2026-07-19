//! Self-reported MIDI routing address.

use crate::unit_id::MidiUnitId;

/// Implemented by audio units that can receive MIDI events.
///
/// Complements the three hot-path traits:
/// - [`crate::MidiInputSource`] — produces raw `(port, event)` pairs
/// - [`crate::MidiQueue`] — delivers events to per-unit queues
/// - [`crate::MidiSource`] — drains queues into a unit's buffer
///
/// `MidiTarget` fills the gap between insertion into the graph and
/// routing: outside callers need a way to ask a unit "what is your
/// routing address?" without conflating that with fundsp's type-fingerprint
/// `get_id()`. Only units that actually receive MIDI implement this, so
/// attempting to route MIDI to a plain DSP node is a downcast miss, not a
/// silent ID collision.
pub trait MidiTarget {
    /// Return the unit's MIDI routing address.
    ///
    /// The returned value must be stable for the unit's lifetime and
    /// should have been allocated via [`MidiUnitId::next`].
    fn midi_unit_id(&self) -> MidiUnitId;
}

//! Opaque identifier for a MIDI-receiving audio unit.

use core::sync::atomic::Ordering;
use std::sync::atomic::AtomicU64;

/// Opaque per-instance identifier for a MIDI-receiving audio unit.
///
/// Each MIDI-receiving unit (synth, sampler, plugin) holds one of these
/// and uses it as its address in the routing tables. IDs are
/// allocated via [`MidiUnitId::next`] from a process-wide atomic counter,
/// so collisions are impossible by construction.
///
/// `From<u64>` / [`MidiUnitId::new`] stay available for deserialization
/// and for tests that need deterministic values; they do **not** increment
/// the allocator.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct MidiUnitId(u64);

/// Process-wide allocator for `MidiUnitId`s. Starts at 1 so that the
/// `Default` value (0) is always distinguishable from a real ID.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl MidiUnitId {
    /// Allocate a fresh `MidiUnitId` unique across the process.
    ///
    /// Call once per MIDI-receiving unit at construction time.
    #[inline]
    pub fn next() -> Self {
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Construct from a raw `u64`. Prefer [`MidiUnitId::next`] for new
    /// units; this is for deserialization and deterministic tests.
    #[inline]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The underlying integer, for storage or FFI.
    ///
    /// Prefer passing the `MidiUnitId` itself — unwrapping here gives up the
    /// distinction from every other `u64` in scope.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for MidiUnitId {
    #[inline]
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<MidiUnitId> for u64 {
    /// The raw id (see [`MidiUnitId::as_u64`]).
    #[inline]
    fn from(id: MidiUnitId) -> Self {
        id.0
    }
}

impl core::fmt::Display for MidiUnitId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MidiUnitId({})", self.0)
    }
}

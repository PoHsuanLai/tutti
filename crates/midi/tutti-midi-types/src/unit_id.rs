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

    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Base for plugin MIDI-out **port** numbers, chosen high so they never collide
/// with the small device-driven hardware input port indices (0, 1, 2, …). A
/// plugin that emits MIDI routes its output through the same
/// [`MidiRoutingSnapshot`](crate::MidiRoutingSnapshot) as a hardware port, keyed
/// on a port index allocated by [`next_plugin_out_port`].
pub const PLUGIN_OUT_PORT_BASE: usize = 1 << 20;

/// Process-wide allocator for plugin MIDI-out port indices, disjoint from
/// hardware input ports (see [`PLUGIN_OUT_PORT_BASE`]).
static NEXT_PLUGIN_OUT_PORT: AtomicU64 = AtomicU64::new(PLUGIN_OUT_PORT_BASE as u64);

/// Allocate a fresh plugin MIDI-out port index, unique across the process and
/// disjoint from hardware input port indices. Call once per plugin that emits
/// MIDI, at wiring time.
#[inline]
pub fn next_plugin_out_port() -> usize {
    NEXT_PLUGIN_OUT_PORT.fetch_add(1, Ordering::Relaxed) as usize
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

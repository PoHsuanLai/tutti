//! This crate's `AudioUnit::get_id` fingerprints.
//!
//! Per the convention in [`tutti_core::node_id`], each crate owns the ids for
//! the node types it defines. `tutti-io` defines exactly one — the mic monitor —
//! which moved here from tutti-sampler with its unit.
//!
//! The mnemonic is unchanged (`MICMONIT`), deliberately: `get_id` is a *type*
//! fingerprint, so altering it would change the deterministic hash fundsp seeds
//! from it. Relocating a type across crates must not resound its noise.

/// `MICMONIT` — [`MicMonitorNode`](crate::MicMonitorNode).
pub(crate) const MIC_MONITOR_ID: u64 = 0x_4D49_434D_4F4E_4954;

const _: () = tutti_core::assert_unique(&[MIC_MONITOR_ID]);

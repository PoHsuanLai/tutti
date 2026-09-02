//! Type-fingerprint IDs returned by [`AudioUnit::get_id`].
//!
//! [`AudioUnit::get_id`] is fundsp's type-fingerprint hook. It serves two
//! purposes:
//!
//! 1. **Deterministic hashing** — fundsp's `ping()` mixes `get_id()` into a
//!    per-unit pseudorandom hash. Two instances of the same type MUST return
//!    the same value, or the hash (and any seeded noise, oscillator phase,
//!    etc.) becomes non-deterministic between runs.
//! 2. **Graph scanning** — tutti's PDC code uses marker ids to find
//!    auto-inserted delay nodes without carrying state between commits.
//!
//! Per-instance routing identity (for MIDI dispatch) lives on
//! [`MidiUnitId`], exposed by each MIDI-receiving unit's inherent
//! `midi_unit_id()`, *not* on `get_id()`.
//!
//! ## Ownership — each crate owns its own ids
//!
//! This module holds ONLY the ids for tutti-core's own infrastructure nodes
//! (PDC delays + transport/automation). Higher crates own the ids for the node
//! types they define, in their own `node_id` module: tutti-nodes (filters,
//! delay, modulation, dynamics, spatial, automation-lane), tutti-polysynth
//! (`POLYSYNT`), tutti-soundfont (`\0RUSTYSY`), tutti-plugin (`PLUGINCL`), tutti-sampler
//! (`SAMPLRND`, `STRSMPLR`, `TSTRCHNT`, `VOICENOD`), tutti-io (`MICMONIT`).
//! This keeps core the bottom layer: it names no node type that lives above
//! it.
//!
//! ## Collisions
//!
//! fundsp reserves small integers (0..~100) for its own AudioNode types; every
//! id here is a 64-bit ASCII mnemonic, well outside that range (enforced by
//! [`mnemonic`]). Uniqueness *within* a crate is a compile error via
//! [`assert_unique`]. Cross-crate uniqueness rests on the mnemonic convention
//! plus this ledger of every reserved 8-char mnemonic (keep it current when
//! adding an id in any crate):
//!
//! ```text
//! core:    PDCDE   MPDC    TRNSCLK\0  AUTOINPT  Click (5 bytes — see CLICK_NODE_ID)
//! units:   AUTOMATE SVFFILT1 LADDERF1 EQBANDN1 DLYNODE1 DLYNODE2 LFO_NODE
//!          PHASERN1 CHORUSN1 FLANGER1 DISTORT1 CONVNOD1 CONVNOD2 SCGATE
//!          SCCOMP  SSCGAT  SSCCOM  LIMITER1 BRKWLLMT PAN\0  BIN\0
//!          (svf/ladder/phaser also derive a stereo sibling via `^ 0xDA02`;
//!           PAN\0 ORs num_outputs into the low byte)
//! polysynth: POLYSYNT
//! soundfont: \0RUSTYSY
//! plugin:  PLUGINCL
//! sampler: SAMPLRND STRSMPLR TSTRCHNT VOICENOD
//! io:      MICMONIT
//! ```
//!
//! [`AudioUnit::get_id`]: fundsp::audiounit::AudioUnit::get_id
//! [`MidiUnitId`]: tutti_midi_types::MidiUnitId

/// Pack an 8-byte ASCII mnemonic into a `get_id()` fingerprint (big-endian).
///
/// The single source of truth for a node id: write `mnemonic(b"SVFFILT1")`
/// rather than a hand-computed hex literal, so the value can never drift from
/// the mnemonic. Const-asserts the result is outside fundsp's reserved
/// small-integer range (0..=100) — any 8-char ASCII string packs far above it,
/// but a caller passing mostly-NUL bytes (a short mnemonic) is caught here.
///
/// Each crate derives its own node ids from this; collisions *within* a crate
/// are a compile error via [`assert_unique`]. Cross-crate uniqueness is held by
/// the mnemonic convention + the reserved-mnemonic ledger in this module's docs.
pub const fn mnemonic(s: &[u8; 8]) -> u64 {
    let v = u64::from_be_bytes(*s);
    assert!(
        v > 100,
        "node-id mnemonic collides with fundsp's reserved range"
    );
    v
}

/// Compile-time assert that a crate's own node ids are all distinct.
///
/// Use as `const _: () = assert_unique(&[ID_A, ID_B, …]);` in each crate's
/// `node_id` module. A duplicate fails `cargo build` (const-eval panic), so the
/// "all ids in one file are eyeball-unique" guarantee survives decentralization
/// for the realistic (intra-crate copy-paste) collision.
pub const fn assert_unique(ids: &[u64]) {
    let mut i = 0;
    while i < ids.len() {
        let mut j = i + 1;
        while j < ids.len() {
            assert!(ids[i] != ids[j], "duplicate node id within crate");
            j += 1;
        }
        i += 1;
    }
}

// ──────────────────────────────────────────────────────────────────────
// PDC marker — re-exported from fundsp, which owns the delay node and the
// `clear_delays` scan that looks for it.
// ──────────────────────────────────────────────────────────────────────
pub use fundsp::latency::PDC_DELAY_ID;

// ──────────────────────────────────────────────────────────────────────
// Transport / control nodes
// ──────────────────────────────────────────────────────────────────────
/// [`TransportClock`](crate::TransportClock)'s type fingerprint — the mnemonic
/// `"TRNSCLK\0"`.
pub const TRANSPORT_CLOCK_ID: u64 = 0x_5452_4E53_434C_4B00;

/// [`ClickNode`](crate::ClickNode)'s type fingerprint — the five ASCII bytes
/// `"Click"`, right-aligned.
///
/// **Not `mnemonic(b"CLICKND1")`, and it must not become one.** This is the
/// literal that `AudioNode::ID` carried while the metronome was an
/// `An<ClickNode>`, and `get_id` feeds `ping`, which seeds every node's
/// pseudorandom phase. Repacking it to this module's usual 8-byte convention
/// would change the graph hash — a silent, audible change with no test that
/// could name it — for the sake of a tidier spelling. It is pinned by
/// `click_node_id_survived_the_audionode_rewrite`.
pub const CLICK_NODE_ID: u64 = 0x0000_0043_6c69_636b;

// Compile-time intra-crate uniqueness guard for core's own ids.
const _: () = assert_unique(&[PDC_DELAY_ID, TRANSPORT_CLOCK_ID, CLICK_NODE_ID]);

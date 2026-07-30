//! What the engine wants from a plugin, as one capability flag set.
//!
//! Modeled on wgpu's `Features`: a single [`bitflags`] set, read in both
//! directions. The host checks the negotiated flags to adapt (does this plugin
//! take MIDI? does it have an editor?); the engine checks the [`Features::CONSUMES`]
//! mask to gate per-block side-band sends (transport, automation, note
//! expression, sequencer context). One bit means both "the plugin can consume
//! this" and "so send it" — there is no plugin that can consume a thing yet
//! should not be sent it, so capability and want collapse to one bit.
//!
//! Numeric wiring (bus widths, latency samples) is NOT here — that is the
//! `Limits` half, and lives on [`crate::LoadedPlugin`] as plain fields. Only
//! flags that are *not* recoverable from those numbers are stored here;
//! `multi_bus` / `latency` stay derived methods on `LoadedPlugin`.
//!
//! The `Serialize`/`Deserialize` derives are gated behind the `serde` feature
//! (the IPC wire path enables it), and serialize as the underlying bits.
//!
//! This is a fixed list of the functionality we support — not a superset of
//! what the formats emit. The three kinds (Required / Negotiated / Best-effort)
//! and the per-format capability table live in the `tutti-plugin` crate README
//! (`## Capability model`). Keep that table in sync with the per-format loaders
//! in `tutti-plugin-server/src/loaders/`, which are the source of truth for what
//! each format actually reports.

use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

bitflags! {
    /// The capabilities a loaded plugin reports. Filled at load time by the
    /// per-format loader (live-probed where the format exposes a query).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct Features: u16 {
        // --- Audio (negotiated) ---
        /// Plugin negotiated 64-bit sample processing at activation.
        /// (Was `LoadedPlugin::supports_f64`.)
        const F64_AUDIO = 1 << 0;

        // --- MIDI (negotiated) ---
        /// Plugin consumes MIDI / note input.
        const MIDI_IN = 1 << 1;
        /// Plugin emits MIDI the host reads back.
        const MIDI_OUT = 1 << 2;

        // --- Editor (negotiated) ---
        /// Plugin has an editor / GUI. (Also kept on `PluginDescriptor` for
        /// the persisted, browse-before-load path; this is the post-load
        /// authoritative copy.)
        const EDITOR = 1 << 3;
        /// Editor supports host-driven resize.
        const EDITOR_RESIZE = 1 << 4;

        // --- Consumes (best-effort — the per-block send-gate set) ---
        /// Plugin wants transport / tempo / playhead each block.
        const TRANSPORT = 1 << 5;
        /// Plugin wants sample-accurate parameter automation curves
        /// (vs. only coarse `set_parameter`).
        const PARAM_AUTOMATION = 1 << 6;
        /// Plugin wants MPE / per-note expression events.
        const NOTE_EXPRESSION = 1 << 7;
        /// Plugin wants the sequencer-context bundle (chords / scales /
        /// per-note text+int). Audience of one format (VST3), by spec.
        const SEQUENCER_CONTEXT = 1 << 8;

        // --- Reactions (host → plugin advisories — NOT per-block feeds) ---
        /// Plugin reacts to host automation-state changes (VST3
        /// `IAutomationState`): the host tells it when it is reading / writing
        /// automation so the editor can show UI feedback (a glowing knob ring).
        /// A *reaction* gate, deliberately NOT part of [`Features::CONSUMES`] —
        /// it gates an edge-triggered host→plugin call, not a per-block send.
        const AUTOMATION_STATE = 1 << 9;
    }
}

impl Features {
    /// The best-effort set the engine gates per-block sends on. Making the
    /// bucket a named mask (rather than a doc comment on scattered fields) is
    /// the anti-hybrid guarantee: the send path reads
    /// `features.intersection(Features::CONSUMES)`, never a format name.
    pub const CONSUMES: Features = Features::TRANSPORT
        .union(Features::PARAM_AUTOMATION)
        .union(Features::NOTE_EXPRESSION)
        .union(Features::SEQUENCER_CONTEXT);
}

/// Which capabilities a loader actually probed, paired with [`Features`].
///
/// A clear bit in `Features` answers three different questions the same way:
/// the plugin said no, our loader never asked, or the format has no way to ask.
/// The send-gate does not care — an unsent block is an unsent block — but a UI
/// badge and a "why is this greyed out?" answer do, and so does anyone auditing
/// what a loader covers.
///
/// This is the [`ParamFlags`](crate::ParamFlags) `known`-mask shape one level
/// up. It is deliberately NOT folded into `Features` as a second bit per
/// capability: the gate path reads `Features` on every block and must stay a
/// single mask-and-compare.
///
/// The distinction between "we didn't implement it" and "the format can't" is
/// the `○` / `✕` split in the per-format capability table in the `tutti-plugin`
/// README. Both are absent from `probed`, because both mean the same thing to a
/// consumer: no plugin answered. Which of the two it is belongs in that table,
/// next to the reason — not duplicated in a runtime bit that would go stale the
/// moment a loader grows the missing path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FeatureReport {
    /// The capabilities the plugin reported, as flags.
    pub features: Features,
    /// The capabilities the loader actually probed. A bit clear here means
    /// nothing was asked, so the matching `features` bit carries no claim.
    pub probed: Features,
}

impl FeatureReport {
    /// Record `features` as the answers to the `probed` capabilities.
    ///
    /// Takes both together so a loader cannot report a bit it never probed, nor
    /// probe a bit it left unset — the same constructor discipline as
    /// [`ParameterInfo::with_flags`](crate::ParameterInfo::with_flags).
    pub fn new(probed: Features, features: Features) -> Self {
        Self {
            probed,
            features: features & probed,
        }
    }

    /// Every capability probed, with the given answers. For a loader that asks
    /// the whole set.
    pub fn all_probed(features: Features) -> Self {
        Self::new(Features::all(), features)
    }

    /// `Some(true)`/`Some(false)` when the loader probed this capability,
    /// [`None`] when it did not.
    ///
    /// Pass exactly one bit; a multi-bit query answers whether *all* of them
    /// were probed and set.
    pub fn get(&self, f: Features) -> Option<bool> {
        self.probed.contains(f).then(|| self.features.contains(f))
    }

    /// The conservative read the per-block send-gate uses: an unprobed
    /// capability is not sent.
    ///
    /// Distinct from [`get`](Self::get) by intent — this is for the hot path,
    /// which has no way to act on "unknown" and must pick a side. `features` is
    /// already masked by `probed` in the constructor, so this is the stored
    /// value rather than a second decision.
    pub fn enabled(&self, f: Features) -> bool {
        self.features.contains(f)
    }
}

/// What each format's loader probes — the `probed` half of
/// [`LoadedPlugin`](crate::LoadedPlugin).
///
/// Named constants rather than expressions inside the load paths, which sit
/// behind per-format `cfg`s and need a real plugin to reach. A claim about what
/// a loader probes should be readable, diffable, and testable without one.
///
/// They live here, beside [`Features`], because two crates build the same
/// format's report: `tutti-plugin-server` loads VST2 out of process and
/// `tutti-plugin` loads it in process. Those answer the same five questions, and
/// a second copy of the list is the drift this module exists to prevent.
///
/// A bit absent here means the loader did not ask. *Why* it did not — the format
/// has no query, or we have not implemented the path — is the `✕` / `○` split in
/// the `tutti-plugin` README capability table. That distinction is
/// documentation, not a runtime bit: it describes this codebase, not the plugin,
/// and would go stale the moment a loader grows the missing path.
pub mod probed {
    use super::Features;

    /// VST3 probes everything except `AUTOMATION_STATE`, which no loader sets.
    pub const VST3: Features = Features::all().difference(Features::AUTOMATION_STATE);

    /// CLAP probes everything except sequencer context (no chord/scale events in
    /// the spec) and `AUTOMATION_STATE`.
    pub const CLAP: Features = Features::all()
        .difference(Features::SEQUENCER_CONTEXT)
        .difference(Features::AUTOMATION_STATE);

    /// VST2 answers five, in or out of process. It has no query for editor
    /// resize, note expression, or sequencer context, and neither loader probes
    /// `effCanDo` for sample-accurate automation.
    pub const VST2: Features = Features::F64_AUDIO
        .union(Features::MIDI_IN)
        .union(Features::MIDI_OUT)
        .union(Features::EDITOR)
        .union(Features::TRANSPORT);

    /// AU answers one. The other nine are unimplemented in this host, not
    /// declined by the units — an AU that takes MIDI still reports no `MIDI_IN`
    /// here.
    pub const AU: Features = Features::EDITOR;

    /// The WASM world (`dawai:audio-plugin` v0.1) is headless, f32-only, and
    /// single-bus by contract, so `EDITOR` and `F64_AUDIO` are genuine
    /// answers rather than gaps — unlike AU, where the same clear bits mean
    /// nobody asked. Guest MIDI output is discarded, so `MIDI_OUT` is a real
    /// `false` too.
    pub const WASM: Features = Features::MIDI_IN
        .union(Features::MIDI_OUT)
        .union(Features::EDITOR)
        .union(Features::F64_AUDIO);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "The plugin said no" and "nobody asked" are different answers.
    ///
    /// The AU loader determines exactly one capability (`EDITOR`); the other
    /// nine are unimplemented host-side. Before this, its `Features::empty()`
    /// was indistinguishable from a plugin that was asked all ten and declined.
    #[test]
    fn an_unprobed_capability_is_not_a_declined_one() {
        let au = FeatureReport::new(Features::EDITOR, Features::EDITOR);

        assert_eq!(au.get(Features::EDITOR), Some(true));
        assert_eq!(
            au.get(Features::MIDI_IN),
            None,
            "a capability the loader never probed must not read as false"
        );

        let declined = FeatureReport::new(Features::MIDI_IN, Features::empty());
        assert_eq!(declined.get(Features::MIDI_IN), Some(false));
    }

    /// The constructor cannot report a bit outside the probed mask.
    #[test]
    fn a_report_cannot_claim_an_unprobed_bit() {
        let r = FeatureReport::new(Features::EDITOR, Features::EDITOR | Features::MIDI_IN);
        assert_eq!(r.get(Features::EDITOR), Some(true));
        assert_eq!(r.get(Features::MIDI_IN), None);
        assert!(!r.features.contains(Features::MIDI_IN));
    }

    /// The send-gate keeps its single mask-and-compare, and an unprobed
    /// capability is never sent.
    #[test]
    fn the_send_gate_treats_unprobed_as_off() {
        let au = FeatureReport::new(Features::EDITOR, Features::EDITOR);
        assert!(!au.enabled(Features::TRANSPORT));
        assert!(au.features.intersection(Features::CONSUMES).is_empty());

        let vst2 = FeatureReport::new(
            Features::TRANSPORT | Features::NOTE_EXPRESSION,
            Features::TRANSPORT,
        );
        assert!(vst2.enabled(Features::TRANSPORT));
        assert_eq!(vst2.get(Features::NOTE_EXPRESSION), Some(false));
    }

    /// A default report claims nothing rather than claiming absence.
    #[test]
    fn a_defaulted_report_claims_nothing() {
        let r = FeatureReport::default();
        for f in [
            Features::F64_AUDIO,
            Features::MIDI_IN,
            Features::EDITOR,
            Features::TRANSPORT,
        ] {
            assert_eq!(r.get(f), None);
            assert!(!r.enabled(f));
        }
    }

    /// `all_probed` leaves no capability unanswered.
    #[test]
    fn all_probed_answers_every_capability() {
        let r = FeatureReport::all_probed(Features::EDITOR);
        assert_eq!(r.get(Features::EDITOR), Some(true));
        assert_eq!(r.get(Features::MIDI_IN), Some(false));
    }

    /// Both halves survive the bincode wire the IPC peers speak. A `probed`
    /// mask that failed to cross would turn every unprobed bit into a
    /// reported `false` on the far side.
    #[cfg(feature = "serde")]
    #[test]
    fn the_probed_mask_survives_the_bincode_round_trip() {
        let r = FeatureReport::new(Features::EDITOR, Features::EDITOR);
        let bytes = bincode::serialize(&r).expect("serialize");
        let back: FeatureReport = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.get(Features::EDITOR), Some(true));
        assert_eq!(back.get(Features::MIDI_IN), None);
    }

    /// Each loader's claim, pinned. These are assertions about our own coverage,
    /// so they change only when a loader grows or loses a probe — at which point
    /// the README capability table needs the same edit.
    #[test]
    fn each_loader_claims_only_what_it_probes() {
        assert_eq!(probed::AU, Features::EDITOR);
        assert_eq!(probed::VST2.bits().count_ones(), 5);
        assert!(!probed::VST3.contains(Features::AUTOMATION_STATE));
        assert!(!probed::CLAP.contains(Features::SEQUENCER_CONTEXT));
    }

    /// The AU loader probes exactly one capability, so nine read as "nobody
    /// asked" rather than as refusals. This is the gap the mask exists to
    /// expose; if AU ever probes MIDI, this test is the reminder to say so.
    #[test]
    fn the_au_loader_claims_only_the_editor() {
        for f in [
            Features::MIDI_IN,
            Features::MIDI_OUT,
            Features::F64_AUDIO,
            Features::TRANSPORT,
            Features::PARAM_AUTOMATION,
        ] {
            assert!(
                !probed::AU.contains(f),
                "{f:?} is not probed by the AU loader, so it must not be claimed"
            );
        }
    }

    /// No loader claims `AUTOMATION_STATE` — nothing sets that bit anywhere, so
    /// claiming it would assert a probe that does not exist.
    #[test]
    fn no_loader_claims_a_capability_nothing_probes() {
        for (name, mask) in [
            ("vst3", probed::VST3),
            ("clap", probed::CLAP),
            ("vst2", probed::VST2),
            ("au", probed::AU),
            ("wasm", probed::WASM),
        ] {
            assert!(
                !mask.contains(Features::AUTOMATION_STATE),
                "{name} claims AUTOMATION_STATE, which no loader populates"
            );
        }
    }

    /// WASM's cleared bits are answers, not gaps — the world is headless and
    /// f32-only by contract, so nothing needs to be probed to know it.
    #[test]
    fn the_wasm_world_answers_what_its_contract_fixes() {
        assert!(
            probed::WASM.contains(Features::EDITOR),
            "headless by contract is an answered absence, not an unasked question"
        );
        assert!(probed::WASM.contains(Features::F64_AUDIO));
        assert!(
            !probed::WASM.contains(Features::TRANSPORT),
            "transport is genuinely not wired in v0.1, so it stays unanswered"
        );
    }
}

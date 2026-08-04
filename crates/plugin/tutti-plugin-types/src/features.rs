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

        // --- Presets (host → plugin advisories — NOT per-block feeds) ---
        /// Plugin can enumerate its own presets by name.
        ///
        /// Two bits rather than one because the formats split exactly here:
        /// AU answers both, CLAP answers only [`Features::PRESET_LOAD`] (its
        /// preset *discovery* is a separate extension, unbound here), so one
        /// combined bit could not describe CLAP without either over- or
        /// under-claiming.
        ///
        /// VST3 sets neither, and that absence is an answer rather than a gap:
        /// a VST3 program is an ordinary parameter carrying
        /// `kIsProgramChange`, selected through the parameter path like any
        /// other value. There is no second mechanism for a bit to describe.
        const PRESET_LIST = 1 << 10;
        /// Host can ask the plugin to load one of its presets.
        ///
        /// Independent of [`Features::PRESET_LIST`]: CLAP loads a preset from a
        /// filesystem path without being able to list what is available. Like
        /// [`Features::AUTOMATION_STATE`], both preset bits are edge-triggered
        /// host→plugin actions and so stay out of [`Features::CONSUMES`].
        const PRESET_LOAD = 1 << 11;
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
    ///
    /// The preset bits are *answered*, not skipped: VST3 routes program
    /// selection through a parameter flagged `kIsProgramChange`, so there is no
    /// separate preset mechanism to report. Both bits are probed and clear.
    pub const VST3: Features = Features::all().difference(Features::AUTOMATION_STATE);

    /// CLAP probes everything except sequencer context (no chord/scale events in
    /// the spec), `AUTOMATION_STATE`, and `PRESET_LIST`.
    ///
    /// `PRESET_LIST` is unprobed rather than declined: CLAP enumerates presets
    /// through the preset-*discovery* extension, which is a factory-level query
    /// this host does not bind. `CLAP_EXT_PRESET_LOAD` answers only whether a
    /// preset can be loaded from a path, so it cannot stand in for the list.
    pub const CLAP: Features = Features::all()
        .difference(Features::SEQUENCER_CONTEXT)
        .difference(Features::AUTOMATION_STATE)
        .difference(Features::PRESET_LIST);

    /// VST2 answers five, in or out of process. It has no query for editor
    /// resize, note expression, or sequencer context, and neither loader probes
    /// `effCanDo` for sample-accurate automation.
    pub const VST2: Features = Features::F64_AUDIO
        .union(Features::MIDI_IN)
        .union(Features::MIDI_OUT)
        .union(Features::EDITOR)
        .union(Features::TRANSPORT);

    /// AU answers four: MIDI input, the editor, and both preset bits. The rest
    /// are unimplemented in this host, not declined by the units.
    ///
    /// `MIDI_IN` is answered from the component type — an AU is an instrument,
    /// music effect or MIDI processor, or it is not — which is the same
    /// predicate the process path gates its per-block `send_midi` on. A plain
    /// `aufx` effect therefore reports `Some(false)` rather than silence.
    ///
    /// `MIDI_OUT` stays unprobed even though the two look symmetric: reading
    /// MIDI back out of an AU needs a host callback installed on the unit, and
    /// this loader installs none, so no unit has been asked.
    ///
    /// The preset bits are live-probed together because one AU property backs
    /// both: a unit answering `kAudioUnitProperty_FactoryPresets` can be asked
    /// to load any preset it listed.
    pub const AU: Features = Features::MIDI_IN
        .union(Features::EDITOR)
        .union(Features::PRESET_LIST)
        .union(Features::PRESET_LOAD);
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
        assert_eq!(
            probed::AU,
            Features::MIDI_IN | Features::EDITOR | Features::PRESET_LIST | Features::PRESET_LOAD
        );
        assert_eq!(probed::VST2.bits().count_ones(), 5);
        assert!(!probed::VST3.contains(Features::AUTOMATION_STATE));
        assert!(!probed::CLAP.contains(Features::SEQUENCER_CONTEXT));
    }

    /// The AU loader probes MIDI input, the editor and the two preset bits; the
    /// remaining six read as "nobody asked" rather than as refusals. This is the
    /// gap the mask exists to expose.
    #[test]
    fn the_au_loader_leaves_the_unimplemented_capabilities_unasked() {
        for f in [
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

    /// The AU loader gates its per-block `send_midi` on the component type, so an
    /// instrument (`aumu`) is sent MIDI and must declare `MIDI_IN`; a plain
    /// effect (`aufx`) is not sent MIDI and must decline it. Two sources of truth
    /// for one fact only stay honest if both are pinned.
    ///
    /// The predicate is `AuType::receives_midi`, which is macOS-only code behind
    /// the `au` feature; this test restates the same instrument/effect split
    /// against the mask, so it runs on every platform.
    #[test]
    fn an_au_instrument_reports_the_midi_input_it_is_actually_sent() {
        assert!(
            probed::AU.contains(Features::MIDI_IN),
            "MIDI_IN must be probed, or an instrument's answer cannot be read back"
        );

        let instrument = FeatureReport::new(probed::AU, Features::MIDI_IN | Features::EDITOR);
        assert_eq!(instrument.get(Features::MIDI_IN), Some(true));
        assert!(instrument.enabled(Features::MIDI_IN));

        let effect = FeatureReport::new(probed::AU, Features::EDITOR);
        assert_eq!(
            effect.get(Features::MIDI_IN),
            Some(false),
            "an aufx effect is never sent MIDI, and now says so rather than staying silent"
        );
    }

    /// MIDI output is not the mirror of MIDI input here. Reading events back out
    /// of an AU needs a host callback this loader never installs, so no unit has
    /// been asked and the bit must stay silent rather than reporting a refusal.
    #[test]
    fn au_midi_output_is_unasked_rather_than_declined() {
        let report = FeatureReport::new(probed::AU, Features::MIDI_IN);
        assert_eq!(report.get(Features::MIDI_IN), Some(true));
        assert_eq!(report.get(Features::MIDI_OUT), None);
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
        ] {
            assert!(
                !mask.contains(Features::AUTOMATION_STATE),
                "{name} claims AUTOMATION_STATE, which no loader populates"
            );
        }
    }

    /// The two preset bits are independent, because one format answers exactly
    /// one of them. Collapsing them would force CLAP to either claim an
    /// enumeration it cannot perform or disclaim a load it can.
    #[test]
    fn a_format_can_load_a_preset_without_being_able_to_list_one() {
        assert!(probed::CLAP.contains(Features::PRESET_LOAD));
        assert!(
            !probed::CLAP.contains(Features::PRESET_LIST),
            "CLAP enumerates through preset-discovery, which this host does not bind"
        );

        // AU backs both from one property, so it answers both.
        assert!(probed::AU.contains(Features::PRESET_LIST));
        assert!(probed::AU.contains(Features::PRESET_LOAD));
    }

    /// VST3's clear preset bits are an answer, not a gap. Its programs are
    /// ordinary parameters carrying `kIsProgramChange`, reached through the
    /// parameter path, so there is no second mechanism to report. If a bit ever
    /// gets set here, something has invented a preset API VST3 does not have.
    #[test]
    fn vst3_answers_that_it_has_no_separate_preset_mechanism() {
        let probed_both = probed::VST3.contains(Features::PRESET_LIST | Features::PRESET_LOAD);
        assert!(probed_both, "both bits are asked");

        let report = FeatureReport::new(probed::VST3, Features::EDITOR);
        assert_eq!(report.get(Features::PRESET_LIST), Some(false));
        assert_eq!(report.get(Features::PRESET_LOAD), Some(false));
    }

    /// Neither preset bit joins the per-block send-gate. Both are edge-triggered
    /// host→plugin actions, like `AUTOMATION_STATE`; adding either to `CONSUMES`
    /// would put a UI action on the audio path.
    #[test]
    fn preset_capabilities_are_not_per_block_sends() {
        assert!(!Features::CONSUMES.contains(Features::PRESET_LIST));
        assert!(!Features::CONSUMES.contains(Features::PRESET_LOAD));
    }

    /// VST2 asks neither. It has `effGetProgramName`/`effSetProgram`, which this
    /// host does not bind, so both bits are unasked rather than declined.
    #[test]
    fn the_vst2_loader_does_not_claim_presets() {
        assert_eq!(probed::VST2.contains(Features::PRESET_LIST), false);
        assert_eq!(probed::VST2.contains(Features::PRESET_LOAD), false);

        let report = FeatureReport::new(probed::VST2, Features::EDITOR);
        assert_eq!(report.get(Features::PRESET_LOAD), None);
    }
}

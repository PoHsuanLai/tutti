//! Shared plugin-classification vocabulary.
//!
//! Each native format reports a category/type taxonomy. These neutral mirrors
//! live here so both the format host crate (which maps its native SDK enum into
//! one, e.g. `tutti-vst2-host` from the `vst` crate's `Category`) and the
//! catalog/wire layer in `tutti-plugin` (which embeds it in `PluginClass`)
//! speak one type instead of each keeping a private copy plus a hand-written
//! cross-walk.
//!
//! Distinct from [`crate::metadata`], which holds the *engine-wiring* load data
//! (bus widths, latency); this is the *classification* taxonomy.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The plugin's declared VST2 category — a neutral mirror of the `vst` crate's
/// `Category`, owned here so neither the host crate's public API nor the wire
/// vocab leaks the `vst` dependency. `tutti-vst2-host` maps the native value
/// into this; `tutti-plugin` embeds it in `PluginClass::Vst2`.
///
/// Three kinds of answer, kept apart:
///
/// - a named variant — the plugin returned a code VST 2.4 assigns a meaning to;
/// - [`Unrecognized`](Self::Unrecognized) — it returned something else, and the
///   number is carried so a caller can still see it;
/// - [`Unasked`](Self::Unasked) — nobody called `effGetPlugCategory`.
///
/// The last two used to be one bare `Unknown` that was also `#[default]`, so a
/// scan that never queried and a plugin that answered `kPlugCategUnknown` were
/// the same value. `kPlugCategUnknown` is itself a real answer (0), and it maps
/// to `Unrecognized(0)` rather than to `Unasked` — the plugin did reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Vst2Category {
    /// Nobody asked. The default, so a descriptor built by struct-update claims
    /// nothing rather than claiming the plugin declined to classify itself.
    #[default]
    Unasked,
    /// The plugin answered with a code this enum does not name, carried
    /// verbatim. Includes `kPlugCategUnknown` (0), which is a plugin saying it
    /// has no category — an answer, not a silence.
    Unrecognized(i32),
    Effect,
    Synth,
    Analysis,
    Mastering,
    Spacializer,
    RoomFx,
    SurroundFx,
    Restoration,
    OfflineProcess,
    Shell,
    Generator,
}

impl Vst2Category {
    /// `true` when a plugin answered at all, whether or not the code is named.
    pub fn was_answered(&self) -> bool {
        !matches!(self, Self::Unasked)
    }
}

/// One facet of a VST3 subcategory string.
///
/// VST3's taxonomy is **not** a list of alternatives. `ivstaudioprocessor.h`
/// defines 44 `PlugType` constants, but they are `|`-delimited *compositions*
/// over these 37 facets — `kInstrumentSynthSampler` is the single string
/// `"Instrument|Synth|Sampler"`, carrying three of them. A variant per constant
/// would need one per combination and still miss the vendor tails plugins ship,
/// so the facet is the unit and [`Vst3SubCategories`] holds the set.
///
/// [`Other`](Self::Other) carries a facet the SDK does not name. Vendor tails
/// are legal; discarding one would spell "the plugin declared nothing", which is
/// the flattening [`Vst2Category::Unrecognized`] exists to prevent.
///
/// `Drum` and `Drums` are both here, and are not a typo to fix: the SDK declares
/// `kInstrumentDrum` as `"Instrument|Drum"` and `kFxDrums` as `"Fx|Drums"`. A
/// host that silently folds them apart from the SDK stops round-tripping.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Vst3PlugType {
    // Top-level roles.
    Fx,
    Instrument,
    Generator,
    Analyzer,
    Spatial,
    Mastering,
    // Instrument families.
    Synth,
    Sampler,
    Drum,
    Piano,
    External,
    // Effect families.
    Delay,
    Reverb,
    Distortion,
    Dynamics,
    Eq,
    Filter,
    Modulation,
    PitchShift,
    Restoration,
    Tools,
    Network,
    ChannelStrip,
    Drums,
    // Source hints.
    Guitar,
    Vocals,
    Bass,
    Microphone,
    // Channel hints.
    Mono,
    Stereo,
    Surround,
    Ambisonics,
    UpDownmix,
    // Processing constraints.
    OnlyRt,
    OnlyOfflineProcess,
    NoOfflineProcess,
    OnlyAra,
    /// A facet the SDK does not name, carried verbatim.
    Other(String),
}

impl Vst3PlugType {
    /// Parse one already-split, already-trimmed facet.
    ///
    /// Never fails: an unrecognized facet becomes [`Other`](Self::Other).
    /// Matching is exact and case-sensitive, as the SDK spells the constants —
    /// a plugin that writes `"instrument"` has not written `kInstrument`, and
    /// silently accepting it would let this table drift from the header.
    pub fn parse_facet(facet: &str) -> Self {
        match facet {
            "Fx" => Self::Fx,
            "Instrument" => Self::Instrument,
            "Generator" => Self::Generator,
            "Analyzer" => Self::Analyzer,
            "Spatial" => Self::Spatial,
            "Mastering" => Self::Mastering,
            "Synth" => Self::Synth,
            "Sampler" => Self::Sampler,
            "Drum" => Self::Drum,
            "Piano" => Self::Piano,
            "External" => Self::External,
            "Delay" => Self::Delay,
            "Reverb" => Self::Reverb,
            "Distortion" => Self::Distortion,
            "Dynamics" => Self::Dynamics,
            "EQ" => Self::Eq,
            "Filter" => Self::Filter,
            "Modulation" => Self::Modulation,
            "Pitch Shift" => Self::PitchShift,
            "Restoration" => Self::Restoration,
            "Tools" => Self::Tools,
            "Network" => Self::Network,
            "Channel Strip" => Self::ChannelStrip,
            "Drums" => Self::Drums,
            "Guitar" => Self::Guitar,
            "Vocals" => Self::Vocals,
            "Bass" => Self::Bass,
            "Microphone" => Self::Microphone,
            "Mono" => Self::Mono,
            "Stereo" => Self::Stereo,
            "Surround" => Self::Surround,
            "Ambisonics" => Self::Ambisonics,
            "Up-Downmix" => Self::UpDownmix,
            "OnlyRT" => Self::OnlyRt,
            "OnlyOfflineProcess" => Self::OnlyOfflineProcess,
            "NoOfflineProcess" => Self::NoOfflineProcess,
            "OnlyARA" => Self::OnlyAra,
            other => Self::Other(other.to_string()),
        }
    }

    /// The facet as the SDK spells it, so a parsed set can be written back.
    pub fn as_sdk_str(&self) -> &str {
        match self {
            Self::Fx => "Fx",
            Self::Instrument => "Instrument",
            Self::Generator => "Generator",
            Self::Analyzer => "Analyzer",
            Self::Spatial => "Spatial",
            Self::Mastering => "Mastering",
            Self::Synth => "Synth",
            Self::Sampler => "Sampler",
            Self::Drum => "Drum",
            Self::Piano => "Piano",
            Self::External => "External",
            Self::Delay => "Delay",
            Self::Reverb => "Reverb",
            Self::Distortion => "Distortion",
            Self::Dynamics => "Dynamics",
            Self::Eq => "EQ",
            Self::Filter => "Filter",
            Self::Modulation => "Modulation",
            Self::PitchShift => "Pitch Shift",
            Self::Restoration => "Restoration",
            Self::Tools => "Tools",
            Self::Network => "Network",
            Self::ChannelStrip => "Channel Strip",
            Self::Drums => "Drums",
            Self::Guitar => "Guitar",
            Self::Vocals => "Vocals",
            Self::Bass => "Bass",
            Self::Microphone => "Microphone",
            Self::Mono => "Mono",
            Self::Stereo => "Stereo",
            Self::Surround => "Surround",
            Self::Ambisonics => "Ambisonics",
            Self::UpDownmix => "Up-Downmix",
            Self::OnlyRt => "OnlyRT",
            Self::OnlyOfflineProcess => "OnlyOfflineProcess",
            Self::NoOfflineProcess => "NoOfflineProcess",
            Self::OnlyAra => "OnlyARA",
            Self::Other(s) => s,
        }
    }
}

/// A VST3 plugin's declared subcategories, parsed into facets.
///
/// Order is preserved because VST3's leading facet is the primary one:
/// `"Instrument|Synth"` leads with the role and qualifies it, and a consumer
/// picking "the main category" wants the first, not an arbitrary set member.
///
/// Parsing splits on `|` **only**. Three SDK facets contain a space —
/// `"Pitch Shift"`, `"Channel Strip"`, `"Up-Downmix"` — so treating whitespace
/// as a delimiter shreds them into unrecognized fragments.
///
/// An empty facet list means the plugin declared an empty string. "The factory
/// is too old to report subcategories at all" is spelled by an absent
/// `Vst3SubCategories`, not by an empty one — see the VST3 host's `ClassInfo`.
/// Serialized as the raw string alone: `facets` is derived from it by
/// [`parse`](Self::parse), so persisting both would let a hand-edited or
/// truncated file carry a facet list that disagrees with the string it came
/// from. Round-tripping through `String` makes that unrepresentable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(from = "String", into = "String"))]
pub struct Vst3SubCategories {
    facets: Vec<Vst3PlugType>,
    raw: String,
}

impl From<String> for Vst3SubCategories {
    fn from(raw: String) -> Self {
        Self::parse(&raw)
    }
}

impl From<Vst3SubCategories> for String {
    fn from(value: Vst3SubCategories) -> Self {
        value.raw
    }
}

impl Vst3SubCategories {
    /// Parse a `|`-delimited subcategory string.
    ///
    /// Empty segments are dropped rather than becoming empty
    /// [`Other`](Vst3PlugType::Other) facets: `"Fx||Delay"` is malformed, and
    /// the two facets it does declare are still the plugin's answer.
    pub fn parse(raw: &str) -> Self {
        let facets = raw
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Vst3PlugType::parse_facet)
            .collect();
        Self {
            facets,
            raw: raw.to_string(),
        }
    }

    /// The parsed facets, in the order the plugin declared them.
    pub fn facets(&self) -> &[Vst3PlugType] {
        &self.facets
    }

    /// The string exactly as the plugin declared it.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whether the plugin declared this facet.
    ///
    /// This is what a classifier should ask, rather than a substring test
    /// against [`raw`](Self::raw): `raw.contains("Instrument")` is also true of
    /// a vendor facet named `"DeInstrumenter"`.
    pub fn has(&self, facet: &Vst3PlugType) -> bool {
        self.facets.contains(facet)
    }

    /// `true` when the plugin declared no facet at all.
    pub fn is_empty(&self) -> bool {
        self.facets.is_empty()
    }
}

/// One CLAP plugin feature tag.
///
/// CLAP's taxonomy is natively a set — `clap_plugin_descriptor::features` is a
/// null-terminated array of strings, so a plugin declares
/// `["audio-effect", "mastering", "stereo"]` directly rather than composing one
/// string the way VST3 does. The 39 named variants mirror
/// `plugin_features.h`; the first four are the primary roles a host dispatches
/// on and the rest qualify them.
///
/// [`Other`](Self::Other) carries a tag the header does not name. The CLAP spec
/// states the list is a set of *standard* features and that plugins may declare
/// their own, so this arm is the format's design rather than a defensive guess.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ClapFeature {
    // Primary roles.
    Instrument,
    AudioEffect,
    NoteEffect,
    NoteDetector,
    Analyzer,
    // Instrument families.
    Synthesizer,
    Sampler,
    Drum,
    DrumMachine,
    // Effect families.
    Filter,
    Phaser,
    Equalizer,
    DeEsser,
    PhaseVocoder,
    Granular,
    FrequencyShifter,
    PitchShifter,
    Distortion,
    TransientShaper,
    Compressor,
    Expander,
    Gate,
    Limiter,
    Flanger,
    Chorus,
    Delay,
    Reverb,
    Tremolo,
    Glitch,
    Utility,
    PitchCorrection,
    Restoration,
    MultiEffects,
    Mixing,
    Mastering,
    // Channel hints.
    Mono,
    Stereo,
    Surround,
    Ambisonic,
    /// A tag the CLAP header does not name, carried verbatim.
    Other(String),
}

impl ClapFeature {
    /// Parse one feature tag. Never fails — an unrecognized tag becomes
    /// [`Other`](Self::Other).
    pub fn parse(tag: &str) -> Self {
        match tag {
            "instrument" => Self::Instrument,
            "audio-effect" => Self::AudioEffect,
            "note-effect" => Self::NoteEffect,
            "note-detector" => Self::NoteDetector,
            "analyzer" => Self::Analyzer,
            "synthesizer" => Self::Synthesizer,
            "sampler" => Self::Sampler,
            "drum" => Self::Drum,
            "drum-machine" => Self::DrumMachine,
            "filter" => Self::Filter,
            "phaser" => Self::Phaser,
            "equalizer" => Self::Equalizer,
            "de-esser" => Self::DeEsser,
            "phase-vocoder" => Self::PhaseVocoder,
            "granular" => Self::Granular,
            "frequency-shifter" => Self::FrequencyShifter,
            "pitch-shifter" => Self::PitchShifter,
            "distortion" => Self::Distortion,
            "transient-shaper" => Self::TransientShaper,
            "compressor" => Self::Compressor,
            "expander" => Self::Expander,
            "gate" => Self::Gate,
            "limiter" => Self::Limiter,
            "flanger" => Self::Flanger,
            "chorus" => Self::Chorus,
            "delay" => Self::Delay,
            "reverb" => Self::Reverb,
            "tremolo" => Self::Tremolo,
            "glitch" => Self::Glitch,
            "utility" => Self::Utility,
            "pitch-correction" => Self::PitchCorrection,
            "restoration" => Self::Restoration,
            "multi-effects" => Self::MultiEffects,
            "mixing" => Self::Mixing,
            "mastering" => Self::Mastering,
            "mono" => Self::Mono,
            "stereo" => Self::Stereo,
            "surround" => Self::Surround,
            "ambisonic" => Self::Ambisonic,
            other => Self::Other(other.to_string()),
        }
    }

    /// The tag as the CLAP header spells it.
    pub fn as_clap_str(&self) -> &str {
        match self {
            Self::Instrument => "instrument",
            Self::AudioEffect => "audio-effect",
            Self::NoteEffect => "note-effect",
            Self::NoteDetector => "note-detector",
            Self::Analyzer => "analyzer",
            Self::Synthesizer => "synthesizer",
            Self::Sampler => "sampler",
            Self::Drum => "drum",
            Self::DrumMachine => "drum-machine",
            Self::Filter => "filter",
            Self::Phaser => "phaser",
            Self::Equalizer => "equalizer",
            Self::DeEsser => "de-esser",
            Self::PhaseVocoder => "phase-vocoder",
            Self::Granular => "granular",
            Self::FrequencyShifter => "frequency-shifter",
            Self::PitchShifter => "pitch-shifter",
            Self::Distortion => "distortion",
            Self::TransientShaper => "transient-shaper",
            Self::Compressor => "compressor",
            Self::Expander => "expander",
            Self::Gate => "gate",
            Self::Limiter => "limiter",
            Self::Flanger => "flanger",
            Self::Chorus => "chorus",
            Self::Delay => "delay",
            Self::Reverb => "reverb",
            Self::Tremolo => "tremolo",
            Self::Glitch => "glitch",
            Self::Utility => "utility",
            Self::PitchCorrection => "pitch-correction",
            Self::Restoration => "restoration",
            Self::MultiEffects => "multi-effects",
            Self::Mixing => "mixing",
            Self::Mastering => "mastering",
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::Surround => "surround",
            Self::Ambisonic => "ambisonic",
            Self::Other(s) => s,
        }
    }
}

/// What a plugin *is*, normalized across the four formats.
///
/// The counterpart to [`Features`](crate::Features), which answers what a
/// plugin can *do*. The two are orthogonal and must stay so: an arpeggiator
/// takes MIDI and is not an instrument, and AU's `aumf` takes MIDI *and* audio.
/// Deriving one from the other misfiles both.
///
/// Each format's native taxonomy is kept verbatim in
/// [`PluginClass`](crate::PluginClass); this is the derived view a host
/// dispatches on, so the four-way match lives here once instead of at every
/// call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PluginRole {
    /// Note-driven sound source: takes MIDI, produces audio, no audio input.
    Instrument,
    /// Processes incoming audio.
    Effect,
    /// MIDI in, MIDI out, no audio role — an arpeggiator or transposer.
    NoteEffect,
    /// Reports rather than processes; not an insert.
    Analyzer,
    /// Produces audio without being note-driven — tone, noise, test signal.
    ///
    /// Distinct from [`Instrument`](Self::Instrument) because three of the four
    /// formats name it separately (VST2 `kPlugCategGenerator`, VST3
    /// `"Fx|Generator"`, AU `augn`). Folding it into `Instrument` discards a
    /// distinction the plugin bothered to declare.
    Generator,
    /// The format declared nothing, or nobody asked.
    #[default]
    Unknown,
}

impl Vst3SubCategories {
    /// The role these facets describe.
    ///
    /// `Instrument` requires the absence of `Fx`, because `"Fx|Instrument"` is
    /// SDK-defined as *"Fx which could be loaded as Instrument too"* — a plugin
    /// declaring both is an effect that a host *may* also offer as a generator,
    /// not a synth. A substring test for `"Instrument"` cannot see that
    /// difference and files every such effect as an instrument.
    pub fn role(&self) -> PluginRole {
        let has = |f| self.has(&f);
        if has(Vst3PlugType::Instrument) && !has(Vst3PlugType::Fx) {
            PluginRole::Instrument
        } else if has(Vst3PlugType::Analyzer) {
            PluginRole::Analyzer
        } else if has(Vst3PlugType::Generator) {
            PluginRole::Generator
        } else if has(Vst3PlugType::Fx) || has(Vst3PlugType::Spatial) {
            PluginRole::Effect
        } else {
            // Only channel hints, only vendor facets, or nothing at all.
            PluginRole::Unknown
        }
    }
}

impl Vst2Category {
    /// The role this category describes.
    ///
    /// `Shell` is a container advertising *other* plugins rather than a
    /// processor of its own, so it is `Unknown` rather than an effect;
    /// `OfflineProcess` likewise never appears as an insert.
    pub fn role(&self) -> PluginRole {
        match self {
            Self::Synth => PluginRole::Instrument,
            Self::Generator => PluginRole::Generator,
            Self::Analysis => PluginRole::Analyzer,
            Self::Effect
            | Self::Mastering
            | Self::Spacializer
            | Self::RoomFx
            | Self::SurroundFx
            | Self::Restoration => PluginRole::Effect,
            Self::Shell | Self::OfflineProcess | Self::Unrecognized(_) | Self::Unasked => {
                PluginRole::Unknown
            }
        }
    }
}

impl ClapFeature {
    /// The role this tag describes, or `None` if it only qualifies one.
    ///
    /// CLAP is the one format that names its primary roles as a closed set, so
    /// only those five answer. The family tags (`synthesizer`, `reverb`, …) and
    /// the channel hints qualify a role rather than assigning one — a plugin
    /// declaring `["audio-effect", "synthesizer"]` is an effect.
    pub fn role(&self) -> Option<PluginRole> {
        match self {
            Self::Instrument => Some(PluginRole::Instrument),
            Self::AudioEffect => Some(PluginRole::Effect),
            Self::NoteEffect | Self::NoteDetector => Some(PluginRole::NoteEffect),
            Self::Analyzer => Some(PluginRole::Analyzer),
            _ => None,
        }
    }
}

/// The role a CLAP feature list describes.
///
/// The tags are a set with no declared precedence, so a plugin may name more
/// than one primary role. First-wins over the plugin's own ordering: that is
/// the only ranking the format supplies, and inventing one here would override
/// what the plugin chose to put first.
pub fn clap_features_role(features: &[ClapFeature]) -> PluginRole {
    features
        .iter()
        .find_map(ClapFeature::role)
        .unwrap_or(PluginRole::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Nobody asked" and "the plugin said it has no category" are different
    /// answers, and both used to be a single bare `Unknown`.
    #[test]
    fn an_unasked_category_is_not_an_unrecognized_one() {
        assert_ne!(Vst2Category::Unasked, Vst2Category::Unrecognized(0));
        assert!(!Vst2Category::Unasked.was_answered());
        assert!(Vst2Category::Unrecognized(0).was_answered());
        assert!(Vst2Category::Effect.was_answered());
    }

    /// The raw code survives, so a caller can see what the plugin actually
    /// returned instead of a flattened `Unknown`. Two unnamed codes stay
    /// distinct from each other.
    #[test]
    fn an_unrecognized_category_keeps_its_code() {
        assert_ne!(
            Vst2Category::Unrecognized(42),
            Vst2Category::Unrecognized(99)
        );
        match Vst2Category::Unrecognized(42) {
            Vst2Category::Unrecognized(code) => assert_eq!(code, 42),
            other => panic!("expected Unrecognized, got {other:?}"),
        }
    }

    /// A default-constructed category claims nothing. `Unasked` is the default
    /// precisely because a descriptor built by struct-update has not queried.
    #[test]
    fn the_default_category_claims_nothing() {
        assert_eq!(Vst2Category::default(), Vst2Category::Unasked);
        assert!(!Vst2Category::default().was_answered());
    }

    /// Both new shapes survive the bincode wire, and an `Unrecognized` payload
    /// arrives with its code intact — a decode that dropped it would restore
    /// the flattening this variant exists to prevent.
    #[cfg(feature = "serde")]
    #[test]
    fn the_category_survives_the_bincode_round_trip() {
        for want in [
            Vst2Category::Unasked,
            Vst2Category::Unrecognized(0),
            Vst2Category::Unrecognized(-7),
            Vst2Category::Synth,
        ] {
            let bytes = bincode::serialize(&want).expect("serialize");
            let back: Vst2Category = bincode::deserialize(&bytes).expect("deserialize");
            assert_eq!(back, want);
        }
    }

    /// A subcategory string is a *set*, not one value. The SDK composes its
    /// constants out of facets, so the parse has to yield all of them.
    #[test]
    fn a_composed_subcategory_parses_into_every_facet_it_names() {
        let c = Vst3SubCategories::parse("Instrument|Synth|Sampler");
        assert_eq!(
            c.facets(),
            [
                Vst3PlugType::Instrument,
                Vst3PlugType::Synth,
                Vst3PlugType::Sampler
            ]
        );
        assert!(c.has(&Vst3PlugType::Sampler));
    }

    /// Three SDK facets contain a space. Splitting on whitespace as well as `|`
    /// would shred each into unrecognized fragments, and the plugin's declared
    /// category would silently become two `Other`s.
    #[test]
    fn a_facet_containing_a_space_survives_the_split() {
        for (raw, want) in [
            ("Fx|Pitch Shift", Vst3PlugType::PitchShift),
            ("Fx|Channel Strip", Vst3PlugType::ChannelStrip),
            ("Fx|Up-Downmix", Vst3PlugType::UpDownmix),
        ] {
            let c = Vst3SubCategories::parse(raw);
            assert!(c.has(&want), "{raw} lost its multi-word facet");
            assert_eq!(c.facets().len(), 2, "{raw} split into the wrong count");
        }
    }

    /// The first facet is the primary one, so parsing must not reorder.
    #[test]
    fn the_declared_order_is_preserved() {
        let c = Vst3SubCategories::parse("Fx|Analyzer");
        assert_eq!(c.facets()[0], Vst3PlugType::Fx);

        let c = Vst3SubCategories::parse("Analyzer|Fx");
        assert_eq!(c.facets()[0], Vst3PlugType::Analyzer);
    }

    /// A vendor facet is carried, not dropped. Discarding it would report the
    /// plugin as having declared nothing there.
    #[test]
    fn an_unrecognized_facet_is_carried_verbatim() {
        let c = Vst3SubCategories::parse("Fx|AcmeSpecial");
        assert_eq!(
            c.facets(),
            [
                Vst3PlugType::Fx,
                Vst3PlugType::Other("AcmeSpecial".to_string())
            ]
        );
        assert_eq!(c.raw(), "Fx|AcmeSpecial");
    }

    /// Facet membership is not a substring test. `raw.contains("Instrument")`
    /// — what the browser used to do — is also true of this vendor facet.
    #[test]
    fn a_facet_test_does_not_match_a_longer_name_containing_it() {
        let c = Vst3SubCategories::parse("Fx|DeInstrumenter");
        assert!(!c.has(&Vst3PlugType::Instrument));
        assert!(
            c.raw().contains("Instrument"),
            "this fixture is only meaningful while the raw string does match"
        );
    }

    /// `Drum` and `Drums` are both real: `kInstrumentDrum` is "Instrument|Drum"
    /// and `kFxDrums` is "Fx|Drums". Folding them together stops round-tripping.
    #[test]
    fn the_singular_and_plural_drum_facets_stay_distinct() {
        assert!(Vst3SubCategories::parse("Instrument|Drum").has(&Vst3PlugType::Drum));
        assert!(Vst3SubCategories::parse("Fx|Drums").has(&Vst3PlugType::Drums));
        assert!(!Vst3SubCategories::parse("Fx|Drums").has(&Vst3PlugType::Drum));
    }

    /// A malformed string still yields the facets it does declare.
    #[test]
    fn an_empty_segment_is_dropped_rather_than_parsed() {
        let c = Vst3SubCategories::parse("Fx||Delay");
        assert_eq!(c.facets(), [Vst3PlugType::Fx, Vst3PlugType::Delay]);
    }

    /// An empty declaration is an empty facet list, not a phantom facet. "The
    /// factory cannot report subcategories" is a *missing* `Vst3SubCategories`.
    #[test]
    fn an_empty_declaration_yields_no_facets() {
        assert!(Vst3SubCategories::parse("").is_empty());
        assert!(Vst3SubCategories::parse("").facets().is_empty());
    }

    /// Every facet round-trips through the SDK spelling, so a parsed set can be
    /// written back out. Catches a variant added to one match arm but not both.
    #[test]
    fn every_facet_round_trips_through_its_sdk_spelling() {
        let all = [
            Vst3PlugType::Fx,
            Vst3PlugType::Instrument,
            Vst3PlugType::Generator,
            Vst3PlugType::Analyzer,
            Vst3PlugType::Spatial,
            Vst3PlugType::Mastering,
            Vst3PlugType::Synth,
            Vst3PlugType::Sampler,
            Vst3PlugType::Drum,
            Vst3PlugType::Piano,
            Vst3PlugType::External,
            Vst3PlugType::Delay,
            Vst3PlugType::Reverb,
            Vst3PlugType::Distortion,
            Vst3PlugType::Dynamics,
            Vst3PlugType::Eq,
            Vst3PlugType::Filter,
            Vst3PlugType::Modulation,
            Vst3PlugType::PitchShift,
            Vst3PlugType::Restoration,
            Vst3PlugType::Tools,
            Vst3PlugType::Network,
            Vst3PlugType::ChannelStrip,
            Vst3PlugType::Drums,
            Vst3PlugType::Guitar,
            Vst3PlugType::Vocals,
            Vst3PlugType::Bass,
            Vst3PlugType::Microphone,
            Vst3PlugType::Mono,
            Vst3PlugType::Stereo,
            Vst3PlugType::Surround,
            Vst3PlugType::Ambisonics,
            Vst3PlugType::UpDownmix,
            Vst3PlugType::OnlyRt,
            Vst3PlugType::OnlyOfflineProcess,
            Vst3PlugType::NoOfflineProcess,
            Vst3PlugType::OnlyAra,
        ];
        assert_eq!(all.len(), 37, "the SDK declares 37 distinct facets");
        for facet in all {
            let spelled = facet.as_sdk_str();
            assert_eq!(
                Vst3PlugType::parse_facet(spelled),
                facet,
                "{spelled} did not round-trip"
            );
        }
    }

    /// CLAP's tags are hyphenated, and each is one whole tag rather than a
    /// composition — `"drum-machine"` is not `drum` plus `machine`.
    #[test]
    fn a_hyphenated_clap_tag_is_one_feature() {
        assert_eq!(ClapFeature::parse("drum-machine"), ClapFeature::DrumMachine);
        assert_eq!(ClapFeature::parse("drum"), ClapFeature::Drum);
        assert_ne!(
            ClapFeature::parse("drum-machine"),
            ClapFeature::parse("drum")
        );
    }

    /// A vendor tag is carried rather than dropped — the CLAP spec permits
    /// plugins to declare features outside the standard list.
    #[test]
    fn an_unrecognized_clap_tag_is_carried_verbatim() {
        assert_eq!(
            ClapFeature::parse("acme:special"),
            ClapFeature::Other("acme:special".to_string())
        );
    }

    /// Matching is exact: a plugin writing `"Instrument"` has not written the
    /// CLAP tag `"instrument"`, and accepting it would let this table drift.
    #[test]
    fn clap_tag_matching_is_case_sensitive() {
        assert_eq!(
            ClapFeature::parse("Instrument"),
            ClapFeature::Other("Instrument".to_string())
        );
    }

    /// Every tag round-trips, so a parsed set can be written back. Catches a
    /// variant added to one match arm but not the other.
    #[test]
    fn every_clap_feature_round_trips_through_its_header_spelling() {
        let all = [
            ClapFeature::Instrument,
            ClapFeature::AudioEffect,
            ClapFeature::NoteEffect,
            ClapFeature::NoteDetector,
            ClapFeature::Analyzer,
            ClapFeature::Synthesizer,
            ClapFeature::Sampler,
            ClapFeature::Drum,
            ClapFeature::DrumMachine,
            ClapFeature::Filter,
            ClapFeature::Phaser,
            ClapFeature::Equalizer,
            ClapFeature::DeEsser,
            ClapFeature::PhaseVocoder,
            ClapFeature::Granular,
            ClapFeature::FrequencyShifter,
            ClapFeature::PitchShifter,
            ClapFeature::Distortion,
            ClapFeature::TransientShaper,
            ClapFeature::Compressor,
            ClapFeature::Expander,
            ClapFeature::Gate,
            ClapFeature::Limiter,
            ClapFeature::Flanger,
            ClapFeature::Chorus,
            ClapFeature::Delay,
            ClapFeature::Reverb,
            ClapFeature::Tremolo,
            ClapFeature::Glitch,
            ClapFeature::Utility,
            ClapFeature::PitchCorrection,
            ClapFeature::Restoration,
            ClapFeature::MultiEffects,
            ClapFeature::Mixing,
            ClapFeature::Mastering,
            ClapFeature::Mono,
            ClapFeature::Stereo,
            ClapFeature::Surround,
            ClapFeature::Ambisonic,
        ];
        assert_eq!(all.len(), 39, "the CLAP header declares 39 features");
        for feature in all {
            let spelled = feature.as_clap_str();
            assert_eq!(
                ClapFeature::parse(spelled),
                feature,
                "{spelled} did not round-trip"
            );
        }
    }

    /// `"Fx|Instrument"` is an effect, not an instrument.
    ///
    /// The SDK defines it as "Fx which could be loaded as Instrument too", so
    /// `Fx` is the plugin's primary claim. The substring test this replaces
    /// (`raw.contains("Instrument")`) files every such effect as a synth — the
    /// bug that motivated the facet types.
    #[test]
    fn an_fx_that_can_also_load_as_an_instrument_is_an_effect() {
        assert_eq!(
            Vst3SubCategories::parse("Fx|Instrument").role(),
            PluginRole::Effect
        );
        assert_eq!(
            Vst3SubCategories::parse("Fx|Instrument|External").role(),
            PluginRole::Effect
        );
        // The plain instrument spellings are unaffected.
        assert_eq!(
            Vst3SubCategories::parse("Instrument|Synth").role(),
            PluginRole::Instrument
        );
        assert_eq!(
            Vst3SubCategories::parse("Instrument").role(),
            PluginRole::Instrument
        );
    }

    /// Facets that only describe channel support say nothing about the role.
    ///
    /// A plugin declaring `"Stereo"` and nothing else has not classified
    /// itself, and answering `Effect` would invent a claim it never made.
    #[test]
    fn channel_hints_alone_do_not_name_a_role() {
        assert_eq!(
            Vst3SubCategories::parse("Stereo").role(),
            PluginRole::Unknown
        );
        assert_eq!(Vst3SubCategories::parse("").role(), PluginRole::Unknown);
        // But they do not suppress a real facet sitting beside them.
        assert_eq!(
            Vst3SubCategories::parse("Fx|Reverb|Stereo").role(),
            PluginRole::Effect
        );
    }

    /// A VST3 generator is not an instrument: `"Fx|Generator"` is a tone/noise
    /// source, not something a keyboard plays.
    #[test]
    fn a_vst3_generator_is_its_own_role() {
        assert_eq!(
            Vst3SubCategories::parse("Fx|Generator").role(),
            PluginRole::Generator
        );
        assert_eq!(
            Vst3SubCategories::parse("Analyzer").role(),
            PluginRole::Analyzer
        );
    }

    /// CLAP's family tags qualify a role; they never assign one.
    ///
    /// `["audio-effect", "synthesizer"]` is an effect — a vocoder may well
    /// declare both, and reading `synthesizer` as primary would misfile it.
    #[test]
    fn a_clap_family_tag_does_not_override_the_primary_role() {
        let features = [ClapFeature::AudioEffect, ClapFeature::Synthesizer];
        assert_eq!(clap_features_role(&features), PluginRole::Effect);

        // A family tag with no primary role beside it names nothing.
        assert_eq!(
            clap_features_role(&[ClapFeature::Synthesizer]),
            PluginRole::Unknown
        );
        assert_eq!(clap_features_role(&[]), PluginRole::Unknown);
    }

    /// Each CLAP primary role maps to its own `PluginRole`.
    #[test]
    fn every_clap_primary_role_is_recognized() {
        assert_eq!(
            clap_features_role(&[ClapFeature::Instrument]),
            PluginRole::Instrument
        );
        assert_eq!(
            clap_features_role(&[ClapFeature::NoteEffect]),
            PluginRole::NoteEffect
        );
        assert_eq!(
            clap_features_role(&[ClapFeature::NoteDetector]),
            PluginRole::NoteEffect
        );
        assert_eq!(
            clap_features_role(&[ClapFeature::Analyzer]),
            PluginRole::Analyzer
        );
    }

    /// A VST2 shell hosts *other* plugins, so it is neither an instrument nor
    /// an effect — and neither is a category nobody asked for.
    #[test]
    fn a_vst2_shell_and_an_unasked_category_both_decline_to_classify() {
        assert_eq!(Vst2Category::Shell.role(), PluginRole::Unknown);
        assert_eq!(Vst2Category::OfflineProcess.role(), PluginRole::Unknown);
        assert_eq!(Vst2Category::Unasked.role(), PluginRole::Unknown);
        assert_eq!(Vst2Category::Unrecognized(0).role(), PluginRole::Unknown);
        // The ones that do classify.
        assert_eq!(Vst2Category::Synth.role(), PluginRole::Instrument);
        assert_eq!(Vst2Category::Generator.role(), PluginRole::Generator);
        assert_eq!(Vst2Category::Analysis.role(), PluginRole::Analyzer);
        assert_eq!(Vst2Category::RoomFx.role(), PluginRole::Effect);
    }

    /// Serializing keeps only the raw string, so the facet list cannot drift
    /// from the string it was parsed out of.
    #[cfg(feature = "serde")]
    #[test]
    fn subcategories_round_trip_through_their_raw_string() {
        let original = Vst3SubCategories::parse("Fx|Reverb");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"Fx|Reverb\"", "should serialize as the bare string");

        let back: Vst3SubCategories = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
        assert_eq!(back.facets().len(), 2, "facets are rebuilt on the way in");
    }
}

//! Parameter discovery, read, and write APIs for Audio Units.
//!
//! Parameters are addressed by a `(scope, element, id)` triple, carried here as
//! [`ParamAddress`]. The bare `list` / `get` / `set` / `info` functions are the
//! ergonomic default and address `kAudioUnitScope_Global` / element `0`, which
//! is where every effect and instrument keeps its parameters; the `_at`
//! variants take an explicit address.
//!
//! Per-element parameters are not hypothetical surface. Measured on macOS 15.6:
//! AUMultiChannelMixer publishes 7 parameters on **each** of its 8 input
//! elements plus 5 on its output element, and AUMixer3D publishes 11 per input
//! element across 64 of them. Writes to the same id on different elements were
//! verified independent — setting input element 0's volume to 0.25 and element
//! 1's to 0.75 reads back 0.25 and 0.75 respectively. Addressing every one of
//! those through element 0, as this module used to, collapsed a whole mixer's
//! per-channel strip onto a single control.
//!
//! LIMITATION (intentional): [`ParamView`] and [`crate::instance::AuInstance`]'s
//! parameter methods stay global/element-0. They are the DAW-facing surface, and
//! the DAW hosts effects and instruments, none of which put parameters anywhere
//! else. A mixer-hosting caller reaches for the `_at` functions directly. That
//! applies to the display-conversion functions below too — [`value_strings`],
//! [`string_from_value`], [`value_from_string`] and [`clump_name`] each have an
//! `_at` variant, and the un-suffixed form addresses `ParamAddress::GLOBAL`.
//!
//! ## Two properties that are keyed by parameter id, not element
//!
//! `ParameterInfo` and `ParameterValueStrings` both take the **parameter id in
//! the element position** — see [`info_at`]. `ParameterStringFromValue`,
//! `ParameterValueFromString` and `ParameterClumpName` do NOT: they carry the id
//! inside their request struct and use the element position normally. Mixing the
//! two conventions up reads metadata for whatever parameter happens to share that
//! number, which is why the distinction is spelled out at each call site.
//!
//! ## Flags under-report; the property read is the authority
//!
//! `kAudioUnitParameterFlag_ValuesHaveStrings` is NOT a reliable gate for
//! [`value_strings`]. Measured on macOS 15.6, AUNBandEQ's "Type" parameter
//! returns 11 filter names with that flag clear. [`AuParameter::values_have_strings`]
//! is carried for reporting only — always attempt the read.
//!
//! ## Measured absence of the string conversions
//!
//! **No Apple AU on macOS 15.6 implements `ParameterStringFromValue` or
//! `ParameterValueFromString`** — probed across 15 units × every parameter ×
//! several candidate values/strings, every call failed. Both are implemented here
//! because third-party AUs use them, but the round-trip cannot be demonstrated
//! against the system corpus, and the tests assert the honest `None` rather than a
//! fabricated success. Do not "fix" those tests by relaxing them into a tautology;
//! the absence is the measurement.

#![cfg(target_os = "macos")]
// AudioUnit is an opaque C pointer (`ComponentInstanceRecord*`) that every
// AudioToolbox call dereferences. Callers must supply a valid unit, same as
// every other entry point in this crate.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::marker::PhantomData;

use crate::error::Result;
use crate::ffi::{check, get_property, get_property_bytes};
use crate::types::*;

/// Where a parameter lives: the `(scope, element)` pair AudioToolbox addresses
/// it by.
///
/// **Not** `tutti_plugin_types::ParamAddress`, which shares the name and answers
/// a different question: that one says whether a parameter is named by an opaque
/// handle or a positional index, across the four hosted formats. This one names
/// the AU-specific *container* a parameter sits in, and both halves are always
/// `u32`. They never appear together — this type is `parameters::ParamAddress`
/// and is not re-exported at the crate root — but the coincidence is worth
/// stating so neither is mistaken for the other.
///
/// Not a bare `(u32, u32)`, because the two are the same type and adjacent in
/// every AudioToolbox signature — `AudioUnitGetParameter(unit, id, scope,
/// element, …)` will happily accept them transposed and return the value of a
/// *real* parameter on the wrong scope. Naming the pair means a call site has
/// to say which is which once, at construction, instead of at every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamAddress {
    /// Raw `kAudioUnitScope_*` constant.
    pub scope: u32,
    /// Element (bus / part) index within that scope.
    pub element: u32,
}

impl ParamAddress {
    /// The global scope, element 0 — where effects and instruments keep every
    /// parameter, and what the un-suffixed functions in this module use.
    pub const GLOBAL: Self = Self {
        scope: K_AUDIO_UNIT_SCOPE_GLOBAL,
        element: 0,
    };

    /// Parameters attached to bus `bus` on `direction`.
    ///
    /// This is the mixer shape: one strip of parameters per input element.
    pub fn on_bus(direction: crate::bus::BusDirection, bus: u32) -> Self {
        Self {
            scope: direction.scope(),
            element: bus,
        }
    }
}

impl Default for ParamAddress {
    fn default() -> Self {
        Self::GLOBAL
    }
}

/// Inclusive `[min, max]` range plus the AU-reported default value.
#[derive(Debug, Clone, Copy)]
pub struct ParamRange {
    /// Minimum legal value.
    pub min: f32,
    /// Maximum legal value.
    pub max: f32,
    /// AU-reported default value.
    pub default: f32,
}

impl ParamRange {
    /// Midpoint of the range. Useful as a neutral test value.
    pub fn mid(&self) -> f32 {
        (self.min + self.max) * 0.5
    }

    /// Clamp `v` into `[min, max]`.
    pub fn clamp(&self, v: f32) -> f32 {
        v.clamp(self.min, self.max)
    }
}

/// Fully-described AU parameter: id, display name, range, and unit kind.
#[derive(Debug, Clone)]
pub struct AuParameter {
    /// AudioUnit parameter id used with `AudioUnitSetParameter` etc.
    pub id: u32,
    /// Human-readable name for UI display.
    pub name: String,
    /// Legal value range and default.
    pub range: ParamRange,
    /// Unit classification (dB, Hz, %, …) for display formatting.
    pub unit: ParameterUnit,
    /// Whether the AU advertises the `IsWritable` flag for this parameter.
    /// A parameter that is readable but not writable maps to `read_only`.
    pub writable: bool,
    /// The taper a UI should draw this parameter's control on.
    ///
    /// Ignoring this draws a log-taper knob as linear, which is not a cosmetic
    /// difference: on AULowpass's cutoff (10 Hz … 22.05 kHz, logarithmic) the
    /// linear midpoint lands at 11 kHz where the musical midpoint is ~470 Hz, so
    /// nearly the whole useful range is crushed into the first few percent of
    /// travel. 39 parameters across the corpus carry a non-linear curve.
    pub display: DisplayCurve,
    /// Whether this parameter is a **meter reading**, not a control
    /// (`kAudioUnitParameterFlag_MeterReadOnly`).
    ///
    /// A host must keep these out of its automation menu. Measured on macOS 15.6:
    /// AUMultibandCompressor publishes 12 of them ("Comp Amount 1-4", "Input
    /// Amplitude 1-4", "Output Amplitude 1-4") and AUSampler 2 ("Output Amp
    /// 0/1"), all of which would otherwise appear as automatable targets that
    /// silently discard writes.
    pub meter_read_only: bool,
    /// Group this parameter belongs to, if the AU advertises
    /// `kAudioUnitParameterFlag_HasClump`.
    ///
    /// `None` means ungrouped. Resolve the id to a label with
    /// [`clump_name`] — a 400-parameter synth is otherwise one flat list.
    /// Measured on macOS 15.6: AUDistortion groups its 22 parameters into 7
    /// clumps ("Delay", "Ring Modulation", "Decimation", …).
    pub clump: Option<u32>,
    /// Whether the AU sets `kAudioUnitParameterFlag_ValuesHaveStrings`.
    ///
    /// **Do not gate a [`value_strings`] call on this.** Measured on macOS 15.6,
    /// AUNBandEQ's "Type" parameter returns 11 value strings while leaving this
    /// flag *clear* (flags `0xd8100000`); gating on it would show the user
    /// "0.000/1.000/2.000" for a filter-type menu. It is carried for reporting
    /// only — the property read is the authority.
    pub values_have_strings: bool,
    /// Whether the AU can ramp this parameter over a render block
    /// (`kAudioUnitParameterFlag_CanRamp`) — i.e. whether a scheduled
    /// automation ramp is honoured or applied as a step at the block boundary.
    pub can_ramp: bool,
    /// Whether the AU asks that this parameter be left out of saved presets
    /// (`kAudioUnitParameterFlag_OmitFromPresets`).
    pub omit_from_presets: bool,
    /// Whether writing this parameter may silently move *others*.
    ///
    /// `None` when neither meta flag is set, which is the overwhelming majority.
    /// A host that caches parameter values must re-read after writing one of
    /// these, because the AU sends no notification for the parameters it moved
    /// as a side effect. [`dependents_of`] names which ones, when the AU says.
    ///
    /// Measured on macOS 15.6: 28 meta-flagged parameters across the 39
    /// instantiable registered units — AUNBandEQ's 8 per-band "Type" controls,
    /// AUGraphicEQ's "Number of Bands", AUPitch's 9, AURoundTripAAC's 3,
    /// AURogerBeep's "Sensitivity", AUNewPitch's "Spectral Coherence", and TDR
    /// Nova's 5. So the flag is *not* rare surface even though
    /// [`dependents_of`] is unanswered by every one of them.
    pub meta: Option<MetaScope>,
}

/// How wide a meta-parameter's influence reaches.
///
/// A **closed** enum for the same reason [`DisplayCurve`] is: these are two
/// specific bits in `AudioUnitParameterInfo::flags` naming a fixed structural
/// role, not an extensible catalog. Apple defines exactly these two, their
/// difference is a real difference in what a host must invalidate, and absence
/// of both is spelled by `Option::None` on [`AuParameter::meta`] rather than by a
/// third variant — "not a meta-parameter" is not a kind of meta-parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaScope {
    /// `kAudioUnitParameterFlag_IsGlobalMeta` — "changing this parameter may
    /// change any number of others in the AudioUnit". Dependents may be on any
    /// scope, and a non-global dependent is dependent in *every* element of its
    /// scope, so a host must invalidate that parameter across all elements.
    Global,
    /// `kAudioUnitParameterFlag_IsElementMeta` — "changing this parameter may
    /// change others in the same element". Dependents share one scope and apply
    /// only within this element, so invalidation stays local.
    Element,
}

impl MetaScope {
    /// Classify the two meta bits of an `AudioUnitParameterInfo::flags` word.
    ///
    /// `None` when neither is set. When an AU sets **both** — which Apple's
    /// header neither blesses nor forbids, and no unit on this machine does —
    /// the answer is [`Global`](Self::Global), because that is the wider claim
    /// and the two differ only in how much a host must invalidate. Picking the
    /// narrower one on an ambiguous answer would under-invalidate, leaving a
    /// stale cached value on another element with nothing to reveal it; picking
    /// the wider one costs a re-read.
    pub fn from_flags(flags: u32) -> Option<Self> {
        if flags & K_AUDIO_UNIT_PARAMETER_FLAG_IS_GLOBAL_META != 0 {
            Some(Self::Global)
        } else if flags & K_AUDIO_UNIT_PARAMETER_FLAG_IS_ELEMENT_META != 0 {
            Some(Self::Element)
        } else {
            None
        }
    }
}

/// One parameter a meta-parameter may move: the `(scope, id)` pair Apple's
/// `AUDependentParameter` carries.
///
/// Not a bare `(u32, u32)`, for the reason [`ParamAddress`] is not: the two
/// halves are the same type and adjacent in memory, so a transposition is
/// invisible. Note this pairs a scope with a parameter **id**, not with an
/// element — so it is deliberately *not* a [`ParamAddress`], which pairs a scope
/// with an element. Which element the dependent applies to is answered by the
/// meta-parameter's [`MetaScope`], not carried in the struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DependentParam {
    /// Raw `kAudioUnitScope_*` the dependent parameter lives on.
    pub scope: u32,
    /// The dependent parameter's id within that scope.
    pub id: u32,
}

/// The taper a parameter's value should be displayed and edited on.
///
/// A **closed** enum, deliberately, unlike [`ParameterUnit`]'s open
/// `Unknown(u32)`. The display field is a fixed 4-bit structural role in
/// `AudioUnitParameterInfo::flags`, not an extensible catalog: Apple's header
/// enumerates exactly these six curves plus "unset", the mask
/// (`kAudioUnitParameterFlag_DisplayMask`) bounds what can appear, and a host has
/// to pick one of a finite set of tapers to draw. An `Unknown` arm here would be
/// a value no UI could act on — so an unrecognized bit pattern maps to
/// [`Self::Linear`], which is the safe default the flags mean by their absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayCurve {
    /// No display flag set: a plain linear control.
    #[default]
    Linear,
    /// `DisplayLogarithmic` — frequency and time controls, overwhelmingly.
    Logarithmic,
    /// `DisplaySquareRoot`.
    SquareRoot,
    /// `DisplaySquared`.
    Squared,
    /// `DisplayCubed`.
    Cubed,
    /// `DisplayCubeRoot`.
    CubeRoot,
    /// `DisplayExponential`.
    Exponential,
}

impl DisplayCurve {
    /// Extract the curve from a raw `AudioUnitParameterInfo::flags` word.
    ///
    /// Masks with Apple's own `kAudioUnitParameterFlag_DisplayMask`, which spans
    /// bits 16..=18 **and** bit 22 — the field is not contiguous. Masking with
    /// `7 << 16` alone compiles, looks right, and silently drops every
    /// logarithmic parameter, which is the majority of them (24 of the 39
    /// curve-carrying parameters measured on macOS 15.6).
    pub fn from_flags(flags: u32) -> Self {
        match flags & K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK {
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_LOGARITHMIC => Self::Logarithmic,
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARE_ROOT => Self::SquareRoot,
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARED => Self::Squared,
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBED => Self::Cubed,
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBE_ROOT => Self::CubeRoot,
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_EXPONENTIAL => Self::Exponential,
            // Includes 0 (no flag set) and any bit pattern Apple has not
            // defined. See the type docs for why this is not an `Unknown` arm.
            _ => Self::Linear,
        }
    }
}

/// Classification of a parameter's physical unit.
///
/// Hosts use this to choose a display formatter (e.g. append `"Hz"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterUnit {
    /// Dimensionless / unclassified.
    Generic,
    /// A discrete choice list: the value is an index into named options, so
    /// `[min, max]` are the first and last index. Common in real units —
    /// AUTimePitch's "Overlap" is `0..10`, TDR Nova has 16 such parameters.
    Indexed,
    /// Treated as a 0/1 toggle.
    Boolean,
    /// 0..=100 percentage.
    Percent,
    /// Duration in seconds.
    Seconds,
    /// Frequency in hertz.
    Hertz,
    /// Amplitude in decibels.
    Decibels,
    /// Linear amplitude gain.
    LinearGain,
    /// The value **is** a MIDI controller number, `0..=127`
    /// (`kAudioUnitParameterUnit_MIDIController` = 12) — not a quantity in a
    /// physical unit.
    ///
    /// Its own arm rather than `Unknown(12)` because the two format differently
    /// and a host cannot tell them apart otherwise: a value of `74` under this
    /// unit is "CC 74", the brightness controller, and rendering it as a bare
    /// number loses the only thing that makes it readable. It is also the unit a
    /// parameter carries when it selects *which* controller drives something —
    /// the parameter side of [`crate::midi_map`].
    ///
    /// Measured on macOS 15.6: no installed AU reports it, so nothing on this
    /// machine exercises it end-to-end. It is decoded anyway because the cost is
    /// one arm and the failure mode without it is silent mis-formatting.
    MidiController,
    /// An AU-specific unit code this crate doesn't recognize.
    Unknown(u32),
}

impl ParameterUnit {
    /// Map a raw `kAudioUnitParameterUnit_*` code onto the typed variant.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC => Self::Generic,
            K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED => Self::Indexed,
            K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN => Self::Boolean,
            K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT => Self::Percent,
            K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS => Self::Seconds,
            K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ => Self::Hertz,
            K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS => Self::Decibels,
            K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN => Self::LinearGain,
            K_AUDIO_UNIT_PARAMETER_UNIT_MIDI_CONTROLLER => Self::MidiController,
            other => Self::Unknown(other),
        }
    }
}

impl std::fmt::Display for ParameterUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generic => write!(f, ""),
            Self::Indexed => write!(f, "index"),
            Self::Boolean => write!(f, "bool"),
            Self::Percent => write!(f, "%"),
            Self::Seconds => write!(f, "s"),
            Self::Hertz => write!(f, "Hz"),
            Self::Decibels => write!(f, "dB"),
            Self::LinearGain => write!(f, "gain"),
            // "CC" rather than a suffix: this unit prefixes its value, because
            // "74 CC" is meaningless where "CC 74" names a controller.
            Self::MidiController => write!(f, "CC"),
            Self::Unknown(v) => write!(f, "unit({v})"),
        }
    }
}

/// Lifetime-bounded view over the parameter state of an AU.
///
/// Borrowed from [`crate::instance::AuInstance::parameters`]; the view is
/// tied to the instance's lifetime so the raw `AudioUnit` pointer can't
/// outlive its owner.
pub struct ParamView<'a> {
    unit: AudioUnit,
    _lt: PhantomData<&'a ()>,
}

impl<'a> ParamView<'a> {
    /// Build a view wrapping a raw `AudioUnit`.
    ///
    /// # Safety
    /// The caller must guarantee that `unit` remains valid for the lifetime
    /// `'a`. Crate-internal constructors derive `'a` from an owning handle.
    pub(crate) unsafe fn new(unit: AudioUnit) -> Self {
        Self {
            unit,
            _lt: PhantomData,
        }
    }

    /// Enumerate all parameters exposed by this AU.
    pub fn list(&self) -> Vec<AuParameter> {
        list(self.unit)
    }

    /// Read the current value of parameter `id`.
    pub fn get(&self, id: u32) -> Result<f32> {
        get(self.unit, id)
    }

    /// Write a new value to parameter `id`.
    pub fn set(&self, id: u32, value: f32) -> Result<()> {
        set(self.unit, id, value)
    }

    /// Ordered display strings for an indexed parameter. See [`value_strings`].
    pub fn value_strings(&self, id: u32) -> Vec<String> {
        value_strings(self.unit, id)
    }

    /// The AU's display string for `value`. See [`string_from_value`].
    pub fn string_from_value(&self, id: u32, value: f32) -> Option<String> {
        string_from_value(self.unit, id, value)
    }

    /// Parse `text` with the AU's own interpretation. See [`value_from_string`].
    pub fn value_from_string(&self, id: u32, text: &str) -> Option<f32> {
        value_from_string(self.unit, id, text)
    }

    /// The AU's label for a parameter clump. See [`clump_name`].
    pub fn clump_name(&self, clump: u32) -> Option<String> {
        clump_name(self.unit, clump)
    }
}

/// Enumerate all parameters on the given raw `AudioUnit`, from the global
/// scope / element 0.
///
/// Returns an empty vec if the AU doesn't advertise a parameter list.
pub fn list(unit: AudioUnit) -> Vec<AuParameter> {
    list_at(unit, ParamAddress::GLOBAL)
}

/// Enumerate the parameters at `addr`.
///
/// Returns an empty vec if the AU advertises no parameter list there. That is
/// the common answer for a scope/element an AU does not use, and it is not an
/// error — asking a plain effect for its input-element parameters legitimately
/// yields nothing.
pub fn list_at(unit: AudioUnit, addr: ParamAddress) -> Vec<AuParameter> {
    let ids_bytes = match unsafe {
        get_property_bytes(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
            addr.scope,
            addr.element,
        )
    } {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    // Decode by chunk rather than reinterpreting the whole buffer as a `[u32]`:
    // a reported size that is not a whole multiple of 4 would otherwise have its
    // trailing partial id read past what the AU actually wrote. `from_ne_bytes`
    // because these are host-order `AudioUnitParameterID`s, not wire data.
    ids_bytes
        .chunks_exact(std::mem::size_of::<u32>())
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .filter_map(|id| info_at(unit, addr, id).ok())
        .collect()
}

/// Read a parameter value from the global scope / element 0.
pub fn get(unit: AudioUnit, id: u32) -> Result<f32> {
    get_at(unit, ParamAddress::GLOBAL, id)
}

/// Read the value of parameter `id` at `addr`.
///
/// # Errors
/// The AU's own status — `kAudioUnitErr_InvalidElement` (`-10877`) for an
/// element it does not have, `kAudioUnitErr_InvalidParameter` for an id it never
/// declared. Neither is converted into a default value: a fabricated `0.0` here
/// is what a host would then write into the user's preset.
pub fn get_at(unit: AudioUnit, addr: ParamAddress, id: u32) -> Result<f32> {
    let mut value: f32 = 0.0;
    check("AudioUnitGetParameter", unsafe {
        AudioUnitGetParameter(unit, id, addr.scope, addr.element, &mut value)
    })?;
    Ok(value)
}

/// Write a parameter value to the global scope / element 0.
pub fn set(unit: AudioUnit, id: u32, value: f32) -> Result<()> {
    set_at(unit, ParamAddress::GLOBAL, id, value)
}

/// Write `value` to parameter `id` at `addr`.
///
/// # Errors
/// As [`get_at`]. A write to an element the AU does not have fails rather than
/// landing on element 0 — which is precisely the aliasing this address type
/// exists to prevent.
pub fn set_at(unit: AudioUnit, addr: ParamAddress, id: u32, value: f32) -> Result<()> {
    check("AudioUnitSetParameter", unsafe {
        AudioUnitSetParameter(unit, id, addr.scope, addr.element, value, 0)
    })
}

/// Read one parameter's metadata, from the global scope / element 0.
///
/// The single-parameter counterpart of [`list`]. Without it, a caller wanting
/// one parameter's declared range had to `list()` the whole catalog and filter
/// — an O(n) walk, each step a property fetch into the plugin, for an O(1)
/// question.
pub fn info(unit: AudioUnit, param_id: u32) -> Result<AuParameter> {
    info_at(unit, ParamAddress::GLOBAL, param_id)
}

/// Read one parameter's metadata at `addr`.
///
/// Note the AudioToolbox quirk this preserves: `kAudioUnitProperty_ParameterInfo`
/// is fetched with the **parameter id in the element position**, not the element
/// index — the property's "element" argument is documented as the id of the
/// parameter being queried. So metadata is per-`(scope, id)` while *values* are
/// per-`(scope, element, id)`, and `addr.element` deliberately does not appear
/// below. Passing it here instead would query metadata for whatever parameter
/// happened to share that number.
pub fn info_at(unit: AudioUnit, addr: ParamAddress, param_id: u32) -> Result<AuParameter> {
    let raw: AudioUnitParameterInfo = unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO,
            addr.scope,
            param_id,
        )?
    };

    let name = extract_name(&raw);

    Ok(AuParameter {
        id: param_id,
        name,
        range: ParamRange {
            min: raw.minValue,
            max: raw.maxValue,
            default: raw.defaultValue,
        },
        unit: ParameterUnit::from_raw(raw.unit),
        writable: raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_IS_WRITABLE != 0,
        display: DisplayCurve::from_flags(raw.flags),
        meter_read_only: raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_METER_READ_ONLY != 0,
        // `clumpID` is only meaningful when the AU says so. Reading it
        // unconditionally would report clump 0 — or whatever uninitialized value
        // the AU left in the field — as a real group for every ungrouped
        // parameter, collapsing them all into one phantom section.
        clump: (raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CLUMP != 0).then_some(raw.clumpID),
        values_have_strings: raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_VALUES_HAVE_STRINGS != 0,
        can_ramp: raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_CAN_RAMP != 0,
        omit_from_presets: raw.flags & K_AUDIO_UNIT_PARAMETER_FLAG_OMIT_FROM_PRESETS != 0,
        meta: MetaScope::from_flags(raw.flags),
    })
}

/// The ordered display strings for an indexed parameter, or an empty vec if the
/// AU publishes none.
///
/// This is what turns a filter-type control reading "0.000 / 1.000 / 2.000" into
/// "Parametric / Butterworth Low Pass / …". The strings are positional: index `i`
/// names the value `range.min + i`.
///
/// Empty is the "no value strings" answer, not an error, for the reason
/// [`crate::instance::AuInstance::factory_presets`] returns an empty vec — the
/// property is optional and most parameters do not implement it, so a refusal and
/// an empty list are the same answer to the caller's question.
///
/// **Never gate this call on
/// [`AuParameter::values_have_strings`].** Measured on macOS 15.6: AUNBandEQ's
/// "Type" parameter (id 2000) returns all 11 filter names while leaving
/// `kAudioUnitParameterFlag_ValuesHaveStrings` clear. The flag under-reports; the
/// property read is the authority.
///
/// # Errors
/// None — see above. Every failure path yields an empty vec.
pub fn value_strings(unit: AudioUnit, id: u32) -> Vec<String> {
    value_strings_at(unit, ParamAddress::GLOBAL, id)
}

/// [`value_strings`] at an explicit address.
pub fn value_strings_at(unit: AudioUnit, addr: ParamAddress, id: u32) -> Vec<String> {
    // As with ParameterInfo, this property takes the PARAMETER ID in the element
    // position, not the element index — see `info_at`.
    let raw: CFArrayRef = match unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_STRINGS,
            addr.scope,
            id,
        )
    } {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    // The AU *copies* the array for us (Create rule), so the host owns this
    // reference and must release it. `CfArray::from_copied` takes that +1 and
    // releases on drop, on every path out of this function — a leak here would
    // be per-call, and a UI rebuilding a menu polls it.
    let Some(array) = (unsafe { crate::cf::CfArray::from_copied(raw) }) else {
        return Vec::new();
    };

    (0..array.len())
        .filter_map(|i| {
            let ptr = array.value_at(i)?;
            // GET rule, not Create: the elements belong to the array we already
            // own. Wrapping them with `from_copied` would over-release strings
            // the host never retained and corrupt the AU's own table.
            let s = unsafe { cfstring_to_string(ptr as CFStringRef) };
            Some(s)
        })
        .collect()
}

/// The AU's own display string for `value` on parameter `id`, if it publishes
/// one.
///
/// This is how a host renders a value the AU formats specially — the documented
/// examples are a gain parameter whose minimum should read "-∞" rather than
/// "-120.0", and a time parameter better shown as SMPTE `HH:MM:SS:FF` than as
/// seconds.
///
/// `None` means "no special string, format the number yourself", which is the
/// answer for most parameters and is not an error. Distinct from `Some("")`,
/// which would be an AU claiming the empty string is the right label.
///
/// ## Measured absence
///
/// **No Apple AU on macOS 15.6 implements this property.** Probed across 15 units
/// × every parameter × the min/mid/max of each range: every call returned an
/// error status. It is implemented here because third-party AUs do use it and the
/// alternative is a host that cannot display their values — but the round-trip
/// cannot be demonstrated against the corpus, and the tests assert the honest
/// `None` rather than a fabricated success.
///
/// # Errors
/// None — an unimplemented property, an unknown id, and "no string for this
/// value" all yield `None`. The distinction is not actionable: in every case the
/// host formats the number itself.
pub fn string_from_value(unit: AudioUnit, id: u32, value: f32) -> Option<String> {
    string_from_value_at(unit, ParamAddress::GLOBAL, id, value)
}

/// [`string_from_value`] at an explicit address.
pub fn string_from_value_at(
    unit: AudioUnit,
    addr: ParamAddress,
    id: u32,
    value: f32,
) -> Option<String> {
    // `inValue` is a *pointer* to the value, not the value. Borrowing the `value`
    // parameter directly is what keeps it alive across the call below: the
    // pointer must stay valid for the whole `AudioUnitGetProperty`, and the
    // parameter outlives this function body. (A null `inValue` would ask the AU
    // to format its own current value; we always name one explicitly.)
    let mut request = AudioUnitParameterStringFromValue {
        inParamID: id,
        inValue: &value,
        outString: std::ptr::null(),
    };
    let mut size = std::mem::size_of::<AudioUnitParameterStringFromValue>() as u32;
    // Not `get_property`: this property is read/write-through — the struct
    // carries the request in and the answer back out in the same buffer — so the
    // call has to pass the populated struct rather than an uninitialized one.
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_STRING_FROM_VALUE,
            addr.scope,
            addr.element,
            &mut request as *mut _ as *mut std::os::raw::c_void,
            &mut size,
        )
    };
    if status != NO_ERR || request.outString.is_null() {
        return None;
    }
    // Documented Copy rule: "the outName may point to a CFStringRef (which if so
    // must be released by the caller)". `CfString::from_copied` takes that +1, so
    // a host polling this per frame does not leak a CFString per call.
    unsafe { crate::cf::CfString::from_copied(request.outString) }.map(|s| s.to_string())
}

/// Parse `text` into a parameter value using the AU's own interpretation.
///
/// The inverse of [`string_from_value`], and what lets a user type "-6 dB" or
/// "Band Pass" into a field instead of hunting for the raw float. Returning the
/// AU's answer rather than a host-side `str::parse` is the point: only the AU
/// knows that "Band Pass" is 5.0, or how its dB scale maps onto its own range.
///
/// `None` when the AU does not implement the property or cannot parse the string.
/// A host should then leave the field at its previous value rather than writing a
/// fallback — a mis-parsed `0.0` would be silently committed to the user's
/// preset.
///
/// ## Measured absence
///
/// As [`string_from_value`]: no Apple AU on macOS 15.6 implements this. Probed
/// across 15 units × every parameter × 5 candidate strings (`"1.0"`,
/// `"Band Pass"`, `"-6"`, `"Parametric"`, `"8"`), every call failed. Note that
/// AUNBandEQ answers `ParameterValueStrings` for the very parameter whose names
/// those are — the enumeration property and the parsing property are independent,
/// and Apple implements only the former.
///
/// # Errors
/// None, by the same reasoning as [`string_from_value`].
pub fn value_from_string(unit: AudioUnit, id: u32, text: &str) -> Option<f32> {
    value_from_string_at(unit, ParamAddress::GLOBAL, id, text)
}

/// [`value_from_string`] at an explicit address.
pub fn value_from_string_at(
    unit: AudioUnit,
    addr: ParamAddress,
    id: u32,
    text: &str,
) -> Option<f32> {
    // The CFString is borrowed by the AU for the duration of the call only, so a
    // host-owned temporary is correct; `CfString::new` releases it on drop.
    let cf = crate::cf::CfString::new(text)?;
    let mut request = AudioUnitParameterValueFromString {
        inParamID: id,
        inString: cf.as_raw() as CFStringRef,
        // NaN rather than 0.0 as the sentinel: if an AU returned `noErr` without
        // writing the field, a 0.0 would be indistinguishable from a genuine
        // parse of "0" and would be written into the user's preset. NaN is
        // filtered below.
        outValue: f32::NAN,
    };
    let mut size = std::mem::size_of::<AudioUnitParameterValueFromString>() as u32;
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_VALUE_FROM_STRING,
            addr.scope,
            addr.element,
            &mut request as *mut _ as *mut std::os::raw::c_void,
            &mut size,
        )
    };
    if status != NO_ERR || !request.outValue.is_finite() {
        return None;
    }
    Some(request.outValue)
}

/// The AU's label for parameter clump `clump`, e.g. `"Ring Modulation"`.
///
/// Resolves an [`AuParameter::clump`] id into the section heading a UI groups
/// under. Without it a 400-parameter synth is one flat alphabetical list.
///
/// `None` when the AU publishes no name for that clump — including for clump `0`,
/// which Apple's header reserves as "no clump" and which therefore has no label
/// by construction.
///
/// Measured on macOS 15.6: AUDistortion names 7 clumps, AUMultibandCompressor 6,
/// AUMatrixReverb 4, AUSampler 3.
///
/// # Errors
/// None — an unimplemented property or an unknown clump id both yield `None`.
pub fn clump_name(unit: AudioUnit, clump: u32) -> Option<String> {
    clump_name_at(unit, ParamAddress::GLOBAL, clump)
}

/// [`clump_name`] at an explicit address.
pub fn clump_name_at(unit: AudioUnit, addr: ParamAddress, clump: u32) -> Option<String> {
    // Clump 0 is Apple's "ungrouped" sentinel, so there is nothing to name. Ask
    // anyway and an AU may hand back a stray label for a group no parameter
    // claims.
    if clump == 0 {
        return None;
    }
    // `AudioUnitParameterNameInfo` in Apple's header — `{ UInt32 inID; SInt32
    // inDesiredLength; CFStringRef outName; }` for the *parameter* name variant,
    // but the ClumpName property uses the 2-field
    // `{ UInt32 inID; CFStringRef outName; }` shape. Hand-declared because
    // `coreaudio-sys` does not export it; the layout is pinned by
    // `tests::clump_name_request_matches_the_c_abi`.
    #[repr(C)]
    struct ClumpNameRequest {
        in_id: u32,
        out_name: CFStringRef,
    }
    let mut request = ClumpNameRequest {
        in_id: clump,
        out_name: std::ptr::null(),
    };
    let mut size = std::mem::size_of::<ClumpNameRequest>() as u32;
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_CLUMP_NAME,
            addr.scope,
            addr.element,
            &mut request as *mut _ as *mut std::os::raw::c_void,
            &mut size,
        )
    };
    if status != NO_ERR || request.out_name.is_null() {
        return None;
    }
    // Copy rule, as for every CF object retrieved from an AU property.
    unsafe { crate::cf::CfString::from_copied(request.out_name) }.map(|s| s.to_string())
}

/// Which parameters the meta-parameter `id` may move when it is written.
///
/// [`dependents_of_at`] against [`ParamAddress::GLOBAL`], which is where every
/// meta-flagged parameter measured on this machine lives.
pub fn dependents_of(unit: AudioUnit, id: u32) -> Option<Vec<DependentParam>> {
    dependents_of_at(unit, ParamAddress::GLOBAL, id)
}

/// [`dependents_of`] at an explicit scope.
///
/// Reads `kAudioUnitProperty_DependentParameters`, an array of Apple's
/// `AUDependentParameter`. Like [`info_at`] and [`value_strings_at`], this
/// property takes the **parameter id in the element position** — `addr.element`
/// deliberately does not appear below, because the thing being addressed is the
/// meta-parameter itself, not a bus.
///
/// # `None` is "the AU did not say", and is not an empty list
///
/// Three states the caller must be able to tell apart, and a `Vec` alone can
/// only spell two:
///
/// - `None` — the AU does not implement the property, or refused this
///   parameter. **Nothing is known**; a host caching values must assume the
///   worst and re-read everything after a write to this parameter.
/// - `Some(vec![])` — the AU implements it and answered "this meta-parameter
///   moves nothing". A host may skip the re-read.
/// - `Some(non-empty)` — exactly these parameters went stale.
///
/// Collapsing the first into the second is the failure this signature exists to
/// prevent: it would turn "I cannot tell you" into "there is nothing to tell",
/// and the stale cached range would then never be refreshed. That is the same
/// absent-versus-reported discipline the parameter-info surface follows.
///
/// # Measured: no unit on this machine implements it
///
/// macOS 15.6, all 39 instantiable registered units (35 Apple plus TDR Nova,
/// TAL Reverb 4, TAL-NoiseMaker). `AudioUnitGetPropertyInfo` answers
/// `kAudioUnitErr_InvalidProperty` (-10879) at every address tried, including
/// once per each of the **28 parameters that do carry a meta flag** — AUNBandEQ's
/// 8 band "Type" controls, AUGraphicEQ's "Number of Bands", AUPitch's 9,
/// AURoundTripAAC's 3, AURogerBeep's "Sensitivity", AUNewPitch's "Spectral
/// Coherence" and TDR Nova's 5.
///
/// That combination is the useful finding, and it is why
/// [`AuParameter::meta`] is carried separately rather than being derived from
/// this call: the flag is the *only* signal a host gets on this machine. Every
/// one of those 28 parameters can silently move others, and not one will say
/// which. A host must therefore treat `meta.is_some()` as "re-read the
/// parameters this AU publishes", and use a non-`None` answer here purely as an
/// optimization when a plugin does implement it.
///
/// # Errors
/// None — every failure is the `None` above. A refusal is the documented normal
/// case for an optional property, and this call is made speculatively for any
/// meta-flagged parameter, so surfacing it as `Err` would make routine absence
/// indistinguishable from a real fault at every call site.
pub fn dependents_of_at(
    unit: AudioUnit,
    addr: ParamAddress,
    id: u32,
) -> Option<Vec<DependentParam>> {
    // `AUDependentParameter` is `{ AudioUnitScope mScope; AudioUnitParameterID
    // mParameterID; }` — two `u32`s, 8 bytes. Layout pinned by
    // `tests::dependent_parameter_matches_the_c_abi`.
    let bytes = unsafe {
        get_property_bytes(
            unit,
            K_AUDIO_UNIT_PROPERTY_DEPENDENT_PARAMETERS,
            addr.scope,
            id,
        )
    }
    .ok()?;

    const ENTRY: usize = std::mem::size_of::<AUDependentParameter>();
    // A trailing partial entry means the AU disagrees with the header about the
    // struct width. Decode the whole entries and drop the remainder rather than
    // reading past what it wrote: `get_property_bytes` sized the buffer from the
    // AU's own `GetPropertyInfo`, so a non-multiple is the AU contradicting
    // itself, not a short read.
    Some(
        bytes
            .chunks_exact(ENTRY)
            .map(|c| {
                // `read_unaligned` because the `Vec<u8>` carries no alignment
                // guarantee, even though 8-byte entries in practice land aligned.
                let raw: AUDependentParameter =
                    unsafe { std::ptr::read_unaligned(c.as_ptr() as *const AUDependentParameter) };
                DependentParam {
                    scope: raw.mScope,
                    id: raw.mParameterID,
                }
            })
            .collect(),
    )
}

/// Prefer the modern CFString name if advertised; otherwise fall back to the
/// 52-byte fixed buffer (null-terminated or full-width).
fn extract_name(info: &AudioUnitParameterInfo) -> String {
    if info.flags & K_AUDIO_UNIT_PARAMETER_FLAG_HAS_CF_NAME_STRING != 0
        && !info.cfNameString.is_null()
    {
        unsafe {
            crate::cf::CfString::from_copied(info.cfNameString)
                .map(|s| s.to_string())
                .unwrap_or_default()
        }
    } else {
        // `name` is `[c_char; 52]` (i8 on macOS); reinterpret as bytes for
        // the null-terminated fallback decode.
        let name_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(info.name.as_ptr() as *const u8, info.name.len()) };
        let end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_bytes.len());
        String::from_utf8_lossy(&name_bytes[..end]).to_string()
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;
    use crate::component::*;

    fn apple_delay_unit() -> AudioUnit {
        apple_effect(*b"dely", "AUDelay")
    }

    /// AUFilter — carries two `Indexed` filter-type selectors.
    fn apple_filter_unit() -> AudioUnit {
        apple_effect(*b"filt", "AUFilter")
    }

    fn apple_effect(sub_type: [u8; 4], name: &str) -> AudioUnit {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(sub_type),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = find_component(&desc).unwrap_or_else(|| panic!("{name} should be present"));
        let mut instance: AudioComponentInstance = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(comp, &mut instance) };
        assert_eq!(status, NO_ERR);
        unsafe { AudioUnitInitialize(instance) };
        instance
    }

    /// `kAudioUnitParameterUnit_Indexed` (1) decodes to its own variant, not
    /// `Unknown(1)`.
    ///
    /// An `Indexed` parameter is a discrete choice list — the loader turns it
    /// into a nonzero `step_count` so a host renders a picker rather than a
    /// continuous slider. While the variant was missing it fell into `Unknown`
    /// and every such parameter looked continuous.
    #[test]
    fn the_indexed_unit_is_not_unknown() {
        assert_eq!(
            ParameterUnit::from_raw(K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED),
            ParameterUnit::Indexed
        );
        assert_ne!(
            ParameterUnit::from_raw(K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED),
            ParameterUnit::Unknown(K_AUDIO_UNIT_PARAMETER_UNIT_INDEXED)
        );
    }

    /// A real Apple AU reports at least one `Indexed` parameter, with a span
    /// wide enough to be a choice list rather than a toggle.
    ///
    /// AUFilter's two "Filter Type" parameters are `0..1`; AUTimePitch's
    /// "Overlap" is `0..10` (measured, macOS 15.6). Without this, the decode
    /// above could be correct against a constant no installed unit ever sends.
    #[test]
    fn a_real_au_reports_an_indexed_parameter() {
        let unit = apple_filter_unit();
        let params = list(unit);
        let indexed: Vec<_> = params
            .iter()
            .filter(|p| p.unit == ParameterUnit::Indexed)
            .collect();
        assert!(
            !indexed.is_empty(),
            "AUFilter reported no Indexed parameters; it has two filter-type \
             selectors, so either the decode broke or the corpus changed"
        );
        for p in indexed {
            assert!(
                p.range.max > p.range.min,
                "indexed param {:?} has a degenerate range {}..{}",
                p.name,
                p.range.min,
                p.range.max
            );
        }
        unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }

    /// `GLOBAL` must name the global scope and element 0 — it is what every
    /// un-suffixed function in this module delegates to, so a wrong constant
    /// here silently redirects the whole existing API.
    #[test]
    fn the_global_address_is_the_global_scope_at_element_zero() {
        assert_eq!(ParamAddress::GLOBAL.scope, K_AUDIO_UNIT_SCOPE_GLOBAL);
        assert_eq!(ParamAddress::GLOBAL.element, 0);
        assert_eq!(ParamAddress::default(), ParamAddress::GLOBAL);
    }

    /// `on_bus` must put the bus index in the *element* field and the direction
    /// in the *scope* field. Transposing them is the exact mistake the named
    /// type exists to prevent, and both fields are `u32`, so nothing else would
    /// catch it.
    #[test]
    fn on_bus_puts_the_bus_in_the_element_slot() {
        use crate::bus::BusDirection;

        let addr = ParamAddress::on_bus(BusDirection::Input, 3);
        assert_eq!(addr.scope, K_AUDIO_UNIT_SCOPE_INPUT);
        assert_eq!(addr.element, 3);

        let out = ParamAddress::on_bus(BusDirection::Output, 0);
        assert_eq!(out.scope, K_AUDIO_UNIT_SCOPE_OUTPUT);
        assert_eq!(out.element, 0);

        // Output bus 0 and the global address share an element index but are
        // different addresses — so the scope is genuinely carried.
        assert_ne!(out, ParamAddress::GLOBAL);
    }

    /// The display field is non-contiguous: bits 16..=18 plus bit 22. Decoding it
    /// with `7 << 16` alone would map every logarithmic parameter to `Linear`,
    /// which is the majority of the curve-carrying ones (24 of 39 measured).
    #[test]
    fn the_display_mask_spans_the_logarithmic_bit_too() {
        // Ground truth from Apple's header: (7 << 16) | (1 << 22).
        assert_eq!(
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK,
            (7 << 16) | (1 << 22)
        );
        // The bit that a contiguous 3-bit mask would miss.
        assert_ne!(
            K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK
                & K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_LOGARITHMIC,
            0,
            "the mask must cover the logarithmic bit, or every log parameter \
             decodes as linear"
        );
        assert_eq!(K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_LOGARITHMIC, 1 << 22);
    }

    /// `kAudioUnitParameterUnit_MIDIController` (12) must decode to its own
    /// variant, not to `Unknown(12)`.
    ///
    /// A unit test rather than an AU probe, and deliberately so: measured on
    /// macOS 15.6, **no** installed AU reports unit 12 — the `Unknown` codes
    /// that do appear are 1, 5, 7, 9, 10, 16, 18, 21, 24 and 25. So nothing on
    /// this machine can exercise the arm end-to-end, and a real-AU test would be
    /// vacuous. This one is not: deleting the match arm makes it fail, which was
    /// verified by mutation.
    ///
    /// The `Display` output is asserted too, because that is the whole reason
    /// the arm exists — `Unknown(12)` prints `unit(12)`, and a value of `74`
    /// under this unit is "CC 74", a controller number, not a bare quantity.
    #[test]
    fn the_midi_controller_unit_is_not_unknown() {
        assert_eq!(
            ParameterUnit::from_raw(K_AUDIO_UNIT_PARAMETER_UNIT_MIDI_CONTROLLER),
            ParameterUnit::MidiController
        );
        // Pinned to Apple's value, from a C run against the real header.
        assert_eq!(K_AUDIO_UNIT_PARAMETER_UNIT_MIDI_CONTROLLER, 12);
        assert_eq!(ParameterUnit::from_raw(12), ParameterUnit::MidiController);
        assert_eq!(ParameterUnit::MidiController.to_string(), "CC");
        // And the codes actually observed on this machine still fall through to
        // `Unknown`, so the new arm did not swallow a neighbour.
        //
        // Code 1 was in this list when it was measured and is deliberately no
        // longer: it is `kAudioUnitParameterUnit_Indexed` (pinned from Apple's
        // header at `AudioUnitProperties.h:1499`), and it now decodes to its own
        // variant — see `the_indexed_unit_is_not_unknown`. Two suites merged
        // independently, one measuring code 1 as unhandled and the other adding
        // the arm that handles it; the arm is right, so the measurement is what
        // moved on.
        for code in [5u32, 7, 9, 10, 16, 18, 21, 24, 25] {
            assert_eq!(
                ParameterUnit::from_raw(code),
                ParameterUnit::Unknown(code),
                "code {code} is measured on this machine and must stay Unknown"
            );
        }
    }

    /// Every documented curve decodes to its own variant, and the flags word
    /// carries other bits that must not disturb it.
    ///
    /// The concrete flag words are the ones measured on macOS 15.6, so this pins
    /// the decode against reality rather than against reconstructed constants.
    #[test]
    fn each_display_curve_decodes_to_its_own_variant() {
        use DisplayCurve as C;
        for (flags, want, who) in [
            (0u32, C::Linear, "no flag set"),
            // AULowpass "Cutoff Frequency" — 0xc8c00000.
            (0xc8c0_0000, C::Logarithmic, "AULowpass cutoff"),
            // AUDistortion "Delay" — 0xc8910000, disp 0x10000.
            (0xc891_0000, C::SquareRoot, "AUDistortion delay"),
            // AUDistortion "Rounding" — 0xc8930000, disp 0x30000.
            (0xc893_0000, C::Cubed, "AUDistortion rounding"),
            // AUReverb2 "Low Freq Decay Time" — 0xc8050000, disp 0x50000.
            (0xc805_0000, C::Exponential, "AUReverb2 low freq decay"),
            // AUNBandEQ "Type" — 0xd8100000: clumped, but NO display curve.
            (0xd810_0000, C::Linear, "AUNBandEQ type"),
        ] {
            assert_eq!(
                DisplayCurve::from_flags(flags),
                want,
                "{who}: flags {flags:#010x} must decode to {want:?}"
            );
        }
        // The two remaining curves, by construction.
        assert_eq!(
            DisplayCurve::from_flags(K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_SQUARED),
            DisplayCurve::Squared
        );
        assert_eq!(
            DisplayCurve::from_flags(K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_CUBE_ROOT),
            DisplayCurve::CubeRoot
        );
        // Default is the safe taper.
        assert_eq!(DisplayCurve::default(), DisplayCurve::Linear);
    }

    /// An unrecognized bit pattern in the display field must fall back to
    /// `Linear`, never panic and never be reported as a real curve.
    ///
    /// `6 << 16` is inside the mask but is not one of Apple's six defined curves.
    #[test]
    fn an_undefined_display_code_falls_back_to_linear() {
        assert_eq!(DisplayCurve::from_flags(6 << 16), DisplayCurve::Linear);
        // And bits outside the mask never select a curve on their own: every
        // non-display flag set at once still decodes to Linear.
        assert_eq!(
            DisplayCurve::from_flags(!K_AUDIO_UNIT_PARAMETER_FLAG_DISPLAY_MASK),
            DisplayCurve::Linear
        );
    }

    /// `clump` must be `None` unless the AU sets `HasClump`. Reading `clumpID`
    /// unconditionally reports clump 0 — or whatever the AU left in the field —
    /// as a real group for every ungrouped parameter, collapsing them into one
    /// phantom section.
    #[test]
    fn a_clump_id_is_only_read_when_the_au_advertises_one() {
        let unit = apple_delay_unit();
        // AUDelay's parameters are measured to carry no clump flag.
        for p in list(unit) {
            assert!(
                p.clump.is_none(),
                "AUDelay param {} ({:?}) reported clump {:?} without HasClump",
                p.id,
                p.name,
                p.clump
            );
        }
        unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }

    /// Clump 0 is Apple's "ungrouped" sentinel and has no label by construction,
    /// so it must not be queried — an AU could otherwise hand back a stray name
    /// for a group no parameter claims.
    #[test]
    fn clump_zero_is_never_named() {
        let unit = apple_delay_unit();
        assert_eq!(clump_name(unit, 0), None);
        unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }

    #[test]
    fn test_list() {
        let unit = apple_delay_unit();
        let params = list(unit);
        assert!(!params.is_empty());
        unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }

    #[test]
    fn test_get_set() {
        let unit = apple_delay_unit();
        let params = list(unit);
        assert!(!params.is_empty());

        let p = &params[0];
        let mid = p.range.mid();
        set(unit, p.id, mid).unwrap();
        let val = get(unit, p.id).unwrap();
        assert!((val - mid).abs() < 0.01);

        unsafe {
            AudioUnitUninitialize(unit);
            AudioComponentInstanceDispose(unit);
        }
    }
}

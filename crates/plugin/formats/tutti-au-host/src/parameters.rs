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
//! else. A mixer-hosting caller reaches for the `_at` functions directly.

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
}

/// Classification of a parameter's physical unit.
///
/// Hosts use this to choose a display formatter (e.g. append `"Hz"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterUnit {
    /// Dimensionless / unclassified.
    Generic,
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
    /// An AU-specific unit code this crate doesn't recognize.
    Unknown(u32),
}

impl ParameterUnit {
    /// Map a raw `kAudioUnitParameterUnit_*` code onto the typed variant.
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            K_AUDIO_UNIT_PARAMETER_UNIT_GENERIC => Self::Generic,
            K_AUDIO_UNIT_PARAMETER_UNIT_BOOLEAN => Self::Boolean,
            K_AUDIO_UNIT_PARAMETER_UNIT_PERCENT => Self::Percent,
            K_AUDIO_UNIT_PARAMETER_UNIT_SECONDS => Self::Seconds,
            K_AUDIO_UNIT_PARAMETER_UNIT_HERTZ => Self::Hertz,
            K_AUDIO_UNIT_PARAMETER_UNIT_DECIBELS => Self::Decibels,
            K_AUDIO_UNIT_PARAMETER_UNIT_LINEAR_GAIN => Self::LinearGain,
            other => Self::Unknown(other),
        }
    }
}

impl std::fmt::Display for ParameterUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generic => write!(f, ""),
            Self::Boolean => write!(f, "bool"),
            Self::Percent => write!(f, "%"),
            Self::Seconds => write!(f, "s"),
            Self::Hertz => write!(f, "Hz"),
            Self::Decibels => write!(f, "dB"),
            Self::LinearGain => write!(f, "gain"),
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

/// Read one parameter's metadata at `addr`.
///
/// Note the AudioToolbox quirk this preserves: `kAudioUnitProperty_ParameterInfo`
/// is fetched with the **parameter id in the element position**, not the element
/// index — the property's "element" argument is documented as the id of the
/// parameter being queried. So metadata is per-`(scope, id)` while *values* are
/// per-`(scope, element, id)`, and `addr.element` deliberately does not appear
/// below. Passing it here instead would query metadata for whatever parameter
/// happened to share that number.
fn info_at(unit: AudioUnit, addr: ParamAddress, param_id: u32) -> Result<AuParameter> {
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
    })
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
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = find_component(&desc).expect("AUDelay should be present");
        let mut instance: AudioComponentInstance = std::ptr::null_mut();
        let status = unsafe { AudioComponentInstanceNew(comp, &mut instance) };
        assert_eq!(status, NO_ERR);
        unsafe { AudioUnitInitialize(instance) };
        instance
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

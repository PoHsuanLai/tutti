//! Parameter discovery, read, and write APIs for Audio Units.
//!
//! LIMITATION (A-3, intentional): every parameter access here — `list`, `get`,
//! `set`, and `info` — is hard-wired to `kAudioUnitScope_Global` / element `0`.
//! AUs may in principle expose parameters on other scopes (Input/Output) or on
//! non-zero elements (per-bus / per-part). No such AU is exercised by this host
//! today, so per-scope/per-element enumeration is deferred until a real plugin
//! needs it rather than modeling surface nothing consumes.

#![cfg(target_os = "macos")]
// AudioUnit is an opaque C pointer (`ComponentInstanceRecord*`) that every
// AudioToolbox call dereferences. Callers must supply a valid unit, same as
// every other entry point in this crate.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::marker::PhantomData;

use crate::error::Result;
use crate::ffi::{check, get_property, get_property_bytes};
use crate::types::*;

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

/// Enumerate all parameters on the given raw `AudioUnit`.
///
/// Returns an empty vec if the AU doesn't advertise a parameter list.
pub fn list(unit: AudioUnit) -> Vec<AuParameter> {
    let ids_bytes = match unsafe {
        get_property_bytes(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_LIST,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )
    } {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    let count = ids_bytes.len() / std::mem::size_of::<u32>();
    let ids: &[u32] =
        unsafe { std::slice::from_raw_parts(ids_bytes.as_ptr() as *const u32, count) };

    ids.iter().filter_map(|&id| info(unit, id).ok()).collect()
}

/// Read a parameter value.
pub fn get(unit: AudioUnit, id: u32) -> Result<f32> {
    let mut value: f32 = 0.0;
    check("AudioUnitGetParameter", unsafe {
        AudioUnitGetParameter(unit, id, K_AUDIO_UNIT_SCOPE_GLOBAL, 0, &mut value)
    })?;
    Ok(value)
}

/// Write a parameter value.
pub fn set(unit: AudioUnit, id: u32, value: f32) -> Result<()> {
    check("AudioUnitSetParameter", unsafe {
        AudioUnitSetParameter(unit, id, K_AUDIO_UNIT_SCOPE_GLOBAL, 0, value, 0)
    })
}

fn info(unit: AudioUnit, param_id: u32) -> Result<AuParameter> {
    let raw: AudioUnitParameterInfo = unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETER_INFO,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
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

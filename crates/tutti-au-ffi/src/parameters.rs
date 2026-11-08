//! The parameter-shaped property queries whose request and answer share one
//! struct, and the info record whose name is a CoreFoundation string.

use std::mem::size_of;

use coreaudio_sys as sys;

use crate::cf::CfString;
use crate::error::Result;
use crate::unit::Unit;

/// `AudioUnitParameterNameInfo`'s two-field form that `ParameterClumpName`
/// uses: `{ UInt32 inID; CFStringRef outName; }`. Hand-declared because
/// `coreaudio-sys` does not export it; the layout is pinned by
/// `clump_name_request_matches_the_c_abi`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ClumpNameRequest {
    in_id: u32,
    out_name: sys::CFStringRef,
}

impl crate::plain::Sealed for ClumpNameRequest {}
// SAFETY: a `u32` and a raw pointer in a `#[repr(C)]` struct: every bit
// pattern, zero included, is valid.
unsafe impl crate::plain::Plain for ClumpNameRequest {}

impl crate::plain::Sealed for sys::AudioUnitParameterStringFromValue {}
// SAFETY: a `u32` and two raw pointers in a `#[repr(C)]` struct.
unsafe impl crate::plain::Plain for sys::AudioUnitParameterStringFromValue {}

impl crate::plain::Sealed for sys::AudioUnitParameterValueFromString {}
// SAFETY: a `u32`, a raw pointer and an `f32` in a `#[repr(C)]` struct.
unsafe impl crate::plain::Plain for sys::AudioUnitParameterValueFromString {}

impl Unit {
    /// `kAudioUnitProperty_ParameterInfo` for parameter `id` at `scope`, with
    /// the display name resolved: the CF name when the record carries one (and
    /// it is released here), otherwise the fixed C-string field.
    ///
    /// # Errors
    /// The `OSStatus` of the read.
    pub fn parameter_info(
        &self,
        scope: u32,
        id: u32,
    ) -> Result<(sys::AudioUnitParameterInfo, String)> {
        let raw: sys::AudioUnitParameterInfo =
            self.get(sys::kAudioUnitProperty_ParameterInfo, scope, id)?;
        let name = if raw.flags & sys::kAudioUnitParameterFlag_HasCFNameString != 0
            && !raw.cfNameString.is_null()
        {
            // SAFETY: with `HasCFNameString` set the record's `cfNameString` is
            // a CFString the caller owns one retain of (the header's rule).
            unsafe { CfString::from_copied(raw.cfNameString) }
                .map(|s| s.to_string())
                .unwrap_or_default()
        } else {
            // `name` is `[c_char; 52]`; reinterpret as bytes for the
            // null-terminated fallback decode.
            let bytes: Vec<u8> = raw.name.iter().map(|&c| c as u8).collect();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            String::from_utf8_lossy(&bytes[..end]).to_string()
        };
        Ok((raw, name))
    }

    /// The unit's own text for `value` of parameter `id`, or `None` when it
    /// does not format one.
    pub fn parameter_string_from_value(
        &self,
        scope: u32,
        element: u32,
        id: u32,
        value: f32,
    ) -> Option<String> {
        // `inValue` is a *pointer* to the value; borrowing the parameter keeps
        // it alive across the call. A null `inValue` would ask for the unit's
        // own current value; this always names one explicitly.
        let request = sys::AudioUnitParameterStringFromValue {
            inParamID: id,
            inValue: &value,
            outString: std::ptr::null(),
        };
        // The property carries the request in and the answer back out in the
        // same struct.
        let answer = self
            .get_in_out(
                sys::kAudioUnitProperty_ParameterStringFromValue,
                scope,
                element,
                request,
            )
            .ok()?;
        // SAFETY: documented Copy rule: "the outString ... must be released by
        // the caller", so a non-null answer is one retain we own.
        unsafe { CfString::from_copied(answer.outString) }.map(|s| s.to_string())
    }

    /// The value the unit parses from `text` for parameter `id`, or `None` when
    /// it declines or answers a non-finite figure.
    pub fn parameter_value_from_string(
        &self,
        scope: u32,
        element: u32,
        id: u32,
        text: &str,
    ) -> Option<f32> {
        // The CFString is borrowed by the unit for the duration of the call
        // only, so a host-owned temporary is correct.
        let string = CfString::new(text);
        let request = sys::AudioUnitParameterValueFromString {
            inParamID: id,
            inString: string.as_raw(),
            // NaN rather than 0.0 as the sentinel: a unit that returned `noErr`
            // without writing the field would otherwise read as a genuine
            // parse of "0".
            outValue: f32::NAN,
        };
        let answer = self
            .get_in_out(
                sys::kAudioUnitProperty_ParameterValueFromString,
                scope,
                element,
                request,
            )
            .ok()?;
        answer.outValue.is_finite().then_some(answer.outValue)
    }

    /// The unit's name for parameter group `clump`, or `None`.
    pub fn parameter_clump_name(&self, scope: u32, element: u32, clump: u32) -> Option<String> {
        let request = ClumpNameRequest {
            in_id: clump,
            out_name: std::ptr::null(),
        };
        let answer = self
            .get_in_out(
                sys::kAudioUnitProperty_ParameterClumpName,
                scope,
                element,
                request,
            )
            .ok()?;
        // SAFETY: Copy rule, as for every CF object retrieved from an AU
        // property.
        unsafe { CfString::from_copied(answer.out_name) }.map(|s| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clump request is hand-declared, so its layout is pinned here:
    /// `{ UInt32; CFStringRef }` is 16 bytes with the pointer at offset 8.
    #[test]
    fn clump_name_request_matches_the_c_abi() {
        assert_eq!(size_of::<ClumpNameRequest>(), 16);
        assert_eq!(std::mem::offset_of!(ClumpNameRequest, in_id), 0);
        assert_eq!(std::mem::offset_of!(ClumpNameRequest, out_name), 8);
    }

    #[test]
    fn the_name_field_is_a_whole_c_string_buffer() {
        // The fallback decode reads every byte of the fixed field.
        let info = sys::AudioUnitParameterInfo::default();
        assert_eq!(info.name.len(), 52);
    }
}

//! Small conversion helpers for VST3 C buffers and raw-byte types.

use vst3::Steinberg::TUID;

/// A UTF-16 buffer up to its first NUL (or its end), lossily decoded.
///
/// The reader of every `String128` and `TChar` array a plugin fills in.
#[must_use]
pub fn utf16_to_string(bytes: &[u16]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf16_lossy(&bytes[..end])
}

/// A `char8` buffer up to its first NUL (or its end), lossily decoded as UTF-8.
#[must_use]
pub fn c_str_to_string(bytes: &[i8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    // `i8 as u8` reinterprets each byte; no unsafe slice cast is needed.
    let u8s: Vec<u8> = bytes[..end].iter().map(|&b| b as u8).collect();
    String::from_utf8_lossy(&u8s).into_owned()
}

/// Formats a 16-byte class ID as four dash-separated groups of four bytes
/// (`XXXXXXXX-XXXXXXXX-XXXXXXXX-XXXXXXXX`), upper-case hex. The catalog ids
/// (`vst3.<id>`) are persisted in this shape.
#[must_use]
pub fn cid_to_string(cid: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}",
        cid[0],
        cid[1],
        cid[2],
        cid[3],
        cid[4],
        cid[5],
        cid[6],
        cid[7],
        cid[8],
        cid[9],
        cid[10],
        cid[11],
        cid[12],
        cid[13],
        cid[14],
        cid[15]
    )
}

/// Converts a 16-byte `Guid` (`[u8; 16]`) to a `TUID` (`[i8; 16]`).
#[must_use]
pub fn guid_as_tuid(guid: &vst3::com_scrape_types::Guid) -> TUID {
    std::array::from_fn(|i| guid[i] as i8)
}

/// The integer type the `vst3` bindings give every SDK enum constant:
/// `DefaultEnumType`, which is `c_int` on Windows and `c_uint` everywhere else.
///
/// The bindings do not re-export `DefaultEnumType` (their `support` module is
/// private), so it is named here through `MediaTypes`, one of the public
/// aliases they define as exactly that type. Naming it through an alias rather
/// than mirroring the `cfg` means a change in the bindings surfaces as a type
/// error at [`sdk_enum_i32`]'s call sites, not as a silently stale copy.
pub type SdkEnum = vst3::Steinberg::Vst::MediaTypes;

/// Widens an SDK enum constant to the `int32` the ABI fields carrying it use
/// (`BusInfo::mediaType`, `ProcessSetup::processMode`, `IBStream::seek`'s
/// `mode`, …).
///
/// The one place this crate spells that conversion, so the platform split
/// lives here rather than at every call site: on Windows `SdkEnum` already *is*
/// `i32` and the cast is a no-op; elsewhere it is a real `u32 -> i32`. Every
/// value passed is a small SDK ordinal, so it never wraps.
#[allow(
    clippy::unnecessary_cast,
    reason = "a no-op on Windows, where `DefaultEnumType` is `c_int`; load-bearing on every other target"
)]
#[must_use]
pub const fn sdk_enum_i32(v: SdkEnum) -> i32 {
    v as i32
}

/// An SDK struct for which the all-zero bit pattern is a valid value: plain
/// integers, floats, arrays of them, raw pointers and unions of those.
///
/// Sealed: only the structs this crate lists below implement it. They are the
/// out-parameter and event records of the SDK, which a plugin fills in or the
/// host fills field by field.
pub trait Zeroable: private::Sealed + Sized {}

mod private {
    pub trait Sealed {}
}

macro_rules! zeroable {
    ($($ty:ty),+ $(,)?) => {$(
        impl private::Sealed for $ty {}
        impl Zeroable for $ty {}
    )+};
}

zeroable!(
    vst3::Steinberg::PFactoryInfo,
    vst3::Steinberg::PClassInfo,
    vst3::Steinberg::PClassInfoW,
    vst3::Steinberg::Vst::ParameterInfo,
    vst3::Steinberg::Vst::NoteExpressionTypeInfo,
    vst3::Steinberg::Vst::KeyswitchInfo,
    vst3::Steinberg::Vst::ProgramListInfo,
    vst3::Steinberg::Vst::UnitInfo,
    vst3::Steinberg::Vst::BusInfo,
    vst3::Steinberg::Vst::RepresentationInfo,
    vst3::Steinberg::Vst::Event,
    vst3::Steinberg::Vst::ProcessContext,
    vst3::Steinberg::Vst::AudioBusBuffers,
);

/// The all-zero value of an SDK struct.
///
/// What a C host writes as `ParameterInfo info = {};` before handing it to a
/// plugin to fill in.
#[must_use]
pub fn zeroed<T: Zeroable>() -> T {
    // SAFETY: `Zeroable` is implemented only for the SDK structs listed above,
    // each made of integers, floats, arrays of them, raw pointers and unions of
    // those, for which all-zero bytes is a valid value (null pointers, `0`,
    // `0.0`).
    unsafe { std::mem::zeroed() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `String128` is read up to its first NUL, whatever follows it; a
    /// buffer with no NUL is read whole; an unpaired surrogate becomes U+FFFD
    /// instead of failing.
    ///
    /// Mutation: drop the `position(..)` and read the whole buffer: the first
    /// assert sees the bytes after the NUL and fails.
    #[test]
    fn a_string128_is_read_to_its_nul_and_decoded_lossily() {
        let mut buf = [0u16; 128];
        for (slot, c) in buf.iter_mut().zip("Väst".encode_utf16()) {
            *slot = c;
        }
        buf[10] = u16::from(b'x'); // after the NUL: must not appear
        assert_eq!(utf16_to_string(&buf), "Väst");

        let full: Vec<u16> = "音源".encode_utf16().collect();
        assert_eq!(utf16_to_string(&full), "音源");
        assert_eq!(utf16_to_string(&[0xD800, 0]), "\u{FFFD}");
        assert_eq!(utf16_to_string(&[]), "");
    }

    /// A `char8` buffer is read to its first NUL as UTF-8, lossily, with the
    /// high bit kept (`i8` bytes above 127 are negative).
    ///
    /// Mutation: map bytes with `b as u8 & 0x7F` and the accented case fails.
    #[test]
    fn a_char8_buffer_is_read_to_its_nul_as_utf8() {
        let mut buf = [0i8; 16];
        for (slot, b) in buf.iter_mut().zip("Éclat".bytes()) {
            *slot = b as i8;
        }
        assert_eq!(c_str_to_string(&buf), "Éclat");
        assert_eq!(c_str_to_string(&[0x61, -1, 0]), "a\u{FFFD}");
    }

    /// A class id formats as four dashed groups of four bytes, upper-case hex.
    ///
    /// Mutation: lower-case the format string and the assert fails.
    #[test]
    fn a_class_id_formats_as_dashed_upper_case_hex() {
        let cid: [u8; 16] = std::array::from_fn(|i| (i as u8) * 17);
        assert_eq!(cid_to_string(&cid), "00112233-44556677-8899AABB-CCDDEEFF");
    }
}

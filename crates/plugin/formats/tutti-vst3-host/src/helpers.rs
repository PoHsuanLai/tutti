//! Small conversion helpers for VST3 C buffers and raw-byte types.

use vst3::Steinberg::TUID;

pub fn utf16_to_string(bytes: &[u16]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf16_lossy(&bytes[..end])
}

pub fn c_str_to_string(bytes: &[i8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    // i8 and u8 have identical layout; the cast is always sound.
    let u8s: &[u8] = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast(), end) };
    String::from_utf8_lossy(u8s).into_owned()
}

/// Format a 16-byte class ID as a `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` hex string.
pub(crate) fn cid_to_string(cid: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}",
        cid[0], cid[1], cid[2], cid[3],
        cid[4], cid[5], cid[6], cid[7],
        cid[8], cid[9], cid[10], cid[11],
        cid[12], cid[13], cid[14], cid[15]
    )
}

/// Convert a 16-byte `Guid` (`[u8; 16]`) to a `TUID` (`[i8; 16]`).
pub(crate) fn guid_as_tuid(guid: &vst3::com_scrape_types::Guid) -> TUID {
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
pub(crate) type SdkEnum = vst3::Steinberg::Vst::MediaTypes;

/// Widen an SDK enum constant to the `int32` the ABI fields carrying it use
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
pub(crate) const fn sdk_enum_i32(v: SdkEnum) -> i32 {
    v as i32
}

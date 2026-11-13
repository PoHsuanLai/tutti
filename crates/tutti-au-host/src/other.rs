//! The off-macOS half of the platform seam: what `crate::component` and
//! `crate::error` need from the platform, with no Audio Unit behind it.
//!
//! There is no AudioToolbox here, so no component can be found and no unit
//! loaded; the portable types (`AuType`, `AuComponentInfo`, `AuError`) still
//! compile, so a scanner's types are the same everywhere.

pub(crate) mod discovery {
    /// Stands in for the AudioToolbox component handle. Nothing off macOS can
    /// make one, so a component that carries it cannot exist either.
    #[derive(Debug, Clone, Copy)]
    pub enum AuComponentRef {}
}

/// How an [`AuType::Unknown`](crate::component::AuType::Unknown) code reads:
/// the hex value (there is no four-char-code decoder off macOS).
pub(crate) fn unknown_code(code: u32) -> String {
    format!("{code:#x}")
}

/// Every status code reads as unknown: the status table is AudioToolbox's.
pub(crate) fn os_status_message(_code: i32) -> &'static str {
    "unknown error"
}

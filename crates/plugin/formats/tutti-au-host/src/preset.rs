//! Factory-preset vocabulary: the host-side shape of an `AUPreset`.
//!
//! AudioToolbox's `AUPreset` is `{ SInt32 presetNumber; CFStringRef presetName; }`
//! — a number paired with a *borrowed* CoreFoundation string whose lifetime is
//! the AU's, not the host's. Neither half survives the array it came out of, so
//! this crate copies both into owned Rust storage at the boundary rather than
//! handing a `CFStringRef` outward: a caller holding one after the enclosing
//! `CFArray` is released would be reading freed memory, and nothing in the type
//! would have warned them.

#![cfg(target_os = "macos")]

/// One factory preset the AU advertises: its selector and its display name.
///
/// `number` is the value to hand back to
/// [`AuInstance::load_factory_preset`](crate::instance::AuInstance::load_factory_preset).
/// It is an AU-assigned selector, not an index: presets are commonly numbered
/// `0..n`, but the AU is free to number them sparsely, so never derive one from
/// a position in the [`factory_presets`](crate::instance::AuInstance::factory_presets)
/// vec.
///
/// Plain `i32`/`String` and no unit newtype: a preset number is an opaque
/// identifier and a name is text — neither is a physical quantity, so there is
/// no unit for them to carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuPreset {
    /// AU-assigned preset selector, as passed to `kAudioUnitProperty_PresentPreset`.
    pub number: i32,
    /// Display name, copied out of the AU's `CFStringRef` into owned storage.
    pub name: String,
}

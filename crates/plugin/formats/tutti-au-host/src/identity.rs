//! What the host tells the AU about itself, and what the AU offers a host UI.
//!
//! Four properties that carry no audio and change no processing — they exist so
//! a plugin's own window and a host's plugin browser can say something truthful
//! about *this* instance rather than about the AU in the abstract:
//!
//! * [`set_context_name`] — where the instance sits in the project ("track 3").
//! * [`set_nick_name`] / [`nick_name`] — the instance's own name, so two loads
//!   of the same AU are distinguishable.
//! * [`parameters_for_overview`] — the AU's curated shortlist of its own most
//!   important parameters.
//! * [`icon_location`] — a file URL for the AU's icon.
//!
//! Without the first two, every instance of a plugin in a session presents as
//! the same anonymous "AUMultibandCompressor", and a user with six of them open
//! has no way to tell which is which from the plugin's own window. Without
//! [`parameters_for_overview`], a compact channel-strip view has to guess which
//! knobs matter and generally shows the first N in declaration order — which is
//! not the same list, see below.
//!
//! # Which AUs actually implement these (measured, macOS 15.6, 52 units)
//!
//! ```text
//! ContextName            52/52   round-trips the exact string
//! NickName               52/52   round-trips the exact string
//! IconLocation           36/52   non-null URL
//! ParametersForOverview  26/52   a genuine subset, reordered
//! ```
//!
//! The first two are answered by *every* instantiable unit on the machine,
//! including the third-party ones, which is why they are plain `Result` writes
//! with no capability query: there is no meaningful "does this AU support it"
//! branch for a host to take.
//!
//! [`parameters_for_overview`] is the one that carries real information. The AU
//! is not merely truncating its parameter list — it curates and *reorders*:
//!
//! ```text
//! AUMatrixReverb         8 of 17 parameters
//! AUTimePitch            4 of 24
//! AUMultibandCompressor 19 of 31, first ids [0, 1, 13, 14, 2, 3]
//! AUDynamicsProcessor    7 of 10, first ids [4, 5, 6, 0, 1, 2]
//! ```
//!
//! Note the id order in the last two: the overview leads with parameters that
//! are *not* first in the parameter list. A host that approximated this by
//! taking the first N of [`crate::parameters`] would show a different and worse
//! set. That reordering is the reason this property is worth reading at all,
//! and it is what the round-trip test pins.
//!
//! # Why `kAudioUnitProperty_HostMIDIProtocol` is not here
//!
//! It governs a delivery path this crate does not host. Apple's header
//! specifies the order: set `HostMIDIProtocol`, then
//! `kAudioUnitProperty_MIDIOutputEventListCallback`, then initialize. That
//! callback is the UMP route; [`crate::midi_out`] installs the legacy
//! `MIDIOutputCallback` instead, so there is nothing downstream of the
//! declaration.
//!
//! Do **not** justify this by comparing it to
//! `kAudioUnitProperty_AudioUnitMIDIProtocol` (64). The two are independent by
//! design: 64 is the AU's protocol, 65 the host's, and the framework converts
//! between them, so 64's value says nothing about whether 65 is needed.
//!
//! One measured trap if this is ever implemented: the header says twice that 65
//! cannot be changed after initialize, but 0 of 55 units enforce that.
//!
//! The related point that *does* matter is upstream of this property:
//! [`AuInstance::send_midi`](crate::instance::AuInstance::send_midi) down-converts
//! MIDI 2.0 to MIDI 1.0 because AUv2's `MusicDeviceMIDIEvent` takes legacy
//! channel-voice bytes and has no UMP form. Carrying MIDI 2.0 resolution into an
//! AU requires the AUv3 event-list entry point, not this property.

#![cfg(target_os = "macos")]

use crate::cf::{CfString, CfUrl};
use crate::error::Result;
use crate::ffi::{get_property, get_property_bytes, set_property};
use crate::types::*;

/// One entry of the AU's overview shortlist: a parameter, and where it lives.
///
/// The AU returns Apple's `AudioUnitParameter`, whose first field is a raw
/// `AudioUnit` handle pointing back at the instance that produced it. That
/// pointer is dropped here rather than carried: it is redundant (the caller
/// already holds the instance it asked) and it would put a raw, unlifetimed
/// handle into a plain `Clone`/`Copy` value that could outlive the AU.
///
/// Plain `u32`s and no unit newtype: a parameter id, a scope and an element are
/// opaque selectors, not physical quantities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverviewParameter {
    /// The parameter's id, as used with [`crate::parameters`].
    pub id: u32,
    /// Scope the parameter lives in — normally [`K_AUDIO_UNIT_SCOPE_GLOBAL`].
    pub scope: u32,
    /// Element within that scope.
    pub element: u32,
}

/// Tell the AU where it sits in the host's project — "track 3", "Drum Bus".
///
/// This is *context*, not identity: it describes the slot, so moving the plugin
/// to another track should rewrite it. For the instance's own name, which is
/// what belongs in a saved session, use [`set_nick_name`].
///
/// The AU copies the string during the call — the header specifies a `CFString`
/// value and every implementer honours the Get/Set rule — so the temporary
/// created here is released on return without the AU holding a dangling
/// reference.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it,
/// though all 52 units measured do. Returns [`AuError::CfStringAlloc`] if
/// CoreFoundation declines to allocate the string.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn set_context_name(unit: AudioUnit, name: &str) -> Result<()> {
    let cf = CfString::new(name).ok_or(crate::error::AuError::CfStringAlloc)?;
    unsafe {
        set_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_CONTEXT_NAME,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &cf.as_raw(),
        )
    }
}

/// Read back the context name set by [`set_context_name`].
///
/// Apple's header marks this property `Read / Write`, and all 57 instantiable
/// units measured on macOS 15.6 return the exact string written. It exists
/// mainly so a test can assert the *write landed* rather than merely that it
/// returned `noErr` — a distinction this crate has been bitten by twice
/// (`HostCallbacks`, `HostMIDIProtocol`).
///
/// `Ok(None)` means the AU answered with a null string.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn context_name(unit: AudioUnit) -> Result<Option<String>> {
    // Copy rule, as in `nick_name`: the returned CFString is +1 and owned here.
    let raw: CFStringRef = unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_CONTEXT_NAME,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )?
    };
    Ok(unsafe { CfString::from_copied(raw) }.map(|s| s.to_string()))
}

/// Give this instance its own name, distinguishing it from another load of the
/// same AU.
///
/// Unlike [`set_context_name`] this is the instance's identity rather than its
/// slot, so it is the one to persist in a session and restore on load.
///
/// # Errors
/// As [`set_context_name`].
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn set_nick_name(unit: AudioUnit, name: &str) -> Result<()> {
    let cf = CfString::new(name).ok_or(crate::error::AuError::CfStringAlloc)?;
    unsafe {
        set_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_NICK_NAME,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &cf.as_raw(),
        )
    }
}

/// Read back the instance name set by [`set_nick_name`].
///
/// `Ok(None)` means the AU answered with a null string — it implements the
/// property but has no name set — which is distinct from the `Err` an AU that
/// does not implement it at all returns.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn nick_name(unit: AudioUnit) -> Result<Option<String>> {
    // Copy rule: `AudioUnitGetProperty` on a CFString property returns a +1
    // reference the host owns, so this must be balanced. `CfString::from_copied`
    // takes that ownership and releases on drop.
    let raw: CFStringRef = unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_NICK_NAME,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )?
    };
    Ok(unsafe { CfString::from_copied(raw) }.map(|s| s.to_string()))
}

/// The AU's own shortlist of its most important parameters, in the AU's
/// priority order.
///
/// Use this to populate a compact view — a channel strip, a collapsed rack row
/// — instead of truncating [`crate::parameters`]. The two are not the same
/// list: several units lead the overview with parameters that sit in the middle
/// of their declaration order (see the module header).
///
/// # Why the caller does not choose a count
///
/// Apple's header says the size of the array passed in controls how many are
/// returned, which invites a host to ask for "the top 3". That is a worse
/// interface than it looks: the AU has *already* ranked them, so truncating is
/// the caller's business and doing it here would silently discard the tail. This
/// asks for the full list — via [`get_property_bytes`], which sizes the buffer
/// from the AU's own `GetPropertyInfo` — and lets the caller take what it wants.
///
/// An AU that implements the property but curates nothing returns an empty vec.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from the 26 of 52 units that do not
/// implement it, which is the common case and not an error condition for a host
/// — fall back to [`crate::parameters`].
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn parameters_for_overview(unit: AudioUnit) -> Result<Vec<OverviewParameter>> {
    let bytes = unsafe {
        get_property_bytes(
            unit,
            K_AUDIO_UNIT_PROPERTY_PARAMETERS_FOR_OVERVIEW,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )?
    };
    let stride = std::mem::size_of::<AudioUnitParameter>();
    // Trailing bytes short of a whole struct are dropped rather than trusted:
    // decoding a partial struct would read uninitialised padding as a
    // parameter id. `stride` is 24 on arm64 (an 8-byte handle plus three u32s
    // plus padding), never zero, so the division is safe.
    let count = bytes.len() / stride;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: `bytes` holds at least `(i + 1) * stride` bytes, and
        // `AudioUnitParameter` is a `#[repr(C)]` struct of a pointer and three
        // `u32`s — valid for any bit pattern the AU writes. Read unaligned
        // because `Vec<u8>` guarantees only 1-byte alignment.
        let p: AudioUnitParameter = unsafe {
            bytes
                .as_ptr()
                .add(i * stride)
                .cast::<AudioUnitParameter>()
                .read_unaligned()
        };
        out.push(OverviewParameter {
            id: p.mParameterID,
            scope: p.mScope,
            element: p.mElement,
        });
    }
    Ok(out)
}

/// A file URL for the AU's icon, for a plugin browser row.
///
/// `Ok(None)` means the AU implements the property but supplied no URL. 36 of
/// 52 units measured return a usable URL.
///
/// # Errors
/// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it.
///
/// # Safety
/// `unit` must reference a live, valid AudioUnit.
pub unsafe fn icon_location(unit: AudioUnit) -> Result<Option<String>> {
    // Copy rule, as in `nick_name`: the returned CFURL is +1 and owned here.
    let raw: coreaudio_sys::CFURLRef = unsafe {
        get_property(
            unit,
            K_AUDIO_UNIT_PROPERTY_ICON_LOCATION,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )?
    };
    Ok(unsafe { CfUrl::from_copied(raw) }.and_then(|u| u.to_path_string()))
}

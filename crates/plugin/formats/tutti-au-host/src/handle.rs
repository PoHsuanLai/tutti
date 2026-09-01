//! Owned handle to an instantiated (but not necessarily initialized) AU.

#![cfg(target_os = "macos")]

use crate::cf::CfString;
use crate::component::AuType;
use crate::error::{AuError, LoadStage, Result};
use crate::ffi::check;
use crate::types::*;

/// RAII wrapper around an `AudioComponentInstance`.
///
/// Dropping the handle calls `AudioComponentInstanceDispose`, so the raw
/// pointer must not be used after the handle is dropped. The handle is
/// `Send` but not `Sync`; all AU property/parameter access serializes
/// through the owning thread.
pub struct AuHandle {
    instance: AudioComponentInstance,
    component: AudioComponent,
    au_type: AuType,
}

// SAFETY: AudioComponentInstance is a plain pointer; AudioToolbox tolerates
// use from any thread as long as access is externally synchronized.
unsafe impl Send for AuHandle {}

impl AuHandle {
    /// Instantiate a component, returning a handle that owns the lifetime.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent` obtained from
    /// `AudioComponentFindNext` (directly or via [`crate::component`]).
    ///
    /// # Errors
    /// Returns [`AuError::NullComponent`] if `component` is null, or
    /// [`AuError::OsStatus`] if `AudioComponentInstanceNew` fails.
    pub unsafe fn new(component: AudioComponent) -> Result<Self> {
        if component.is_null() {
            return Err(AuError::NullComponent);
        }

        // The component came from `AudioComponentFindNext`, so describing it
        // must not fail. Swallowing a failure here would silently misclassify
        // the AU as `Unknown(0)`, breaking MIDI routing / type-gated behavior
        // downstream — surface it as an error instead.
        //
        // Read *before* instantiating, because the flags say whether this entry
        // point can instantiate at all.
        let mut desc = AudioComponentDescription::default();
        check(
            "AudioComponentGetDescription",
            AudioComponentGetDescription(component, &mut desc),
        )
        .map_err(|e| AuError::load_failed("<undescribed>", LoadStage::Opening, e.to_string()))?;

        // `AudioComponent.h:498-502`: `AudioComponentInstantiate` "must be used
        // to instantiate any component with
        // kAudioComponentFlag_RequiresAsyncInstantiation set". The system sets
        // that flag for v3 audio units with views.
        //
        // Refusing here rather than letting the call fail turns
        // `kAudioUnitErr_CannotDoInCurrentContext` (-10863) — which reads like
        // a transient condition worth retrying — into a statement about the
        // component. Measured on macOS 15.6: of 138 installed components, 5 set
        // the flag and all 5 return -10863 from the synchronous call.
        if desc.componentFlags & K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC_INSTANTIATION != 0 {
            return Err(AuError::RequiresAsyncInstantiation);
        }

        let mut instance: AudioComponentInstance = std::ptr::null_mut();
        check(
            "AudioComponentInstanceNew",
            AudioComponentInstanceNew(component, &mut instance),
        )
        .map_err(|e| {
            AuError::load_failed(
                component_triple(&desc),
                LoadStage::Instantiation,
                e.to_string(),
            )
        })?;
        let au_type = AuType::from_raw(desc.componentType);

        Ok(Self {
            instance,
            component,
            au_type,
        })
    }

    /// Raw `AudioUnit` pointer for passing to AudioToolbox APIs.
    pub fn raw_unit(&self) -> AudioUnit {
        self.instance
    }

    /// Raw factory handle (the `AudioComponent` this instance was created from).
    pub fn component(&self) -> AudioComponent {
        self.component
    }

    /// High-level type classification cached at creation time.
    pub fn au_type(&self) -> AuType {
        self.au_type
    }

    /// Copy the AU's display name. Returns `"<unknown>"` on failure.
    pub fn get_name(&self) -> String {
        unsafe {
            let mut name_ref: coreaudio_sys::CFStringRef = std::ptr::null();
            let status = AudioComponentCopyName(self.component, &mut name_ref);
            if status != NO_ERR {
                return String::from("<unknown>");
            }
            CfString::from_copied(name_ref)
                .map(|s| s.to_string())
                .unwrap_or_else(|| String::from("<unknown>"))
        }
    }
}

impl Drop for AuHandle {
    fn drop(&mut self) {
        unsafe {
            AudioComponentInstanceDispose(self.instance);
        }
    }
}

/// A component's identity as its decoded `type/subtype/manufacturer` triple —
/// `"aufx/dely/appl"`.
///
/// This is what [`AuError::LoadFailed`] carries in place of the path the other
/// three host crates report, because an AU is addressed by an OS-registered
/// component rather than by a file. Four-char codes are decoded because that is
/// the form a user can match against a plugin list; the registry's own
/// comparisons stay on the raw codes.
fn component_triple(desc: &AudioComponentDescription) -> String {
    format!(
        "{}/{}/{}",
        fourcc_to_string(desc.componentType),
        fourcc_to_string(desc.componentSubType),
        fourcc_to_string(desc.componentManufacturer)
    )
}

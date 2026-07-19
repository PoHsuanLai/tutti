//! Owned handle to an instantiated (but not necessarily initialized) AU.

#![cfg(target_os = "macos")]

use crate::cf::CfString;
use crate::component::AuType;
use crate::error::{AuError, Result};
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

        let mut instance: AudioComponentInstance = std::ptr::null_mut();
        check(
            "AudioComponentInstanceNew",
            AudioComponentInstanceNew(component, &mut instance),
        )?;

        let mut desc = AudioComponentDescription::default();
        let _ = AudioComponentGetDescription(component, &mut desc);
        let au_type = AuType::from_raw(desc.component_type);

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
            let mut name_ref: core_foundation_sys::string::CFStringRef = std::ptr::null();
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

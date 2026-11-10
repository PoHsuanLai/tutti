//! Owned handle to an instantiated (but not necessarily initialized) AU.

#![cfg(target_os = "macos")]

use tutti_au_ffi::{ComponentRef, Unit};

use crate::component::AuType;
use crate::error::{AuError, LoadStage, Result};
use crate::types::*;

/// RAII wrapper around an `AudioComponentInstance`.
///
/// Dropping the handle disposes the instance once nothing else (a listener, a
/// render notification) is still registered on it. The handle is `Send` but not
/// `Sync`; all AU property/parameter access serializes through the owning
/// thread.
pub struct AuHandle {
    unit: Unit,
    component: ComponentRef,
    au_type: AuType,
}

impl std::fmt::Debug for AuHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuHandle")
            .field("au_type", &self.au_type)
            .finish_non_exhaustive()
    }
}

impl AuHandle {
    /// Instantiates a component, returning a handle that owns the lifetime.
    ///
    /// # Errors
    /// Returns [`AuError::RequiresAsyncInstantiation`] for a v3 unit that must
    /// be created asynchronously, or [`AuError::LoadFailed`] if the component
    /// cannot be described or instantiated.
    pub fn new(component: ComponentRef) -> Result<Self> {
        // The component came from the registry, so describing it must not fail.
        // Swallowing a failure here would silently misclassify the AU as
        // `Unknown(0)`, breaking MIDI routing / type-gated behavior downstream
        // — surface it as an error instead.
        //
        // Read *before* instantiating, because the flags say whether this entry
        // point can instantiate at all.
        let desc = component.description().map_err(|e| {
            AuError::load_failed("<undescribed>", LoadStage::Opening, e.to_string())
        })?;

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

        let unit = Unit::new(component).map_err(|e| {
            AuError::load_failed(
                component_triple(&desc),
                LoadStage::Instantiation,
                e.to_string(),
            )
        })?;
        let au_type = AuType::from_raw(desc.componentType);

        Ok(Self {
            unit,
            component,
            au_type,
        })
    }

    /// The instance, for the safe AudioToolbox calls on it.
    pub fn unit(&self) -> &Unit {
        &self.unit
    }

    /// The component this instance was created from.
    pub fn component(&self) -> ComponentRef {
        self.component
    }

    /// High-level type classification cached at creation time.
    pub fn au_type(&self) -> AuType {
        self.au_type
    }

    /// Copies the AU's display name. Returns `"<unknown>"` on failure.
    pub fn get_name(&self) -> String {
        self.component
            .name()
            .unwrap_or_else(|| String::from("<unknown>"))
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

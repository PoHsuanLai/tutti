//! Components in the system registry.

use coreaudio_sys as sys;

use crate::cf::CfString;
use crate::error::{OsError, check};

/// An `AudioComponent` the registry holds: one installed Audio Unit.
///
/// The only way to get one is to ask the registry ([`ComponentRef::find_next`],
/// [`ComponentRef::first_matching`]), so it always came from
/// `AudioComponentFindNext`. It is `Copy` and names the component, not an
/// instance; [`Unit::new`](crate::Unit::new) makes the instance.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentRef(sys::AudioComponent);

impl std::fmt::Debug for ComponentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ComponentRef").field(&self.0).finish()
    }
}

// SAFETY: an `AudioComponent` is an opaque pointer to a record of the
// process-wide component registry. The registry owns the record, keeps it for
// the life of the process (a component is never freed, only an instance of
// one), and AudioToolbox documents its lookup and description calls as
// callable from any thread. The value is only ever passed back to those calls.
unsafe impl Send for ComponentRef {}
// SAFETY: as above; the value is immutable and every call that takes it reads
// the registry through AudioToolbox's own locking.
unsafe impl Sync for ComponentRef {}

impl ComponentRef {
    /// The next registered component matching `desc` after `after` (the first
    /// when `after` is `None`), or `None` at the end of the list. A zero field
    /// of `desc` matches anything.
    pub fn find_next(
        after: Option<ComponentRef>,
        desc: &sys::AudioComponentDescription,
    ) -> Option<ComponentRef> {
        let after = after.map_or(std::ptr::null_mut(), |c| c.0);
        // SAFETY: `after` is null or a component the registry gave us, and
        // `desc` is a live description for the call.
        let next = unsafe { sys::AudioComponentFindNext(after, desc) };
        (!next.is_null()).then_some(ComponentRef(next))
    }

    /// The first registered component matching `desc`.
    pub fn first_matching(desc: &sys::AudioComponentDescription) -> Option<ComponentRef> {
        Self::find_next(None, desc)
    }

    /// Every registered component matching `desc`, in registry order.
    pub fn all_matching(desc: &sys::AudioComponentDescription) -> Vec<ComponentRef> {
        let mut out = Vec::new();
        let mut at = None;
        while let Some(next) = Self::find_next(at, desc) {
            out.push(next);
            at = Some(next);
        }
        out
    }

    /// The component's description: its type, subtype, manufacturer and flags.
    ///
    /// # Errors
    /// The `OSStatus` of `AudioComponentGetDescription`.
    pub fn description(self) -> Result<sys::AudioComponentDescription, OsError> {
        let mut desc = sys::AudioComponentDescription::default();
        // SAFETY: `self.0` is a registry component and `desc` is writable.
        let status = unsafe { sys::AudioComponentGetDescription(self.0, &mut desc) };
        check("AudioComponentGetDescription", status)?;
        Ok(desc)
    }

    /// The component's display name, or `None` when the registry has none.
    pub fn name(self) -> Option<String> {
        let mut name: sys::CFStringRef = std::ptr::null();
        // SAFETY: `self.0` is a registry component and `name` is writable.
        let status = unsafe { sys::AudioComponentCopyName(self.0, &mut name) };
        check("AudioComponentCopyName", status).ok()?;
        // SAFETY: `AudioComponentCopyName` follows the Copy rule: on success
        // `name` is null or a CFString we own one retain of.
        unsafe { CfString::from_copied(name) }.map(|s| s.to_string())
    }

    /// The component's version word (`major << 16 | minor << 8 | bugfix`).
    pub fn version(self) -> Option<u32> {
        let mut raw: u32 = 0;
        // SAFETY: `self.0` is a registry component and `raw` is writable.
        let status = unsafe { sys::AudioComponentGetVersion(self.0, &mut raw) };
        check("AudioComponentGetVersion", status).ok().map(|()| raw)
    }

    /// The raw `AudioComponent`, for a call this crate does not wrap. Using the
    /// pointer is the caller's `unsafe`.
    pub fn as_raw(self) -> sys::AudioComponent {
        self.0
    }

    pub(crate) fn raw(self) -> sys::AudioComponent {
        self.0
    }
}

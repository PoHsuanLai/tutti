//! Helper for dereferencing optional CLAP extension pointers.
//!
//! Extension pointers are null when the plugin does not implement the
//! extension. [`opt`] collapses the null-check + deref into a single
//! expression that chains with `?`.

/// Dereference a cached extension pointer to a shared reference, or `None`
/// if the plugin does not implement that extension.
///
/// # Safety
/// The caller must ensure the extension pointer outlives the returned
/// reference. In practice this holds for any `&self` borrow on
/// [`ClapInstance`](super::ClapInstance), since the extension pointers are
/// owned by the plugin and the plugin outlives `self`.
pub(crate) unsafe fn opt<'a, T>(ptr: *const T) -> Option<&'a T> {
    if ptr.is_null() {
        None
    } else {
        Some(&*ptr)
    }
}

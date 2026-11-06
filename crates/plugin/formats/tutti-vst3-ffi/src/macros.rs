//! Crate-private macros.

/// `impl Debug` that names the type and hides its fields, for the COM objects
/// and handles whose state is not worth printing.
macro_rules! opaque_debug {
    ($ty:ty) => {
        opaque_debug!($ty, stringify!($ty));
    };
    ($ty:ty, $name:expr) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct($name).finish_non_exhaustive()
            }
        }
    };
}

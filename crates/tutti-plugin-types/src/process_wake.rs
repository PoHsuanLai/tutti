//! [`ProcessWake`]: what a plugin's request to be processed again raises.

use std::fmt;
use std::sync::Arc;

/// The host's answer to a plugin asking to be processed again (CLAP's
/// `request_process`): a handle the format host raises from the thread the
/// plugin called on, and whoever bound it acts on.
///
/// A host that parks a plugin which reported it has nothing to render binds
/// one through [`Control::bind_wake`](crate::Control::bind_wake). `raise`
/// allocates nothing and takes no lock, so it is safe from any thread, an
/// audio callback included; what the bound closure does is the binder's, and
/// must be as cheap.
#[derive(Clone)]
pub struct ProcessWake(Arc<dyn Fn() + Send + Sync>);

impl ProcessWake {
    /// A wake that runs `raise` each time the plugin asks. Control thread:
    /// allocates.
    pub fn new(raise: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(raise))
    }

    /// The plugin asked to be processed again. Any thread; wait-free when the
    /// bound closure is.
    pub fn raise(&self) {
        (self.0)();
    }
}

impl fmt::Debug for ProcessWake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessWake").finish_non_exhaustive()
    }
}

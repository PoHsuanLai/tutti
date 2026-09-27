//! The first error an offline voice hit, latched for its fork's health probe.

use std::error::Error;
use std::sync::{Arc, OnceLock};

/// The first error latched into it, kept: whether a voice rendering offline
/// has failed in a way its output cannot say. The disk voice latches one, and
/// its fork hands the graph a `tutti_graph::ForkHealth` probe
/// ([`VoiceHealth`](super::disk_voice::VoiceHealth)) that reads it. Read on
/// the control thread, after or between rendered spans, never from the audio
/// path.
///
/// Latching takes no lock and, after the first, allocates nothing (a later
/// error is dropped unboxed), but the first boxes its error: latch from a
/// thread that may allocate, which an offline render's is.
///
/// Until design doc 013 Phase 5 this was `tutti-node`'s, beside a
/// `RenderFault` trait it was the only implementor of; the trait went with
/// that crate, and the latch moved to its one user.
#[derive(Default)]
pub(crate) struct FaultLatch(OnceLock<Arc<dyn Error + Send + Sync>>);

impl FaultLatch {
    /// Keep `error` unless an earlier one is already kept.
    pub(crate) fn latch(&self, error: impl Error + Send + Sync + 'static) {
        if self.0.get().is_none() {
            let _ = self.0.set(Arc::new(error));
        }
    }

    /// `None` while the voice renders what it describes; the first failure
    /// otherwise. Once failed, it stays failed.
    pub(crate) fn fault(&self) -> Option<Arc<dyn Error + Send + Sync>> {
        self.0.get().cloned()
    }
}

impl core::fmt::Debug for FaultLatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0.get() {
            Some(e) => write!(f, "FaultLatch({e})"),
            None => f.write_str("FaultLatch(ok)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct E(&'static str);
    impl core::fmt::Display for E {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for E {}

    /// **The first error is kept**: a later one does not replace it, and an
    /// untouched latch reports nothing.
    ///
    /// Mutation (run): drop the `is_none` guard and `set` unconditionally →
    /// still passes (`OnceLock::set` refuses a second value), so the guard is
    /// what spares a later error its box, not what keeps the first; the
    /// property pinned here is `OnceLock`'s. Mutation (run): `fault` returns
    /// `None` always → fails.
    #[test]
    fn the_first_error_is_kept() {
        let l = FaultLatch::default();
        assert!(l.fault().is_none());
        l.latch(E("first"));
        l.latch(E("second"));
        assert_eq!(l.fault().expect("latched").to_string(), "first");
    }
}

//! How many read errors in a row a reader puts up with.
//!
//! The input pump and the device watch both read a non-blocking client until it
//! is empty, then wait on `tutti_alsa_ffi::Poller`.

/// Read errors in a row a reader puts up with before it stops.
pub(super) const MAX_DRAIN_ERRORS: usize = 8;

/// Read errors in a row, which end a reader at [`MAX_DRAIN_ERRORS`]. A read that
/// succeeds starts the count again, so scattered overruns in a long burst do
/// not end the reader with events unread.
#[derive(Debug, Default)]
pub(super) struct ErrorRun(usize);

impl ErrorRun {
    /// Counts an error; true when the run has reached the limit.
    pub(super) fn failed(&mut self) -> bool {
        self.0 += 1;
        self.0 >= MAX_DRAIN_ERRORS
    }

    /// A read succeeded.
    pub(super) fn read(&mut self) {
        self.0 = 0;
    }
}

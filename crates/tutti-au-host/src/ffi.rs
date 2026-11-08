//! The host's view of an `OSStatus`.

#![cfg(target_os = "macos")]

use crate::error::{AuError, Result};
use crate::types::*;

/// Turn a raw `OSStatus` into a `Result`, attaching the calling function name
/// for diagnostics on failure.
pub(crate) fn check(function: &'static str, status: OSStatus) -> Result<()> {
    if status == NO_ERR {
        Ok(())
    } else {
        Err(AuError::OsStatus {
            function,
            code: status,
        })
    }
}

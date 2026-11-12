//! Test-only support for this crate's unit tests: the signal helpers and the
//! network editors.

mod networks;
mod signals;

pub(crate) use networks::map_absorption;
pub(crate) use signals::{amplitude_at, lock_in};

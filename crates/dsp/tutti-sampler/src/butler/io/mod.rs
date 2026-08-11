//! Butler-thread disk I/O: reading wave data into the ring.
//!
//! Strictly the read path — [`refill`] decides how much to move and moves it,
//! [`wave_io`] unpacks a resident wave into interleaved frames. Loop-boundary
//! policy (`loops`) and stream preroll (`preroll`) are siblings, not members:
//! they reposition streams rather than read them.

pub(super) mod refill;
pub(super) mod wave_io;

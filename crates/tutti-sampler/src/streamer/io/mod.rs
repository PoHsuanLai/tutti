//! Streamer-thread disk I/O: reading wave data into the ring.
//!
//! Strictly the read path — [`refill`] decides how much to move and moves it,
//! [`wave_io`] unpacks a resident wave into interleaved frames. Loop-boundary
//! policy (`loops`) is a sibling, not a member: it repositions streams rather
//! than reads them.

pub(super) mod refill;
pub(super) mod wave_io;

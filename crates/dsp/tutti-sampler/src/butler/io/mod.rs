//! Butler-thread disk I/O: reading wave data into the ring.
//!
//! Just the two now. `loops` (loop-boundary policy), `pdc` (stream preroll,
//! renamed `preroll`), and `capture` (a WAV sink with no butler coupling at
//! all) used to be filed here too, none of them I/O.

pub(super) mod refill;
pub(super) mod wave_io;

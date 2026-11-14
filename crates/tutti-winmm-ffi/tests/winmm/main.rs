//! The winmm calls against the Windows this runs on: what a CI runner has
//! (no MIDI input, the built-in GS Wavetable Synth output). Every test here is
//! Windows-only, and a missing synth is a failure with its name, never a skip.

#[cfg(windows)]
mod gs_synth;
#[cfg(windows)]
mod sleeper;

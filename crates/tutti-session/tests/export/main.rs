//! Offline render through the session: bounce, cancel and the forks of every node kind.

// Stems: a node's own output beside the master, aligned to it, in one pass.
#[cfg(feature = "export")]
mod bounce_stems;
// Stopping an export and reading its progress (`BounceHandle`).
#[cfg(feature = "export")]
mod export_cancel;
// What an export renders and forks: the graph's own latency and tail, a
// prepare hook's edit and timeline, a fork's isolation from live playback,
// and where a fork is built and dropped (the session halves of the
// `bevy-tutti` export suites).
#[cfg(feature = "export")]
mod export_fork;
// A parallel export is bit-identical to the serial one.
#[cfg(feature = "export")]
mod export_parallel;
// A sampler voice (memory and disk) exported through a fork.
#[cfg(all(feature = "export", feature = "sampler"))]
mod export_fork_sampler;
// A SoundFont, a polysynth and their clip, exported through a fork.
mod dither_agrees;
#[cfg(all(
    feature = "export",
    feature = "midi",
    feature = "synth",
    feature = "soundfont"
))]
mod export_fork_synth;

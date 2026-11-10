//! In-process VST2 host.
//!
//! Pairs the halves of an activated `tutti_vst2_host::Vst2Loaded` with the same `PluginHandle` API the
//! out-of-process backends expose. The plugin's `Lifecycle` (doc 016 §2.6) runs
//! on the control side; its realtime `Processor` is lent to the audio thread
//! through a `tutti_rt::handoff`, which takes no lock. A block with no
//! Processor in hand (one recalled for a rate change, or refused its rate)
//! renders silence and is counted (`unlent_blocks`).

mod client;
mod control_backend;
mod fork;
mod instance;
mod loader;
mod node;

pub use client::{BoundInProcessVst2Client, InProcessVst2Client};
pub use loader::{load_client, load_client_restoring};
pub use node::InProcessVst2Controls;

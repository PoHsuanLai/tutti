// Forked from vst-rs 0.3.0. This is *owned* code, not a read-only vendor drop:
// the fork already diverges deliberately (host-side audioMaster callbacks, plus
// the host-safety fixes documented in `host.rs`), so its lints are chased like
// any other crate here. The blanket `allow(clippy::all)` that used to sit at the
// top of this file is what kept those bugs invisible.
//
// The remaining allows are narrow and each has a reason:
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![allow(missing_docs)]
// Elided-lifetime style across a 2021-edition port of a 2018-edition crate.
#![allow(mismatched_lifetime_syntaxes)]
//
// `allow(useless_ptr_null_checks)` used to sit here, justified as "the null
// checks on FFI function pointers in `host.rs` are load-bearing even though
// rustc believes an `extern "C" fn` is non-null by type". rustc was right and
// the justification was backwards: the checks compiled to `false` under `-O`,
// so the guard shipped only in debug builds while release jumped to address 0.
// The nullable slots are `Option<...>` now and the lint has nothing left
// to fire on — if it ever fires again, that is a real bug, not noise.

//! The host half of the VST2.4 API: the ABI types, the plugin loader and the host callback.
//!
//! Every VST2 `unsafe` lives here (`scripts/check-plugin-ffi.sh`).
//!
//! The VST API is multi-threaded. A VST host calls into a plugin generally from two threads -
//! the *processing* thread and the *UI* thread. The organization of this crate reflects this
//! structure to ensure that the threading assumptions of Safe Rust are fulfilled and data
//! races are avoided.
//!
//! # Plugins
//! The plugin-authoring half (`plugin_main!`, the `AEffect` builder and the
//! plugin-side dispatcher) lives in the test-only `tutti-vst2-author`; this crate
//! holds the `Plugin` and `PluginParameters` traits the host drives a loaded
//! plugin through, and the ABI types both sides share.
//!
//! # Hosts
//!
//! ## `Host` Trait
//! All hosts must implement the [`Host` trait](host/trait.Host.html). To load a VST plugin, you
//! need to wrap your host in an `Arc<T>` wrapper. Along with the
//! plugin path, this can be passed to the [`PluginLoader::load`] method to create a plugin loader
//! which can spawn plugin instances.
//!
//! ## Example Host
//! ```no_run
//! extern crate tutti_vst2_ffi as vst;
//!
//! use std::sync::Arc;
//! use std::path::Path;
//!
//! use tutti_vst2_ffi::host::{Host, PluginLoader};
//! use tutti_vst2_ffi::plugin::Plugin;
//!
//! struct SampleHost;
//!
//! impl Host for SampleHost {
//!     fn automate(&self, index: i32, value: f32) {
//!         println!("Parameter {} had its value changed to {}", index, value);
//!     }
//! }
//!
//! fn main() {
//!     let host = Arc::new(SampleHost);
//!     let path = Path::new("/path/to/vst");
//!
//!     let mut loader = PluginLoader::load(path, host.clone()).unwrap();
//!     let mut instance = loader.instance().unwrap();
//!
//!     println!("Loaded {}", instance.get_info().name);
//!
//!     instance.init();
//!     println!("Initialized instance!");
//!
//!     println!("Closing instance...");
//!     // Not necessary as the instance is shut down when it goes out of scope anyway.
//!     // drop(instance);
//! }
//!
//! ```
//!
//! [`PluginLoader::load`]: host/struct.PluginLoader.html#method.load
//!

extern crate libc;
extern crate libloading;
extern crate num_enum;
extern crate num_traits;
#[macro_use]
extern crate log;
#[macro_use]
extern crate bitflags;

pub mod api;
pub mod buffer;
pub mod channels;
pub mod control_cell;
pub mod editor;
pub mod event;
pub mod host;
pub mod interfaces;
pub mod midi;
pub mod plugin;
pub mod scratch;

// Test-only global allocator for the RT-safety test of the render refusal; it aborts on
// any allocation inside an `assert_no_alloc` scope.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

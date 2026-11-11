//! This plugin's exported probe symbols, as typed safe calls (feature
//! `probe`), for the suites that load it.
//!
//! The plugin keeps its switches and captures in statics of its cdylib. A test reaches the ones the host under test sees
//! through the cdylib's own image: the rlib linked for its types is a separate
//! copy with separate statics. [`ClapProbe`] opens the cdylib and calls each
//! `#[no_mangle] extern "C"` export through a method that carries the export's
//! exact signature, so no caller names a symbol or assumes a signature.
//!
//! The images are opened once and never closed: dropping a handle would
//! decrement the refcount of a library a live instance may be using, and a
//! switch set before a load must stay resident for the instance loaded after
//! it.

use std::path::Path;

use libloading::Library;

use crate::{ProcessCapture, ThreadCapture, ThreadPoolCapture};

/// The plugin's `#[no_mangle] unsafe extern "C" fn(u32)` switches: one
/// signature, one name each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClapSwitch {
    /// [`RenderMode`](crate::RenderMode).
    RenderMode,
    /// [`StatusMode`](crate::StatusMode).
    StatusMode,
    /// [`PortLayoutMode`](crate::PortLayoutMode).
    PortLayout,
    /// The `PORTS_64BIT_*` mask.
    Ports64Bit,
    /// The `audio-ports.rescan` flags to request at the next `on_main_thread`.
    RequestPortsRescan,
    /// Extra latency frames from the next activation.
    LatencyAtActivation,
    /// Whether `activate` announces `latency.changed()` (0 or 1).
    LatencyChangedInActivate,
    /// Whether the plugin offers `clap.tail` (0 or 1).
    TailExtension,
}

impl ClapSwitch {
    /// The export's name, NUL-terminated. Every one is, in this crate,
    /// `#[no_mangle] pub unsafe extern "C" fn(u32)`
    /// (`lib.rs`: `set_render_mode`, `set_port_layout`, `set_ports_64bit`,
    /// `request_ports_rescan`, `set_latency_changed_in_activate`,
    /// `set_latency_at_activation`, `set_tail_extension`; `rt_probe.rs`:
    /// `set_status_mode`).
    fn symbol(self) -> &'static [u8] {
        match self {
            Self::RenderMode => b"tutti_test_plugin_set_render_mode\0",
            Self::StatusMode => b"tutti_test_plugin_set_status_mode\0",
            Self::PortLayout => b"tutti_test_plugin_set_port_layout\0",
            Self::Ports64Bit => b"tutti_test_plugin_set_ports_64bit\0",
            Self::RequestPortsRescan => b"tutti_test_plugin_request_ports_rescan\0",
            Self::LatencyAtActivation => b"tutti_test_plugin_set_latency_at_activation\0",
            Self::LatencyChangedInActivate => {
                b"tutti_test_plugin_set_latency_changed_in_activate\0"
            }
            Self::TailExtension => b"tutti_test_plugin_set_tail_extension\0",
        }
    }
}

/// The reference CLAP plugin, opened for its probe symbols.
///
/// Switches reach every image of the plugin a test loads: the cargo artifact
/// and its `.clap` alias (`tutti_fixtures::clap_probe`). On unix the alias
/// is a symlink, so both opens reach one image; on Windows it is a copy, which
/// the loader maps as a second module with its own statics. Captures and the
/// counters are read from the cargo artifact, the image the suites load by
/// that path.
#[derive(Debug)]
pub struct ClapProbe {
    artifact: Library,
    dot_clap: Library,
}

impl ClapProbe {
    /// Opens the plugin's two images: the cargo artifact
    /// (`tutti_fixtures::cdylib`) and its `.clap` alias
    /// (`tutti_fixtures::clap_probe`). The caller keeps the probe for the
    /// rest of the process (see the module).
    ///
    /// # Panics
    ///
    /// If either image does not open.
    #[must_use]
    pub fn open(artifact: &Path, dot_clap: &Path) -> Self {
        let open = |path: &Path| {
            // SAFETY: the path is this crate's cdylib, which the caller's
            // dependency built. Its initialisers are the plugin's own, which
            // only set statics.
            unsafe { Library::new(path) }.expect("reference plugin opens")
        };
        Self {
            artifact: open(artifact),
            dot_clap: open(dot_clap),
        }
    }

    /// Sets `switch` to `value` in every image of the plugin.
    pub fn set(&self, switch: ClapSwitch, value: u32) {
        for lib in [&self.artifact, &self.dot_clap] {
            call_u32(lib, switch.symbol(), value);
        }
    }

    /// `tutti_test_plugin_thread_command(cmd)` in every image: run a
    /// `tutti_clap_test_plugin::CMD_*` at the next legal call site. The
    /// plugin's reply is not read.
    pub fn thread_command(&self, cmd: u32) {
        for lib in [&self.artifact, &self.dot_clap] {
            // SAFETY: `tutti_test_plugin_thread_command` is `#[no_mangle] pub
            // extern "C" fn(cmd: u32) -> u32` in `threading.rs`.
            unsafe {
                let f =
                    get::<extern "C" fn(u32) -> u32>(lib, b"tutti_test_plugin_thread_command\0");
                f(cmd);
            }
        }
    }

    /// `tutti_test_plugin_param_command(cmd)` in every image: make a
    /// `host.params.rescan` as `PARAM_CMD_*` says. The plugin's reply is not
    /// read.
    pub fn param_command(&self, cmd: u32) {
        for lib in [&self.artifact, &self.dot_clap] {
            // SAFETY: `tutti_test_plugin_param_command` is `#[no_mangle] pub
            // extern "C" fn(cmd: u32) -> u32` in `params_state.rs`.
            unsafe {
                let f = get::<extern "C" fn(u32) -> u32>(lib, b"tutti_test_plugin_param_command\0");
                f(cmd);
            }
        }
    }

    /// Arms (or, with 0, disarms) the thread-pool probe to request `tasks`
    /// tasks, in the cargo artifact's image only: the one a loader opens by
    /// that path.
    pub fn set_thread_pool_tasks(&self, tasks: u32) {
        call_u32(
            &self.artifact,
            b"tutti_test_plugin_set_thread_pool_tasks\0",
            tasks,
        );
    }

    /// Makes `activate` refuse the rate whose `f64::to_bits` is `rate_bits`
    /// (`u64::MAX` lifts it).
    pub fn set_activate_refusal(&self, rate_bits: u64) {
        // SAFETY: `tutti_test_plugin_set_activate_refusal` is `#[no_mangle]
        // pub unsafe extern "C" fn(rate_bits: u64, frames: u32)` in
        // `refusal.rs`; 0 frames is the "any block size" the old call passed.
        unsafe {
            let f = get::<unsafe extern "C" fn(u64, u32)>(
                &self.artifact,
                b"tutti_test_plugin_set_activate_refusal\0",
            );
            f(rate_bits, 0);
        }
    }

    /// The rate of the plugin's most recent accepted `activate` since the last
    /// [`reset_activation`](Self::reset_activation), if any.
    pub fn last_accepted_rate(&self) -> Option<f64> {
        let (mut bits, mut frames) = (0u64, 0u32);
        // SAFETY: `tutti_test_plugin_last_accepted_activation` is
        // `#[no_mangle] pub unsafe extern "C" fn(*mut u64, *mut u32) -> bool`
        // in `refusal.rs`; both out-params are valid locals.
        let seen = unsafe {
            let f = get::<unsafe extern "C" fn(*mut u64, *mut u32) -> bool>(
                &self.artifact,
                b"tutti_test_plugin_last_accepted_activation\0",
            );
            f(&mut bits, &mut frames)
        };
        seen.then(|| f64::from_bits(bits))
    }

    /// How many times the plugin accepted `activate` since
    /// [`reset_activation`](Self::reset_activation).
    pub fn activation_count(&self) -> u32 {
        // SAFETY: `tutti_test_plugin_activate_accepts` is `#[no_mangle] pub
        // unsafe extern "C" fn() -> u32` in `refusal.rs`.
        unsafe {
            get::<unsafe extern "C" fn() -> u32>(
                &self.artifact,
                b"tutti_test_plugin_activate_accepts\0",
            )()
        }
    }

    /// Zeroes [`activation_count`](Self::activation_count) and the last
    /// accepted rate.
    pub fn reset_activation(&self) {
        call_unit(&self.artifact, b"tutti_test_plugin_reset_activation\0");
    }

    /// What the plugin recorded of its threading and timer calls since the
    /// last [`thread_reset`](Self::thread_reset).
    ///
    /// # Panics
    ///
    /// If the plugin reports no capture.
    pub fn thread_capture(&self) -> ThreadCapture {
        let mut cap = ThreadCapture::default();
        // SAFETY: `tutti_test_plugin_thread_capture` is `#[no_mangle] pub
        // unsafe extern "C" fn(out: *mut ThreadCapture) -> bool` in
        // `threading.rs`, and `cap` is a valid, exclusive `ThreadCapture`
        // (`#[repr(C)]`, the same type as the rlib's).
        let ok = unsafe {
            get::<unsafe extern "C" fn(*mut ThreadCapture) -> bool>(
                &self.artifact,
                b"tutti_test_plugin_thread_capture\0",
            )(&mut cap)
        };
        assert!(ok, "the plugin reports its thread capture");
        cap
    }

    /// Zeroes [`thread_capture`](Self::thread_capture).
    pub fn thread_reset(&self) {
        call_unit(&self.artifact, b"tutti_test_plugin_thread_reset\0");
    }

    /// What the plugin recorded of its last `process` call.
    ///
    /// # Panics
    ///
    /// If the plugin recorded no `process` call.
    pub fn process_capture(&self) -> ProcessCapture {
        let mut cap = ProcessCapture::default();
        // SAFETY: `tutti_test_plugin_capture` is `#[no_mangle] pub unsafe
        // extern "C" fn(out: *mut ProcessCapture) -> bool` in `lib.rs`, and
        // `cap` is a valid, exclusive `ProcessCapture` (`#[repr(C)]`).
        let ok = unsafe {
            get::<unsafe extern "C" fn(*mut ProcessCapture) -> bool>(
                &self.artifact,
                b"tutti_test_plugin_capture\0",
            )(&mut cap)
        };
        assert!(ok, "the plugin recorded a process() call");
        cap
    }

    /// What every `process` since the plugin's last reset saw of the
    /// thread-pool probe.
    pub fn thread_pool_capture(&self) -> ThreadPoolCapture {
        // SAFETY: `tutti_test_plugin_thread_pool_capture` is `#[no_mangle] pub
        // unsafe extern "C" fn() -> ThreadPoolCapture` in `thread_pool.rs`,
        // the same `#[repr(C)]` type as the rlib's.
        unsafe {
            get::<unsafe extern "C" fn() -> ThreadPoolCapture>(
                &self.artifact,
                b"tutti_test_plugin_thread_pool_capture\0",
            )()
        }
    }
}

/// Resolves `name` in `lib` as an `F`.
///
/// # Safety
///
/// `name` must be an export of the reference plugin defined with exactly the
/// signature `F`. Every caller in this module names the definition it relies
/// on.
unsafe fn get<F: Copy>(lib: &Library, name: &[u8]) -> F {
    // SAFETY: the caller's contract; the library is never closed, so the
    // pointer stays valid.
    let symbol: libloading::Symbol<F> =
        unsafe { lib.get(name) }.expect("export of the reference plugin");
    *symbol
}

/// A `fn(u32)` export of the plugin.
fn call_u32(lib: &Library, name: &[u8], value: u32) {
    // SAFETY: only [`ClapSwitch::symbol`]'s exports and
    // `tutti_test_plugin_set_thread_pool_tasks` reach here; each is
    // `#[no_mangle] pub unsafe extern "C" fn(u32)` in the plugin.
    unsafe {
        get::<unsafe extern "C" fn(u32)>(lib, name)(value);
    }
}

/// A `fn()` export of the plugin.
fn call_unit(lib: &Library, name: &[u8]) {
    // SAFETY: only `tutti_test_plugin_reset_activation` and
    // `tutti_test_plugin_thread_reset` reach here; each is `#[no_mangle] pub
    // [unsafe] extern "C" fn()` in the plugin.
    unsafe {
        get::<unsafe extern "C" fn()>(lib, name)();
    }
}

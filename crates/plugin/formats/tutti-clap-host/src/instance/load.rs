//! Construction of [`ClapLoaded`]: probe, load, and the editor-only entry
//! point, plus the port-layout / f64-support helpers they read off the freshly
//! instantiated plugin.

use super::config::{AudioConfig, LifecycleFlags, PortLayout};
use super::descriptor::{self, load_descriptor};
use super::ext;
use super::extensions::ExtensionCache;
use super::handle::PluginHandle;
use super::ClapLoaded;
use crate::error::{ClapError, LoadStage, Result};
use crate::host::{ClapHost, HostState};
use crate::types::PluginInfo;
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_SUPPORTS_64BITS,
};
use clap_sys::plugin::clap_plugin;
use std::path::Path;
use std::sync::Arc;

impl ClapLoaded {
    /// Lightweight probe: read the CLAP descriptor without creating or
    /// initializing the plugin instance.
    pub fn probe(bundle_path: &Path, library_path: Option<&Path>) -> Result<PluginInfo> {
        let load_path = library_path.unwrap_or(bundle_path);

        let library = unsafe {
            libloading::Library::new(load_path).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Failed to load library: {e}"),
            })?
        };

        let loaded = load_descriptor(&library, bundle_path)?;
        let info = loaded.info;
        drop(loaded.entry_guard); // drop order: guard before library
        drop(library);
        Ok(info)
    }

    /// Load a CLAP plugin from a path that is either a file or a bundle directory.
    pub fn load(path: impl AsRef<Path>, sample_rate: f64, max_frames: u32) -> Result<Self> {
        Self::load_with_library(path.as_ref(), None, sample_rate, max_frames)
    }

    /// Load a CLAP plugin with a pre-resolved library path.
    ///
    /// `bundle_path` is the original `.clap` bundle directory (passed to `init()`).
    /// `library_path` is the resolved binary for dlopen. If `None`, `bundle_path`
    /// is used for both.
    pub fn load_with_library(
        bundle_path: &Path,
        library_path: Option<&Path>,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<Self> {
        let load_path = library_path.unwrap_or(bundle_path);

        let library = unsafe {
            libloading::Library::new(load_path).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Failed to load library: {e}"),
            })?
        };

        let descriptor::LoadedDescriptor {
            entry_guard,
            factory_ptr,
            factory,
            info: mut plugin_info,
        } = load_descriptor(&library, bundle_path)?;

        let host_state = Arc::new(HostState::new());
        let host = Box::new(ClapHost::new(host_state.clone()));

        let plugin_id_cstr =
            std::ffi::CString::new(plugin_info.id.as_str()).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: format!("Invalid plugin ID: {e}"),
            })?;

        let create_fn = factory.create_plugin.ok_or_else(|| ClapError::LoadFailed {
            path: bundle_path.to_path_buf(),
            stage: LoadStage::Instantiation,
            reason: "No create_plugin function".to_string(),
        })?;

        let plugin_ptr = unsafe {
            create_fn(
                factory_ptr as *const _,
                host.as_raw(),
                plugin_id_cstr.as_ptr(),
            )
        };

        if plugin_ptr.is_null() {
            return Err(ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: "Failed to create plugin instance".to_string(),
            });
        }

        // H5: bind the raw pointer into its owning handle BEFORE the init
        // checks. `create_plugin` has already handed us ownership, so every
        // exit from here on must `destroy()` it — CLAP's spec is explicit: "If
        // init returns false, the host must destroy the plugin instance."
        // Previously both early returns below (missing `init`, `init` false)
        // dropped the raw pointer on the floor and then `dlclose`d the library
        // out from under a live instance. `PluginHandle::drop` now covers both.
        let plugin = PluginHandle::new(plugin_ptr);

        let plugin_init_fn =
            unsafe { plugin.as_ref() }
                .init
                .ok_or_else(|| ClapError::LoadFailed {
                    path: bundle_path.to_path_buf(),
                    stage: LoadStage::Initialization,
                    reason: "No plugin init function".to_string(),
                })?;

        if !unsafe { plugin_init_fn(plugin.as_ptr()) } {
            return Err(ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: "Plugin init failed".to_string(),
            });
        }

        let extensions = ExtensionCache::query(plugin.as_ptr());

        let mut ports = PortLayout {
            inputs: port_channels(plugin.as_ptr(), extensions.audio.ports, true),
            outputs: port_channels(plugin.as_ptr(), extensions.audio.ports, false),
        };

        plugin_info.audio_inputs = ports.input_channel_total().max(2);
        plugin_info.audio_outputs = ports.output_channel_total().max(2);

        if ports.inputs.is_empty() {
            ports.inputs.push(2);
        }
        if ports.outputs.is_empty() {
            ports.outputs.push(2);
        }

        let audio = AudioConfig {
            sample_rate,
            max_frames,
            supports_f64: check_f64_support(plugin.as_ptr(), extensions.audio.ports),
        };

        Ok(Self {
            plugin,
            _entry_guard: entry_guard,
            _library: library,
            _host: host,
            host_state,
            extensions,
            info: plugin_info,
            audio,
            ports,
            flags: LifecycleFlags::default(),
        })
    }

    /// Load a CLAP plugin for editor/parameter/state work only — never for
    /// audio. The returned instance must NOT be `activate()`d, `process()`d, or
    /// `start_processing()`d; doing so is a misuse of an editor-only load.
    ///
    /// CLAP's `gui`, `params`, and `state` extensions work without activation,
    /// so this skips the audio-config negotiation a processing load needs. The
    /// sample rate / max-frames passed to the plugin are placeholders that are
    /// never used (no `activate()` call consumes them). Use this in the
    /// in-process GUI host, where audio runs in a separate instance/process.
    pub fn load_editor_only(bundle_path: &Path, library_path: Option<&Path>) -> Result<Self> {
        // Placeholder audio config: never used because the caller must not
        // activate this instance. A processing load uses `load_with_library`
        // with the real sample rate / block size instead.
        const EDITOR_ONLY_SAMPLE_RATE: f64 = 44_100.0;
        const EDITOR_ONLY_MAX_FRAMES: u32 = 512;
        Self::load_with_library(
            bundle_path,
            library_path,
            EDITOR_ONLY_SAMPLE_RATE,
            EDITOR_ONLY_MAX_FRAMES,
        )
    }
}

/// Per-port channel counts for one side, read off `clap.audio-ports`.
///
/// # Why a hole truncates instead of being skipped
///
/// `audio-ports.h` declares `count()` as "Number of ports" and `get()` as
/// "Returns true on success" — a count, not the upper bound of a sparse index
/// space. There is no spec provision for `get(i)` failing at `i < count`, so a
/// plugin that does it is malformed. What matters is that the host's recovery
/// not be worse than stopping.
///
/// It was. This used to `filter_map`, which **skips** the hole and closes the
/// gap: with 5 ports and `get(3)` failing, the returned `Vec` had 4 entries and
/// port 4's channel count landed at index 3. That list is positional — it is
/// the *only* description of the port geometry, and
/// [`refill_port_buffers`](super::audio) walks it in order to carve a flat
/// pointer array into per-port `clap_audio_buffer` descriptors, advancing its
/// offset by each entry's channel count. So a skipped hole silently renumbers
/// every later port and hands the plugin channels belonging to its neighbour —
/// misrouted audio, with no error anywhere and geometry that still looks
/// self-consistent.
///
/// Truncating at the hole keeps the surviving list a true **prefix** of the
/// plugin's real port list: every port the host does present sits at its own
/// index carrying its own channel count, so nothing is ever misattributed. The
/// host presents fewer ports than the plugin declared, which CLAP already
/// tolerates (`process` carries explicit `audio_inputs_count` /
/// `audio_outputs_count`, and the caller pads or drops against them) — whereas
/// a wrong-width port at the wrong index is unrepresentable as anything but a
/// bug. Failing the whole load was the other candidate and is disproportionate:
/// a hole at index 0 would kill a plugin whose remaining ports are perfectly
/// describable, and truncation degrades to exactly that empty-list case on its
/// own when the hole *is* at 0.
fn port_channels(
    plugin: *const clap_plugin,
    audio_ports: *const clap_plugin_audio_ports,
    is_input: bool,
) -> Vec<u32> {
    let Some(ext) = (unsafe { ext::opt(audio_ports) }) else {
        return Vec::new();
    };
    let (count_fn, get_fn) = match (ext.count, ext.get) {
        (Some(c), Some(g)) => (c, g),
        _ => return Vec::new(),
    };
    let count = unsafe { count_fn(plugin, is_input) };
    let mut channels = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(plugin, i, is_input, &mut info) } {
            // Stop, don't skip: every later port's index would shift.
            break;
        }
        channels.push(info.channel_count);
    }
    channels
}

/// Whether any output port advertises `CLAP_AUDIO_PORT_SUPPORTS_64BITS`.
///
/// # Why this stops at a hole too
///
/// Same enumeration shape as [`port_channels`], but the consequence differs
/// and is worth naming, because "it's only a bool" is the reasoning that would
/// leave it unfixed. This result is not positional, so a hole cannot *shift*
/// anything — yet it must still stop at the same index, because the two
/// functions describe the same port list and are consumed together.
///
/// [`port_channels`] truncates at the hole, so ports past it are ones the host
/// has decided not to present at all. Scanning past the hole here would let a
/// port the host will never hand the plugin decide the sample format for the
/// ports it does — `activate::<f64>()` would then succeed on a claim made by a
/// port that is absent from the negotiated layout. Stopping keeps both reads
/// describing the same prefix, which is the only way the two stay consistent.
fn check_f64_support(
    plugin: *const clap_plugin,
    audio_ports: *const clap_plugin_audio_ports,
) -> bool {
    let Some(ext) = (unsafe { ext::opt(audio_ports) }) else {
        return false;
    };
    let (count_fn, get_fn) = match (ext.count, ext.get) {
        (Some(c), Some(g)) => (c, g),
        _ => return false,
    };
    let count = unsafe { count_fn(plugin, false) };
    let mut supports = false;
    for i in 0..count {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(plugin, i, false, &mut info) } {
            // Past this index `port_channels` presents no ports, so no port
            // past it may vote on the sample format.
            break;
        }
        if (info.flags & CLAP_AUDIO_PORT_SUPPORTS_64BITS) != 0 {
            supports = true;
        }
    }
    supports
}

#[cfg(test)]
mod enumeration_hole_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Index at which the stub's `get` reports failure. Process-global because
    /// the stub is a bare `extern "C"` fn with no state parameter — the same
    /// reason the reference plugin's switches are globals.
    static HOLE: AtomicU32 = AtomicU32::new(u32::MAX);
    /// Number of ports the stub's `count` reports.
    static COUNT: AtomicU32 = AtomicU32::new(0);

    /// Stub layout: port `i` has `i + 1` channels, and only the **last** port
    /// advertises 64-bit support.
    ///
    /// Both choices are load-bearing. Distinct widths make a shift visible in
    /// `port_channels`; putting the 64-bit flag last means a `check_f64_support`
    /// that scans past a hole reaches a different answer than one that stops,
    /// which is the whole distinction under test.
    unsafe extern "C" fn stub_count(_plugin: *const clap_plugin, _is_input: bool) -> u32 {
        COUNT.load(Ordering::SeqCst)
    }

    unsafe extern "C" fn stub_get(
        _plugin: *const clap_plugin,
        index: u32,
        _is_input: bool,
        info: *mut clap_audio_port_info,
    ) -> bool {
        if index == HOLE.load(Ordering::SeqCst) {
            return false;
        }
        let last = COUNT.load(Ordering::SeqCst).saturating_sub(1);
        (*info).channel_count = index + 1;
        (*info).flags = if index == last {
            CLAP_AUDIO_PORT_SUPPORTS_64BITS
        } else {
            0
        };
        true
    }

    fn stub_ext() -> clap_plugin_audio_ports {
        clap_plugin_audio_ports {
            count: Some(stub_count),
            get: Some(stub_get),
        }
    }

    /// Configure the stub and run `f`. Serialized by the harness running these
    /// in one binary; each call sets both globals, so no test inherits state.
    fn with_layout<R>(count: u32, hole: u32, f: impl FnOnce(&clap_plugin_audio_ports) -> R) -> R {
        COUNT.store(count, Ordering::SeqCst);
        HOLE.store(hole, Ordering::SeqCst);
        let ext = stub_ext();
        f(&ext)
    }

    /// The baseline: with no hole, every port is reported at its own index.
    ///
    /// Without this, a bug that dropped all ports unconditionally would satisfy
    /// the truncation assertions below for entirely the wrong reason.
    #[test]
    fn no_hole_reports_every_port() {
        let channels = with_layout(4, u32::MAX, |ext| {
            port_channels(std::ptr::null(), ext, false)
        });
        assert_eq!(channels, vec![1, 2, 3, 4]);
    }

    /// A hole must truncate, never renumber.
    ///
    /// The pre-fix `filter_map` returned `[1, 2, 4]` here: three ports, with
    /// port 3's four channels sitting at index 2 where a two-channel port
    /// belongs. `refill_port_buffers` slices a flat pointer array by these
    /// counts in order, so that list routes channels to the wrong ports.
    #[test]
    fn hole_truncates_the_port_list() {
        let channels = with_layout(4, 2, |ext| port_channels(std::ptr::null(), ext, false));
        assert_eq!(
            channels,
            vec![1, 2],
            "a hole at index 2 must yield the prefix [1, 2]; [1, 2, 4] means \
             the host skipped the hole and moved port 3 into index 2"
        );
    }

    /// A hole at index 0 leaves nothing describable.
    #[test]
    fn hole_at_zero_yields_no_ports() {
        let channels = with_layout(4, 0, |ext| port_channels(std::ptr::null(), ext, false));
        assert!(channels.is_empty());
    }

    /// `check_f64_support` must not let a port past the hole vote.
    ///
    /// Only the last port advertises 64-bit here, and the hole sits before it.
    /// `port_channels` therefore presents a layout that excludes that port
    /// entirely — so reporting 64-bit support would let `activate::<f64>()`
    /// succeed on a claim made by a port the plugin will never be handed.
    #[test]
    fn f64_support_ignores_ports_past_a_hole() {
        let supports = with_layout(4, 2, |ext| check_f64_support(std::ptr::null(), ext));
        assert!(
            !supports,
            "the only 64-bit-capable port (index 3) lies past the hole at \
             index 2, so it is absent from the presented layout and must not \
             decide the sample format"
        );
    }

    /// The complement: a 64-bit-capable port *inside* the surviving prefix
    /// still counts. Without this, `check_f64_support` returning a constant
    /// `false` would pass the test above.
    #[test]
    fn f64_support_still_sees_ports_before_a_hole() {
        // count = 1 → index 0 is the last port, so it carries the flag; the
        // hole at 1 is past the end of the surviving prefix.
        let supports = with_layout(1, 1, |ext| check_f64_support(std::ptr::null(), ext));
        assert!(
            supports,
            "port 0 advertises 64-bit and precedes the hole, so support must \
             still be reported"
        );
    }
}

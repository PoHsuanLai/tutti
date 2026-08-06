//! Construction of [`ClapLoaded`]: probe, load, and the editor-only entry
//! point, plus the port-layout / f64-support helpers they read off the freshly
//! instantiated plugin.

use super::config::{AudioConfig, LifecycleFlags, PortLayout};
use super::descriptor::{self, load_descriptor};
use super::ext;
use super::extensions::ExtensionCache;
use super::handle::PluginHandle;
use super::ports::layout_from_clap_port;
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
use tutti_plugin_types::BusChannels;
// Layout construction moved into `PortLayout::default_empty_buses`; the tests
// below still name the type directly to assert on canonicalized widths.
#[cfg(test)]
use tutti_plugin_types::ChannelLayout;

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

        let loaded = load_descriptor(&library, bundle_path, None)?;
        let info = loaded.info;
        drop(loaded.entry_guard); // drop order: guard before library
        drop(library);
        Ok(info)
    }

    /// Every plugin a `.clap` bundle advertises, not just the first.
    ///
    /// A bundle is a factory: `get_plugin_count` exists because one file may
    /// ship a synth plus companion effects. [`probe`](Self::probe) answers for
    /// the default (first) plugin, which is the whole answer for most bundles;
    /// this is how a caller finds the rest, and the ids it returns are what
    /// [`load_plugin`](Self::load_plugin) accepts.
    pub fn probe_all(bundle_path: &Path, library_path: Option<&Path>) -> Result<Vec<PluginInfo>> {
        let load_path = library_path.unwrap_or(bundle_path);

        let library = unsafe {
            libloading::Library::new(load_path).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Failed to load library: {e}"),
            })?
        };

        let loaded = load_descriptor(&library, bundle_path, None)?;
        let siblings = loaded.siblings;
        drop(loaded.entry_guard); // drop order: guard before library
        drop(library);
        Ok(siblings)
    }

    /// Load a CLAP plugin from a path that is either a file or a bundle directory.
    pub fn load(path: impl AsRef<Path>, sample_rate: f64, max_frames: u32) -> Result<Self> {
        Self::load_with_library(path.as_ref(), None, sample_rate, max_frames)
    }

    /// Load a named plugin from a bundle that ships more than one.
    ///
    /// `plugin_id` is an id from [`probe_all`](Self::probe_all). A bundle with
    /// a single plugin needs [`load`](Self::load), which takes the only one
    /// there is.
    pub fn load_plugin(
        path: impl AsRef<Path>,
        plugin_id: &str,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<Self> {
        Self::load_selected(
            path.as_ref(),
            None,
            Some(plugin_id),
            sample_rate,
            max_frames,
        )
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
        Self::load_selected(bundle_path, library_path, None, sample_rate, max_frames)
    }

    /// The one load path. `plugin_id` `None` means "the bundle's first plugin",
    /// which is what every single-plugin bundle wants and what this crate did
    /// unconditionally before multi-plugin bundles were reachable.
    pub fn load_selected(
        bundle_path: &Path,
        library_path: Option<&Path>,
        plugin_id: Option<&str>,
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
            // The load path needs the selected plugin, not the bundle's roster;
            // `probe_all` is where a caller reads that.
            siblings: _,
        } = load_descriptor(&library, bundle_path, plugin_id)?;

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

        // Default the bus lists FIRST, then derive the totals from them, so the
        // two cannot disagree. See `PortLayout::default_empty_buses` for why the
        // old `.max(2)` on the totals was the wrong shape.
        ports.default_empty_buses();

        plugin_info.audio_inputs = ports.input_channel_total();
        plugin_info.audio_outputs = ports.output_channel_total();

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
/// A `get(i)` failure at `i < count` is malformed — CLAP has no sparse index
/// space — and **truncates** rather than skipping. The returned list is
/// positional: [`refill_port_buffers`](super::audio) walks it in order,
/// advancing its offset by each entry's channel count. A `filter_map` (what
/// this used to be) closes the gap, so every later port silently moves down one
/// index and gets handed its neighbour's channels. Truncating keeps the list a
/// true prefix, which CLAP tolerates — `process` carries explicit
/// `audio_inputs_count` / `audio_outputs_count`.
fn port_channels(
    plugin: *const clap_plugin,
    audio_ports: *const clap_plugin_audio_ports,
    is_input: bool,
) -> BusChannels {
    let Some(ext) = (unsafe { ext::opt(audio_ports) }) else {
        return BusChannels::new();
    };
    let (count_fn, get_fn) = match (ext.count, ext.get) {
        (Some(c), Some(g)) => (c, g),
        _ => return BusChannels::new(),
    };
    let count = unsafe { count_fn(plugin, is_input) };
    let mut channels = BusChannels::with_capacity(count as usize);
    for i in 0..count {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_fn(plugin, i, is_input, &mut info) } {
            // Stop, don't skip: every later port's index would shift.
            break;
        }
        // Same FFI-inbound conversion `audio_port_info` uses, so a port's
        // stored layout and its reported layout cannot disagree. The
        // `port_type` tag is what makes `Mono`/`Stereo` named rather than
        // inferred from the width.
        channels.push(layout_from_clap_port(info.port_type, info.channel_count));
    }
    channels
}

/// Whether any output port advertises `CLAP_AUDIO_PORT_SUPPORTS_64BITS`.
///
/// Stops at a `get` failure for the same index as [`port_channels`], though
/// nothing here is positional: ports past the hole are absent from the
/// presented layout, so letting one vote would make `activate::<f64>()` succeed
/// on a claim by a port the plugin is never handed.
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
    use std::sync::Mutex;

    /// Index at which the stub's `get` reports failure. Global because the stub
    /// is a bare `extern "C"` fn with no state parameter.
    static HOLE: AtomicU32 = AtomicU32::new(u32::MAX);
    /// Number of ports the stub's `count` reports.
    static COUNT: AtomicU32 = AtomicU32::new(0);
    /// Serializes [`with_layout`]. The globals above are process-wide, every
    /// test configures them differently, and cargo runs the tests on parallel
    /// threads: unserialized, one test's `store` lands inside another's `f` and
    /// the suite fails on roughly 45% of runs.
    static LAYOUT_LOCK: Mutex<()> = Mutex::new(());

    /// Stub layout: port `i` has `i + 1` channels, and only the **last** port
    /// advertises 64-bit support. Distinct widths make an index shift visible;
    /// the flag being last is what makes scanning-past differ from stopping.
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

    /// Configure the stub and run `f` under [`LAYOUT_LOCK`], so the store and
    /// the read that follows it cannot interleave with another test's.
    ///
    /// The lock is taken poison-tolerant: a failing assertion inside `f` panics
    /// with the guard held, and propagating that as a `PoisonError` would turn
    /// one real failure into four misleading ones in the other tests.
    fn with_layout<R>(count: u32, hole: u32, f: impl FnOnce(&clap_plugin_audio_ports) -> R) -> R {
        let _guard = LAYOUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        COUNT.store(count, Ordering::SeqCst);
        HOLE.store(hole, Ordering::SeqCst);
        let ext = stub_ext();
        f(&ext)
    }

    /// Baseline — without it, dropping all ports unconditionally would satisfy
    /// the truncation assertions below.
    #[test]
    fn no_hole_reports_every_port() {
        let channels = with_layout(4, u32::MAX, |ext| {
            port_channels(std::ptr::null(), ext, false)
        });
        // The stub leaves `port_type` null, so each layout is built from the
        // reported width alone — 1..=4 in port order.
        assert_eq!(
            channels.as_slice(),
            [
                ChannelLayout::MONO,
                ChannelLayout::STEREO,
                ChannelLayout::from(3u16),
                ChannelLayout::QUAD
            ]
        );
    }

    /// A hole must truncate, never renumber. The pre-fix `filter_map` returned
    /// `[1, 2, 4]` — port 3's width at index 2.
    #[test]
    fn hole_truncates_the_port_list() {
        let channels = with_layout(4, 2, |ext| port_channels(std::ptr::null(), ext, false));
        assert_eq!(
            channels.as_slice(),
            [ChannelLayout::MONO, ChannelLayout::STEREO],
            "a hole at index 2 must yield the prefix [Mono, Stereo]; a trailing \
             Quad means the host skipped the hole and moved port 3 (width 4) \
             into index 2"
        );
    }

    /// A hole at index 0 leaves nothing describable.
    #[test]
    fn hole_at_zero_yields_no_ports() {
        let channels = with_layout(4, 0, |ext| port_channels(std::ptr::null(), ext, false));
        assert!(channels.is_empty());
    }

    /// The only 64-bit-capable port is the last one, past the hole — so it is
    /// absent from the presented layout and must not vote.
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

    /// The complement — without it, a constant `false` would pass the test
    /// above.
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

//! Construction of [`ClapLoaded`]: probe, load, and the editor-only entry
//! point, plus the port-layout / f64-support helpers they read off the freshly
//! instantiated plugin.

use super::config::{AudioConfig, LifecycleFlags, PortLayout};
use super::descriptor::{self, load_descriptor};
use super::ext;
use super::extensions::ExtensionCache;
use super::plugin_ptr::PluginPtr;
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
// The tests below name the type directly to assert on canonicalized widths.
#[cfg(test)]
use tutti_plugin_types::ChannelLayout;

impl ClapLoaded {
    /// Reads the descriptor of the bundle's first plugin without creating an
    /// instance.
    ///
    /// `library_path` is the binary to open when it differs from
    /// `bundle_path`; `None` opens `bundle_path` itself.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened or has
    /// no compatible `clap_entry` or plugin factory.
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

    /// Returns the descriptor of every plugin a `.clap` bundle advertises.
    ///
    /// A bundle is a factory: `get_plugin_count` exists because one file may
    /// ship a synth plus companion effects. [`probe`](Self::probe) answers for
    /// the default (first) plugin, which is the whole answer for most bundles;
    /// this is how a caller finds the rest, and the ids it returns are what
    /// [`load_plugin`](Self::load_plugin) accepts.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened or has
    /// no compatible `clap_entry` or plugin factory.
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

    /// Loads and initializes the first plugin in a `.clap` bundle.
    ///
    /// `sample_rate` (Hz) and `max_frames` (the largest block
    /// [`process`](crate::ClapActive::process) will be asked for) are fixed
    /// here and used by [`activate`](Self::activate).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tutti_clap_host::ClapLoaded;
    ///
    /// let plugin = ClapLoaded::load("/usr/lib/clap/MyPlugin.clap", 48_000.0, 512)?;
    /// println!("{} {}", plugin.info().name, plugin.info().version);
    /// # Ok::<(), tutti_clap_host::ClapError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened, has no
    /// compatible `clap_entry` or plugin factory, or the plugin cannot be
    /// created or initialized; `stage` says which step failed.
    pub fn load(path: impl AsRef<Path>, sample_rate: f64, max_frames: u32) -> Result<Self> {
        Self::load_with_library(path.as_ref(), None, sample_rate, max_frames)
    }

    /// Loads a named plugin from a bundle that ships more than one.
    ///
    /// `plugin_id` is an id from [`probe_all`](Self::probe_all). A bundle with
    /// a single plugin needs [`load`](Self::load), which takes the only one
    /// there is.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened, has no
    /// compatible `clap_entry` or plugin factory, or the plugin cannot be
    /// created or initialized; `stage` says which step failed.
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

    /// Loads a CLAP plugin with a pre-resolved library path.
    ///
    /// `bundle_path` is the original `.clap` bundle directory (passed to `init()`).
    /// `library_path` is the resolved binary for dlopen. If `None`, `bundle_path`
    /// is used for both.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened, has no
    /// compatible `clap_entry` or plugin factory, or the plugin cannot be
    /// created or initialized; `stage` says which step failed.
    pub fn load_with_library(
        bundle_path: &Path,
        library_path: Option<&Path>,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<Self> {
        Self::load_selected(bundle_path, library_path, None, sample_rate, max_frames)
    }

    /// Loads a plugin with every option explicit: a pre-resolved library path
    /// and an optional plugin id.
    ///
    /// `plugin_id` `None` means the bundle's first plugin, which is what every
    /// single-plugin bundle wants. The other `load*` constructors call this.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened, has no
    /// compatible `clap_entry` or plugin factory, or the plugin cannot be
    /// created or initialized; `stage` says which step failed.
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

        // Bind the raw pointer into its owning handle BEFORE the init
        // checks. `create_plugin` has already handed us ownership, so every
        // exit from here on must `destroy()` it — CLAP's spec is explicit: "If
        // init returns false, the host must destroy the plugin instance."
        // Both early returns below (missing `init`, `init` false) would
        // otherwise drop the raw pointer on the floor and then `dlclose` the
        // library out from under a live instance; `PluginPtr::drop` covers both.
        let plugin = PluginPtr::new(plugin_ptr);

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

        // The declared layout, empty sides included: `process` hands the
        // plugin exactly these buffers, as CLAP requires (`audio_inputs_count`
        // equals the plugin's own port count on that side).
        let ports = PortLayout {
            inputs: port_channels(plugin.as_ptr(), extensions.audio.ports, true),
            outputs: port_channels(plugin.as_ptr(), extensions.audio.ports, false),
        };

        // The reported totals state an empty side as one stereo bus (the
        // graph-facing shape). Default a copy's bus lists, then derive the
        // totals from it, so the two cannot disagree; see
        // `PortLayout::default_empty_buses`.
        let mut reported = ports.clone();
        reported.default_empty_buses();
        plugin_info.audio_inputs = reported.input_channel_total();
        plugin_info.audio_outputs = reported.output_channel_total();

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
            thread_pool: None,
        })
    }

    /// Loads a CLAP plugin for editor/parameter/state work only — never for
    /// audio. The returned instance must NOT be `activate()`d, `process()`d, or
    /// `start_processing()`d; doing so is a misuse of an editor-only load.
    ///
    /// CLAP's `gui`, `params`, and `state` extensions work without activation,
    /// so this skips the audio-config negotiation a processing load needs. The
    /// sample rate / max-frames passed to the plugin are placeholders that are
    /// never used (no `activate()` call consumes them). Use this in the
    /// in-process GUI host, where audio runs in a separate instance/process.
    ///
    /// # Errors
    ///
    /// Returns [`ClapError::LoadFailed`] if the library cannot be opened, has no
    /// compatible `clap_entry` or plugin factory, or the plugin cannot be
    /// created or initialized; `stage` says which step failed.
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
/// advancing its offset by each entry's channel count. A `filter_map` would
/// close the gap, so every later port would silently move down one index and
/// get handed its neighbour's channels. Truncating keeps the list a
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

/// Whether the plugin can be activated for 64-bit audio: every port on both
/// sides advertises `CLAP_AUDIO_PORT_SUPPORTS_64BITS`, and there is at least
/// one port.
///
/// Every port, inputs included, is this host's conservative rule, not a CLAP
/// requirement: CLAP flags 64-bit per port, and a host may hand each port
/// its own width. This host activates in one sample width and hands it for
/// every buffer (`data64` with `data32` null), so it picks 64-bit only when
/// no port would be handed a table it never claimed to read.
///
/// A plugin with no ports at all (or no `audio-ports` extension) is `false`:
/// it has no buffer to gain precision on, and f32 is the width every CLAP
/// plugin must accept. A side with no ports does not veto a side that has
/// them all flagged (an instrument with only 64-bit outputs is `true`): the
/// host hands no buffer on a side the plugin declared empty.
///
/// Stops at a `get` failure for the same index as [`port_channels`]: ports
/// past the hole are absent from the presented layout, so they neither count
/// nor veto.
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
    let mut presented = 0u32;
    for is_input in [true, false] {
        let count = unsafe { count_fn(plugin, is_input) };
        for i in 0..count {
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            if !unsafe { get_fn(plugin, i, is_input, &mut info) } {
                // Past this index `port_channels` presents no ports, so no
                // port past it may vote on the sample format.
                break;
            }
            if (info.flags & CLAP_AUDIO_PORT_SUPPORTS_64BITS) == 0 {
                return false;
            }
            presented += 1;
        }
    }
    presented > 0
}

#[cfg(test)]
mod enumeration_hole_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    // The stub's configuration. Global because the stub is a bare
    // `extern "C"` fn with no state parameter.
    /// Index at which the stub's `get` reports failure, on either side.
    static HOLE: AtomicU32 = AtomicU32::new(u32::MAX);
    /// Number of input / output ports the stub's `count` reports.
    static COUNT_IN: AtomicU32 = AtomicU32::new(0);
    static COUNT_OUT: AtomicU32 = AtomicU32::new(0);
    /// Bit `i` set: input / output port `i` advertises 64-bit support.
    static F64_IN: AtomicU32 = AtomicU32::new(0);
    static F64_OUT: AtomicU32 = AtomicU32::new(0);
    /// Serializes [`with_stub`]. The globals above are process-wide, every
    /// test configures them differently, and cargo runs the tests on parallel
    /// threads: unserialized, one test's `store` lands inside another's `f` and
    /// the suite fails on roughly 45% of runs.
    static LAYOUT_LOCK: Mutex<()> = Mutex::new(());

    /// Port `i` has `i + 1` channels, so an index shift is visible.
    unsafe extern "C" fn stub_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
        if is_input { &COUNT_IN } else { &COUNT_OUT }.load(Ordering::SeqCst)
    }

    unsafe extern "C" fn stub_get(
        _plugin: *const clap_plugin,
        index: u32,
        is_input: bool,
        info: *mut clap_audio_port_info,
    ) -> bool {
        if index == HOLE.load(Ordering::SeqCst) {
            return false;
        }
        let flagged = if is_input { &F64_IN } else { &F64_OUT }.load(Ordering::SeqCst);
        (*info).channel_count = index + 1;
        (*info).flags = if flagged & (1 << index) != 0 {
            CLAP_AUDIO_PORT_SUPPORTS_64BITS
        } else {
            0
        };
        true
    }

    /// One stub configuration: port counts, the hole, and which ports carry
    /// the 64-bit flag (bit `i` = port `i`).
    #[derive(Clone, Copy)]
    struct Stub {
        inputs: u32,
        outputs: u32,
        hole: u32,
        f64_in: u32,
        f64_out: u32,
    }

    /// Every port of an `n`-port side flagged.
    const fn all(n: u32) -> u32 {
        (1 << n) - 1
    }

    /// Configures the stub and runs `f` under [`LAYOUT_LOCK`], so the store and
    /// the read that follows it cannot interleave with another test's.
    ///
    /// The lock is taken poison-tolerant: a failing assertion inside `f` panics
    /// with the guard held, and propagating that as a `PoisonError` would turn
    /// one real failure into misleading ones in the other tests.
    fn with_stub<R>(stub: Stub, f: impl FnOnce(&clap_plugin_audio_ports) -> R) -> R {
        let _guard = LAYOUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        COUNT_IN.store(stub.inputs, Ordering::SeqCst);
        COUNT_OUT.store(stub.outputs, Ordering::SeqCst);
        HOLE.store(stub.hole, Ordering::SeqCst);
        F64_IN.store(stub.f64_in, Ordering::SeqCst);
        F64_OUT.store(stub.f64_out, Ordering::SeqCst);
        let ext = clap_plugin_audio_ports {
            count: Some(stub_count),
            get: Some(stub_get),
        };
        f(&ext)
    }

    /// `count` ports on each side, a hole at `hole`, no 64-bit flags.
    fn with_layout<R>(count: u32, hole: u32, f: impl FnOnce(&clap_plugin_audio_ports) -> R) -> R {
        with_stub(
            Stub {
                inputs: count,
                outputs: count,
                hole,
                f64_in: 0,
                f64_out: 0,
            },
            f,
        )
    }

    fn f64_support(stub: Stub) -> bool {
        with_stub(stub, |ext| check_f64_support(std::ptr::null(), ext))
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

    /// A hole must truncate, never renumber. A `filter_map` would return
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

    /// Every port on both sides flagged: the plugin can run at 64-bit.
    /// Baseline for the vetoes below; without it a constant `false` passes
    /// them all.
    ///
    /// Mutation (run): `presented > 0` → `false` fails here.
    #[test]
    fn f64_support_when_every_port_is_flagged() {
        assert!(f64_support(Stub {
            inputs: 2,
            outputs: 2,
            hole: u32::MAX,
            f64_in: all(2),
            f64_out: all(2),
        }));
    }

    /// The outputs all claim 64-bit, one input does not. The host hands
    /// `data64` to every buffer, so that input would get a table it never
    /// agreed to read.
    ///
    /// Mutation (run): scanning only the outputs (`for is_input in [false]`,
    /// the old check) → `true`, fails.
    #[test]
    fn an_unflagged_input_port_vetoes_f64() {
        assert!(!f64_support(Stub {
            inputs: 2,
            outputs: 2,
            hole: u32::MAX,
            f64_in: 0b01,
            f64_out: all(2),
        }));
    }

    /// One flagged output is not enough when another output is not.
    ///
    /// Mutation (run): `continue` instead of `return false` at an unflagged
    /// port (any flagged port then suffices) → `true`, fails.
    #[test]
    fn an_unflagged_output_port_vetoes_f64() {
        assert!(!f64_support(Stub {
            inputs: 1,
            outputs: 2,
            hole: u32::MAX,
            f64_in: all(1),
            f64_out: 0b10,
        }));
    }

    /// No ports at all: f32, the width every CLAP plugin must accept (see
    /// `check_f64_support` for why it is not the vacuous `true`).
    ///
    /// Mutation (run): returning `true` instead of `presented > 0` fails.
    #[test]
    fn a_plugin_with_no_ports_is_f32() {
        assert!(!f64_support(Stub {
            inputs: 0,
            outputs: 0,
            hole: u32::MAX,
            f64_in: 0,
            f64_out: 0,
        }));
    }

    /// An instrument (no inputs) whose outputs are all flagged runs at 64-bit:
    /// the empty side does not veto.
    ///
    /// Mutation (run): requiring a port on each side → `false`, fails.
    #[test]
    fn an_empty_side_does_not_veto_f64() {
        assert!(f64_support(Stub {
            inputs: 0,
            outputs: 2,
            hole: u32::MAX,
            f64_in: 0,
            f64_out: all(2),
        }));
    }

    /// The only unflagged port lies past the hole, so it is absent from the
    /// presented layout and must not veto.
    ///
    /// Mutation (run): `continue` instead of `break` at the failed `get` →
    /// port 3 vetoes, fails.
    #[test]
    fn f64_support_ignores_ports_past_a_hole() {
        assert!(
            f64_support(Stub {
                inputs: 4,
                outputs: 4,
                hole: 2,
                f64_in: 0b0111,
                f64_out: 0b0111,
            }),
            "the only unflagged ports (index 3) lie past the hole at index 2, \
             so they are absent from the presented layout and must not decide \
             the sample format"
        );
    }

    /// The complement: an unflagged port before the hole still vetoes, on the
    /// side scanned after a side that hit its hole.
    ///
    /// Mutation (run): `return true` instead of `break` at the failed `get`
    /// (a hole ends the whole scan as a success) passes the test above and
    /// fails here.
    #[test]
    fn f64_support_still_sees_ports_before_a_hole() {
        assert!(
            !f64_support(Stub {
                inputs: 4,
                outputs: 4,
                hole: 2,
                f64_in: all(4),
                f64_out: 0b1110,
            }),
            "output port 0 does not advertise 64-bit and precedes the hole, so \
             it must still veto"
        );
    }
}

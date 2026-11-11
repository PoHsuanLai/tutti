//! Construction of [`ClapLoaded`]: probe, load, and the port-layout /
//! f64-support helpers they read off the freshly instantiated plugin.

use super::ClapLoaded;
use super::config::{AudioConfig, LifecycleFlags, PortLayout};
use super::control::descriptor_of;
use crate::error::{ClapError, LoadStage, Result};
use crate::host::{ClapHost, HostState};
use crate::types::PluginInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tutti_clap_ffi::{AudioPortInfo, Bundle, DescriptorInfo, LoadError, PortKind, Stage};
use tutti_plugin_types::{BusChannels, ChannelLayout, EditorPresence};
use tutti_plugin_types::{Load, MainThread, Origin};
use tutti_types::ChannelCountOutOfRange;

/// A placeholder rate for the plugin until its first activation, whose
/// `Prepare` carries the real one.
const PLACEHOLDER_SAMPLE_RATE: f64 = 44_100.0;
/// A placeholder block size, for the same reason.
const PLACEHOLDER_MAX_FRAMES: u32 = 512;

/// What to load: which library of a bundle and which plugin of the library.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    library: Option<PathBuf>,
    plugin_id: Option<String>,
}

impl LoadOptions {
    /// Defaults: the bundle path itself, and the library's first plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads this library file rather than the bundle path.
    #[must_use]
    pub fn library(mut self, path: impl Into<PathBuf>) -> Self {
        self.library = Some(path.into());
        self
    }

    /// Loads the plugin with this id from the bundle rather than its first.
    #[must_use]
    pub fn plugin_id(mut self, id: impl Into<String>) -> Self {
        self.plugin_id = Some(id.into());
        self
    }
}

fn stage_of(stage: Stage) -> LoadStage {
    match stage {
        Stage::Opening => LoadStage::Opening,
        Stage::Factory => LoadStage::Factory,
        Stage::Instantiation => LoadStage::Instantiation,
        Stage::Initialization => LoadStage::Initialization,
    }
}

fn load_failed(bundle_path: &Path, error: LoadError) -> ClapError {
    ClapError::LoadFailed {
        path: bundle_path.to_path_buf(),
        stage: stage_of(error.stage()),
        reason: error.reason(),
    }
}

fn info_of(d: &DescriptorInfo) -> PluginInfo {
    PluginInfo::new(d.id.clone(), d.name.clone())
        .vendor(d.vendor.clone())
        .version(d.version.clone())
        .url(d.url.clone())
        .description(d.description.clone())
        .features(d.features.clone())
}

fn open(bundle_path: &Path, library_path: Option<&Path>) -> Result<Bundle> {
    Bundle::open(bundle_path, library_path).map_err(|e| load_failed(bundle_path, e))
}

impl ClapLoaded {
    /// Re-reads the plugin's audio ports and their 64-bit support. Only while
    /// the plugin is deactivated: the spec makes the ports `[!active]`.
    ///
    /// # Errors
    /// [`ClapError::NotSupported`] while the plugin is active;
    /// [`ClapError::PortTooWide`] when a port reports more channels than a
    /// layout holds.
    pub fn rescan_audio_ports(&mut self, main: &MainThread) -> Result<()> {
        if self.flags.active {
            return Err(ClapError::NotSupported(
                "audio ports can be re-read only while the plugin is deactivated".to_string(),
            ));
        }
        let (ports, supports_f64) = read_audio_ports(main, &mut self.info, &self.plugin)?;
        self.ports = ports;
        self.audio.supports_f64 = supports_f64;
        Ok(())
    }

    /// Reads a bundle's first plugin's descriptor without instantiating it.
    ///
    /// # Errors
    /// The library cannot be opened, or has no readable factory or descriptor.
    pub fn probe(bundle_path: impl AsRef<Path>, library_path: Option<&Path>) -> Result<PluginInfo> {
        let bundle_path = bundle_path.as_ref();
        let bundle = open(bundle_path, library_path)?;
        let index = bundle
            .select(None)
            .map_err(|e| load_failed(bundle_path, e))?;
        Ok(info_of(&bundle.descriptors()[index]))
    }

    /// Reads every plugin a bundle's factory advertises.
    ///
    /// # Errors
    /// As [`probe`](Self::probe).
    pub fn probe_all(
        bundle_path: impl AsRef<Path>,
        library_path: Option<&Path>,
    ) -> Result<Vec<PluginInfo>> {
        let bundle_path = bundle_path.as_ref();
        let bundle = open(bundle_path, library_path)?;
        bundle
            .select(None)
            .map_err(|e| load_failed(bundle_path, e))?;
        Ok(bundle.descriptors().iter().map(info_of).collect())
    }

    /// Loads and instantiates a plugin, not yet activated. Rate and block
    /// size are the activation's ([`Loaded::activate`](tutti_plugin_types::Loaded::activate)).
    ///
    /// # Errors
    /// The bundle cannot be opened, the plugin id is not in it, or the plugin
    /// refuses to be created or initialised, or an audio port reports more
    /// channels than a layout holds ([`ClapError::PortTooWide`]).
    pub fn load(
        main: &MainThread,
        bundle_path: impl AsRef<Path>,
        options: LoadOptions,
    ) -> Result<Self> {
        let bundle_path = bundle_path.as_ref();
        let bundle = open(bundle_path, options.library.as_deref())?;
        let index = bundle
            .select(options.plugin_id.as_deref())
            .map_err(|e| load_failed(bundle_path, e))?;
        let mut plugin_info = info_of(&bundle.descriptors()[index]);
        // The file opened and the plugin chosen in it, by id even when the
        // load named none, so a reopen takes the same one.
        let origin = Origin::new(
            options.library.as_deref().unwrap_or(bundle_path),
            Load::new().plugin(bundle.descriptors()[index].id.clone()),
        );

        let host_state = Arc::new(HostState::new(main));
        let host = ClapHost::new(host_state.clone()).into_inner();
        let plugin = bundle
            .instantiate(index, host, host_state.roles.clone())
            .map_err(|e| load_failed(bundle_path, e))?;

        let (ports, supports_f64) = read_audio_ports(main, &mut plugin_info, &plugin)?;
        let audio = AudioConfig {
            sample_rate: PLACEHOLDER_SAMPLE_RATE,
            max_frames: PLACEHOLDER_MAX_FRAMES,
            supports_f64,
            f64_active: false,
        };

        let loaded = Self {
            plugin,
            host_state,
            descriptor: std::cell::OnceCell::new(),
            unmeasured: descriptor_of(&plugin_info, EditorPresence::Unknown),
            info: plugin_info,
            audio,
            ports,
            flags: LifecycleFlags::default(),
            work: Default::default(),
            thread_pool: None,
            origin,
        };
        Ok(loaded)
    }
}

/// What the loader reads audio ports from: the plugin, or a stand-in.
pub(crate) trait AudioPortSource {
    /// `audio-ports.count`.
    fn port_count(&self, main: &MainThread, is_input: bool) -> u32;
    /// `audio-ports.get`.
    fn port_info(&self, main: &MainThread, index: u32, is_input: bool) -> Option<AudioPortInfo>;
}

impl AudioPortSource for tutti_clap_ffi::Plugin {
    fn port_count(&self, main: &MainThread, is_input: bool) -> u32 {
        self.audio_port_count(main, is_input)
    }

    fn port_info(&self, main: &MainThread, index: u32, is_input: bool) -> Option<AudioPortInfo> {
        self.audio_port_info(main, index, is_input)
    }
}

/// The channel layout a port's type and count name: mono and stereo by their
/// type tag, anything else by count.
///
/// A count past what a layout holds is refused, never wrapped: a plugin
/// reporting 65 538 channels must not become a stereo bus.
pub(super) fn layout_of_port(
    port: &AudioPortInfo,
) -> std::result::Result<ChannelLayout, ChannelCountOutOfRange> {
    match port.port_type {
        PortKind::Mono => Ok(ChannelLayout::MONO),
        PortKind::Stereo => Ok(ChannelLayout::STEREO),
        _ => ChannelLayout::try_from(port.channel_count),
    }
}

fn port_channels(
    main: &MainThread,
    source: &impl AudioPortSource,
    is_input: bool,
) -> Result<BusChannels> {
    let count = source.port_count(main, is_input);
    let mut channels = BusChannels::with_capacity(count as usize);
    for i in 0..count {
        let Some(port) = source.port_info(main, i, is_input) else {
            break;
        };
        let layout = layout_of_port(&port).map_err(|_| ClapError::PortTooWide {
            is_input,
            index: i,
            channels: port.channel_count,
        })?;
        channels.push(layout);
    }
    Ok(channels)
}

/// Reads the plugin's port layout into `info` and returns it with whether
/// every presented port takes 64-bit audio.
fn read_audio_ports(
    main: &MainThread,
    info: &mut PluginInfo,
    source: &impl AudioPortSource,
) -> Result<(PortLayout, bool)> {
    let ports = PortLayout {
        inputs: port_channels(main, source, true)?,
        outputs: port_channels(main, source, false)?,
    };
    info.audio_inputs = ports.input_channel_total();
    info.audio_outputs = ports.output_channel_total();
    Ok((ports, check_f64_support(main, source)))
}

/// Whether the plugin processes in 64-bit: every presented port (inputs and
/// outputs) advertises `CLAP_AUDIO_PORT_SUPPORTS_64BITS`, and at least one is
/// presented. A plugin with no ports is f32, the width every CLAP plugin must
/// accept, not the vacuous `true`.
///
/// Stops at the first port that fails to enumerate, as the layout does, so a
/// port past a hole (absent from the layout) does not decide the format.
fn check_f64_support(main: &MainThread, source: &impl AudioPortSource) -> bool {
    let mut presented = 0u32;
    for is_input in [true, false] {
        let count = source.port_count(main, is_input);
        for i in 0..count {
            let Some(port) = source.port_info(main, i, is_input) else {
                break;
            };
            if !port
                .flags
                .contains(tutti_clap_ffi::AudioPortFlags::SUPPORTS_64BIT)
            {
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

    fn claim() -> MainThread {
        MainThread::claim().expect("one test, one process")
    }

    /// A plugin's audio-port enumeration, as configured data: `inputs` and
    /// `outputs` ports, port `index` answering `false` at `hole`, port `i`
    /// `i + 1` channels wide and 64-bit when bit `i` of `f64_in` / `f64_out`
    /// is set.
    #[derive(Clone, Copy)]
    struct Stub {
        inputs: u32,
        outputs: u32,
        hole: u32,
        f64_in: u32,
        f64_out: u32,
    }

    impl AudioPortSource for Stub {
        fn port_count(&self, _main: &MainThread, is_input: bool) -> u32 {
            if is_input { self.inputs } else { self.outputs }
        }

        fn port_info(
            &self,
            _main: &MainThread,
            index: u32,
            is_input: bool,
        ) -> Option<AudioPortInfo> {
            if index == self.hole {
                return None;
            }
            let flagged = if is_input { self.f64_in } else { self.f64_out };
            Some(AudioPortInfo {
                id: index,
                name: String::new(),
                flags: if flagged & (1 << index) != 0 {
                    tutti_clap_ffi::AudioPortFlags::SUPPORTS_64BIT
                } else {
                    tutti_clap_ffi::AudioPortFlags::empty()
                },
                channel_count: index + 1,
                port_type: PortKind::Unspecified,
                in_place_pair: u32::MAX,
            })
        }
    }

    const fn all(n: u32) -> u32 {
        (1 << n) - 1
    }

    fn with_layout(count: u32, hole: u32) -> Stub {
        Stub {
            inputs: count,
            outputs: count,
            hole,
            f64_in: 0,
            f64_out: 0,
        }
    }

    fn f64_support(stub: Stub) -> bool {
        check_f64_support(&claim(), &stub)
    }

    /// Baseline — without it, dropping all ports unconditionally would satisfy
    /// the truncation assertions below.
    #[test]
    fn no_hole_reports_every_port() {
        let channels = port_channels(&claim(), &with_layout(4, u32::MAX), false).unwrap();
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
        let channels = port_channels(&claim(), &with_layout(4, 2), false).unwrap();
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
        let channels = port_channels(&claim(), &with_layout(4, 0), false).unwrap();
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

    /// A port whose type tag does not name its width, reporting `channels`.
    fn unspecified_port(channels: u32) -> AudioPortInfo {
        AudioPortInfo {
            id: 0,
            name: String::new(),
            flags: tutti_clap_ffi::AudioPortFlags::empty(),
            channel_count: channels,
            port_type: PortKind::Unspecified,
            in_place_pair: u32::MAX,
        }
    }

    /// One input port reporting a fixed count.
    struct Wide(u32);

    impl AudioPortSource for Wide {
        fn port_count(&self, _main: &MainThread, is_input: bool) -> u32 {
            u32::from(is_input)
        }

        fn port_info(
            &self,
            _main: &MainThread,
            index: u32,
            _is_input: bool,
        ) -> Option<AudioPortInfo> {
            (index == 0).then(|| unspecified_port(self.0))
        }
    }

    /// A count a layout holds is read as it is, to the last one; one past it is
    /// refused with the count, not wrapped into a narrower bus.
    ///
    /// Mutation (run): `_ => ChannelLayout::try_from(port.channel_count)`
    /// written `Ok(ChannelLayout::from_count(port.channel_count as u16))` makes
    /// 65 538 a stereo bus, 65 536 an empty one, and fails every refusal here.
    #[test]
    fn a_port_count_past_a_layout_is_refused_not_wrapped() {
        assert_eq!(
            layout_of_port(&unspecified_port(u32::from(u16::MAX))),
            Ok(ChannelLayout::from(u16::MAX))
        );
        for wide in [65_536, 65_537, 65_538, u32::MAX] {
            assert_eq!(
                layout_of_port(&unspecified_port(wide)),
                Err(ChannelCountOutOfRange(u64::from(wide)))
            );
        }
        // The type tag names the width, so the count is not consulted.
        let mut stereo = unspecified_port(70_000);
        stereo.port_type = PortKind::Stereo;
        assert_eq!(layout_of_port(&stereo), Ok(ChannelLayout::STEREO));
    }

    /// The load fails, naming the port and its side, instead of building a
    /// bus of the wrapped width.
    ///
    /// Mutation (run): as above, the cast makes `port_channels` return
    /// `Ok([Stereo])` for 65 538 and this fails.
    #[test]
    fn a_plugin_reporting_a_huge_port_fails_the_load_naming_the_port() {
        let err = port_channels(&claim(), &Wide(65_538), true).expect_err("refused");
        assert!(matches!(
            err,
            ClapError::PortTooWide {
                is_input: true,
                index: 0,
                channels: 65_538
            }
        ));
        let message = err.to_string();
        assert!(message.contains("input port 0"), "{message}");
        assert!(message.contains("65538"), "{message}");

        let mut info = PluginInfo::new("id", "name");
        assert!(matches!(
            read_audio_ports(&claim(), &mut info, &Wide(u32::MAX)),
            Err(ClapError::PortTooWide { .. })
        ));
        assert!(read_audio_ports(&claim(), &mut info, &Wide(8)).is_ok());
    }
}

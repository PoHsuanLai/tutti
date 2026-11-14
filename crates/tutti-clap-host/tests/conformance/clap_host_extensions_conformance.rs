//! Host-conformance harness for the extensions a host offers a plugin or tells
//! it about: **track-info** and **transport-control**, driven by a real plugin across the real CLAP FFI.
//!
//! The reference plugin records what the host handed it (what `track-info.get` answered) and issues the transport
//! requests on command. The expected figures are here, not in the plugin. Its
//! capture and command words are process-globals, so every test holds
//! [`PROBE_LOCK`] for its whole scenario.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::support;
use support::active::{Activate, Active, ActiveExt, drive_block};
use support::main_thread;

use tutti_clap_host::{ClapLoaded, ClapProcessContext, LoadOptions};
use tutti_clap_test_plugin::{
    HOSTEXT_CMD_CONTINUE, HOSTEXT_CMD_ENABLE_LOOP, HOSTEXT_CMD_JUMP, HOSTEXT_CMD_LOOP_REGION,
    HOSTEXT_CMD_PAUSE, HOSTEXT_CMD_RECORD, HOSTEXT_CMD_START, HOSTEXT_CMD_START_BURST,
    HOSTEXT_CMD_STOP, HOSTEXT_CMD_TOGGLE_LOOP, HOSTEXT_CMD_TOGGLE_PLAY, HOSTEXT_CMD_TOGGLE_RECORD,
    HOSTEXT_CMD_TRACK_GET, HostExtCapture,
};
use tutti_plugin_types::{
    ChannelLayout, Control, Delivered, Request, TrackInfo, TrackKind, TransportRequest,
};
use tutti_types::{Beat, BeatDuration};

/// `clap_beattime` is 2^31 units to the beat (`fixedpoint.h`); pinned here so a
/// failure names the wire value the plugin sent.
const BEAT: i64 = 1 << 31;

/// `CLAP_TRACK_INFO_*` (`ext/track-info.h`), pinned for the same reason.
const HAS_TRACK_NAME: u64 = 1 << 0;
const HAS_TRACK_COLOR: u64 = 1 << 1;
const HAS_AUDIO_CHANNEL: u64 = 1 << 2;
const IS_FOR_RETURN_TRACK: u64 = 1 << 3;
const IS_FOR_BUS: u64 = 1 << 4;
const IS_FOR_MASTER: u64 = 1 << 5;

static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// A held [`PROBE_LOCK`] plus a freshly reset probe.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        call::<unsafe extern "C" fn()>(b"tutti_test_plugin_hostext_reset\0", |f| unsafe { f() });
        Probe { _lock: lock }
    }

    fn load(&self) -> ClapLoaded {
        let path = Path::new(tutti_fixtures::clap_probe());
        ClapLoaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            path,
            LoadOptions::new().library(path),
        )
        .expect("reference plugin should load")
    }

    fn activate(&self) -> Active<f32> {
        self.load()
            .activate_at::<f32>(48_000.0, 512)
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }

    fn capture(&self) -> HostExtCapture {
        let mut cap = std::mem::MaybeUninit::<HostExtCapture>::zeroed();
        call::<unsafe extern "C" fn(*mut HostExtCapture) -> bool>(
            b"tutti_test_plugin_hostext_capture\0",
            |f| assert!(unsafe { f(cap.as_mut_ptr()) }),
        );
        // SAFETY: the plugin wrote a whole `HostExtCapture`.
        unsafe { cap.assume_init() }
    }

    /// Latches `cmd` and runs the plugin's `on_main_thread`, where it calls the
    /// host.
    fn command(&self, inst: &mut ClapLoaded, cmd: u32, a: i64, b: i64) {
        call::<unsafe extern "C" fn(u32, i64, i64)>(
            b"tutti_test_plugin_hostext_command\0",
            |f| unsafe { f(cmd, a, b) },
        );
        inst.on_main_thread(main_thread());
    }

    fn process_command(&self, cmd: u32) {
        call::<unsafe extern "C" fn(u32, i64, i64)>(
            b"tutti_test_plugin_hostext_process_command\0",
            |f| unsafe { f(cmd, 0, 0) },
        );
    }
}

/// Opens the reference plugin's image (a second time, once a host has loaded
/// it: they share one) and runs `f` on the exported symbol.
///
/// The image is never closed: a switch set before a load lives in statics that
/// an unloaded image would lose, and the host's load must find them set.
fn call<F: Copy>(name: &[u8], f: impl FnOnce(F)) {
    // SAFETY: an exported symbol of the reference cdylib, of the type named at
    // each call site.
    unsafe {
        let lib =
            libloading::Library::new(tutti_fixtures::clap_probe()).expect("open reference plugin");
        let sym: libloading::Symbol<F> = lib.get(name).expect("probe symbol present");
        f(*sym);
        std::mem::forget(lib);
    }
}

/// Tells the plugin its track, which an in-process plugin always takes.
fn set_track(inst: &mut ClapLoaded, info: &TrackInfo) {
    assert_eq!(
        inst.set_track_info(main_thread(), info),
        Delivered::Yes,
        "an in-process plugin takes the track"
    );
}

fn requests(inst: &mut ClapLoaded) -> Vec<Request> {
    let mut out = Vec::new();
    inst.take_requests(main_thread(), &mut out);
    out
}

// ---------------------------------------------------------------------------
// track-info
// ---------------------------------------------------------------------------

fn stereo_track() -> TrackInfo {
    TrackInfo {
        name: Some("Lead".to_owned()),
        color: Some(rgb::RGBA8::new(10, 20, 30, 40)),
        audio: Some(ChannelLayout::STEREO),
        kind: TrackKind::Track,
    }
}

/// With nothing said about the track, `get` answers `false`.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_get_answers_false_until_the_host_says_something() {
    let probe = Probe::acquire();
    let mut inst = probe.load();

    probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

    let cap = probe.capture();
    assert_eq!(cap.track_queries, 1);
    assert!(!cap.track_query_ok);
}

/// `get` carries the name, colour and audio width the host set, each with its
/// flag, and a stereo track names the `stereo` port type.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_get_answers_what_the_host_set() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    set_track(&mut inst, &stereo_track());

    probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

    let track = probe.capture().track_queried;
    assert!(probe.capture().track_query_ok);
    assert_eq!(
        track.flags,
        HAS_TRACK_NAME | HAS_TRACK_COLOR | HAS_AUDIO_CHANNEL
    );
    assert_eq!(track.name_str(), "Lead");
    assert_eq!(track.color, [10, 20, 30, 40]);
    assert_eq!(track.audio_channel_count, 2);
    assert_eq!(track.port_type_str(), "stereo");
}

/// A field the host leaves unset has no flag, so the plugin is not told a
/// name or colour that does not exist.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_flags_only_the_fields_the_host_knows() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    let info = TrackInfo {
        audio: Some(ChannelLayout::MONO),
        ..TrackInfo::default()
    };
    set_track(&mut inst, &info);

    probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

    let track = probe.capture().track_queried;
    assert_eq!(track.flags, HAS_AUDIO_CHANNEL);
    assert_eq!(track.audio_channel_count, 1);
    assert_eq!(track.port_type_str(), "mono");
}

/// A width CLAP has no port-type word for is a channel count with no type.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_gives_no_port_type_for_a_width_clap_has_no_word_for() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    let info = TrackInfo {
        audio: Some(ChannelLayout::from(6u16)),
        ..TrackInfo::default()
    };
    set_track(&mut inst, &info);

    probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

    let track = probe.capture().track_queried;
    assert_eq!(track.audio_channel_count, 6);
    assert!(!track.has_port_type);
}

/// Each kind of track sets its own flag and no other.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_kind_sets_exactly_one_flag() {
    for (kind, flag) in [
        (TrackKind::Track, 0),
        (TrackKind::Return, IS_FOR_RETURN_TRACK),
        (TrackKind::Bus, IS_FOR_BUS),
        (TrackKind::Master, IS_FOR_MASTER),
    ] {
        let probe = Probe::acquire();
        let mut inst = probe.load();
        let info = TrackInfo {
            kind,
            ..TrackInfo::default()
        };
        set_track(&mut inst, &info);

        probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

        assert_eq!(probe.capture().track_queried.flags, flag, "{kind:?}");
    }
}

/// A name longer than the buffer is cut to fit, on a character boundary, and
/// stays text.
/// Spec: clap.ext.track_info.host.get
#[test]
fn track_info_cuts_a_long_name_on_a_character_boundary() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    // Two bytes a character: 255 bytes of room hold 127 of them.
    let info = TrackInfo {
        name: Some("é".repeat(300)),
        ..TrackInfo::default()
    };
    set_track(&mut inst, &info);

    probe.command(&mut inst, HOSTEXT_CMD_TRACK_GET, 0, 0);

    assert_eq!(probe.capture().track_queried.name_str(), "é".repeat(127));
}

/// The plugin is told when the host's answer changes, and `get` inside the
/// call already answers the new value.
/// Spec: clap.ext.track_info.changed
#[test]
fn the_plugin_is_told_when_the_track_changes_and_get_then_answers_the_new_value() {
    let probe = Probe::acquire();
    let mut inst = probe.load();

    set_track(&mut inst, &stereo_track());

    let cap = probe.capture();
    assert_eq!(cap.track_changed_calls, 1);
    assert_eq!(cap.track_at_changed.name_str(), "Lead");
}

/// Setting what is already set tells the plugin nothing.
/// Spec: clap.ext.track_info.changed
#[test]
fn an_unchanged_track_does_not_tell_the_plugin() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    set_track(&mut inst, &stereo_track());

    set_track(&mut inst, &stereo_track());
    assert_eq!(probe.capture().track_changed_calls, 1);

    let renamed = TrackInfo {
        name: Some("Bass".to_owned()),
        ..stereo_track()
    };
    set_track(&mut inst, &renamed);
    let cap = probe.capture();
    assert_eq!(cap.track_changed_calls, 2);
    assert_eq!(cap.track_at_changed.name_str(), "Bass");
}

// ---------------------------------------------------------------------------
// transport-control
// ---------------------------------------------------------------------------

/// Issues `cmd` from the plugin's main thread and returns what the host
/// raised.
fn asked(cmd: u32, a: i64, b: i64) -> Vec<Request> {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    let _ = requests(&mut inst);
    probe.command(&mut inst, cmd, a, b);
    assert!(probe.capture().transport_offered, "the host offers it");
    requests(&mut inst)
}

fn transport(request: TransportRequest) -> Vec<Request> {
    vec![Request::Transport(request)]
}

/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn request_start_reaches_the_host_as_a_start_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_START, 0, 0),
        transport(TransportRequest::Start)
    );
}

/// Spec: clap.ext.transport_control.host.request_stop
#[test]
fn request_stop_reaches_the_host_as_a_stop_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_STOP, 0, 0),
        transport(TransportRequest::Stop)
    );
}

/// Spec: clap.ext.transport_control.host.request_continue
#[test]
fn request_continue_reaches_the_host_as_a_continue_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_CONTINUE, 0, 0),
        transport(TransportRequest::Continue)
    );
}

/// Spec: clap.ext.transport_control.host.request_pause
#[test]
fn request_pause_reaches_the_host_as_a_pause_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_PAUSE, 0, 0),
        transport(TransportRequest::Pause)
    );
}

/// Spec: clap.ext.transport_control.host.request_toggle_play
#[test]
fn request_toggle_play_reaches_the_host_as_a_toggle_play_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_TOGGLE_PLAY, 0, 0),
        transport(TransportRequest::TogglePlay)
    );
}

/// The fixed-point position is `raw / 2^31` beats: 3.25 beats is `3.25 * 2^31`.
/// Spec: clap.ext.transport_control.host.request_jump
#[test]
fn request_jump_carries_the_position_in_beats() {
    let raw = BEAT * 13 / 4;
    assert_eq!(
        asked(HOSTEXT_CMD_JUMP, raw, 0),
        transport(TransportRequest::Jump(Beat::new(3.25)))
    );
}

/// The region is a start position and a length, both in beats.
/// Spec: clap.ext.transport_control.host.request_loop_region
#[test]
fn request_loop_region_carries_the_start_and_the_length_in_beats() {
    assert_eq!(
        asked(HOSTEXT_CMD_LOOP_REGION, BEAT * 8, BEAT * 3 / 2),
        transport(TransportRequest::LoopRegion {
            start: Beat::new(8.0),
            duration: BeatDuration::new(1.5),
        })
    );
}

/// Spec: clap.ext.transport_control.host.request_toggle_loop
#[test]
fn request_toggle_loop_reaches_the_host_as_a_toggle_loop_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_TOGGLE_LOOP, 0, 0),
        transport(TransportRequest::ToggleLoop)
    );
}

/// Spec: clap.ext.transport_control.host.request_enable_loop
#[test]
fn request_enable_loop_carries_whether_to_loop() {
    assert_eq!(
        asked(HOSTEXT_CMD_ENABLE_LOOP, 1, 0),
        transport(TransportRequest::EnableLoop(true))
    );
    assert_eq!(
        asked(HOSTEXT_CMD_ENABLE_LOOP, 0, 0),
        transport(TransportRequest::EnableLoop(false))
    );
}

/// Spec: clap.ext.transport_control.host.request_record
#[test]
fn request_record_carries_whether_to_record() {
    assert_eq!(
        asked(HOSTEXT_CMD_RECORD, 1, 0),
        transport(TransportRequest::Record(true))
    );
    assert_eq!(
        asked(HOSTEXT_CMD_RECORD, 0, 0),
        transport(TransportRequest::Record(false))
    );
}

/// Spec: clap.ext.transport_control.host.request_toggle_record
#[test]
fn request_toggle_record_reaches_the_host_as_a_toggle_record_request() {
    assert_eq!(
        asked(HOSTEXT_CMD_TOGGLE_RECORD, 0, 0),
        transport(TransportRequest::ToggleRecord)
    );
}

/// Requests arrive once, in the order the plugin made them: a second take finds
/// none.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn transport_requests_are_taken_once_and_in_order() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    probe.command(&mut inst, HOSTEXT_CMD_START, 0, 0);
    probe.command(&mut inst, HOSTEXT_CMD_STOP, 0, 0);
    probe.command(&mut inst, HOSTEXT_CMD_PAUSE, 0, 0);

    assert_eq!(
        requests(&mut inst),
        [
            Request::Transport(TransportRequest::Start),
            Request::Transport(TransportRequest::Stop),
            Request::Transport(TransportRequest::Pause),
        ]
    );
    assert!(requests(&mut inst).is_empty(), "taken once");
}

/// The extension is `[main-thread]`: a request the plugin makes from its audio
/// thread is refused, not queued, so the audio thread never reaches the queue.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn a_transport_request_from_the_audio_thread_is_refused() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let _ = requests(inst.loaded_mut());

    probe.process_command(HOSTEXT_CMD_START);
    drive_block(&mut inst, 128, &ClapProcessContext::default());

    assert!(probe.capture().transport_offered, "the plugin did ask");
    assert!(
        !requests(inst.loaded_mut())
            .iter()
            .any(|r| matches!(r, Request::Transport(_))),
        "nothing is queued from the audio thread"
    );
}

/// A plugin that asks faster than the host takes is not queued without bound:
/// the first requests are kept, and each one refused is a plugin-misbehaving
/// line in the log, so none is lost unseen.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn a_flood_of_transport_requests_is_bounded_and_each_refusal_is_logged() {
    let probe = Probe::acquire();
    let mut inst = probe.load();
    inst.drain_log();

    probe.command(&mut inst, HOSTEXT_CMD_START_BURST, 100, 0);

    assert_eq!(requests(&mut inst).len(), 64);
    let refused = inst
        .drain_log()
        .into_iter()
        .filter(|r| {
            r.severity == clap_sys::ext::log::CLAP_LOG_PLUGIN_MISBEHAVING
                && r.message.starts_with("transport request Start refused")
        })
        .count();
    assert_eq!(refused, 100 - 64, "every refused request is logged once");
}

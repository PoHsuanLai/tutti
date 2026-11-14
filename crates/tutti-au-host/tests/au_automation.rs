//! Automation timing, the plugin's own edits, and the parameters a unit flags
//! `NonRealTime`, against Apple's Audio Units.
//!
//! The properties come from the AudioToolbox headers:
//!
//! - `AUComponent.h`, `AudioUnitScheduleParameters`: events schedule "intra-buffer
//!   changes to the value of a parameter" at a `bufferOffset`, and apply to the
//!   render call in flight only.
//! - `AudioUnitUtilities.h`: a parameter listener hears the changes the unit's own
//!   editor makes (a write through `AUParameterSet`).
//! - `AudioUnitProperties.h`, `kAudioUnitParameterFlag_NonRealTime`: changing the
//!   parameter "in real-time will cause a glitch".

#![cfg(target_os = "macos")]

use tutti_au_host::component::find_component;
use tutti_au_host::types::{AudioComponentDescription, K_AUDIO_UNIT_TYPE_EFFECT};
use tutti_au_host::{AuController, AuLoaded, AuProcessor};
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Loaded, MaxBlock, ParamEdit, ParamId, ParameterChanges, PluginIo,
    Prepare, ProcessContext, ProcessOutput, Processor,
};

const RATE: tutti_types::SampleRate = tutti_types::SampleRate::SR_48K;
const BLOCK: usize = 512;

fn delay_loaded() -> AuLoaded {
    let desc = AudioComponentDescription {
        componentType: K_AUDIO_UNIT_TYPE_EFFECT,
        componentSubType: u32::from_be_bytes(*b"dely"),
        componentManufacturer: u32::from_be_bytes(*b"appl"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let comp = find_component(&desc).expect("AUDelay ships with macOS");
    AuLoaded::new(comp, RATE, BLOCK as u32, &tutti_fixtures::detached_work()).unwrap()
}

fn activate(au: AuLoaded) -> (AuProcessor, AuController) {
    let prepare = Prepare::new(RATE, MaxBlock::of(tutti_types::Samples(BLOCK as _)));
    au.activate(tutti_fixtures::main_thread(), prepare)
        .unwrap_or_else(|e| panic!("AUDelay activates: {}", e.reason))
}

/// One block of `level` on both channels through `processor` with `changes`.
fn render_block(processor: &mut AuProcessor, level: f32, changes: &ParameterChanges) -> Vec<f32> {
    let input = vec![vec![level; BLOCK]; 2];
    let mut output = vec![vec![0.0f32; BLOCK]; 2];
    let inputs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    let ctx = ProcessContext::new().params(changes);
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer {
            inputs: &inputs,
            outputs: &mut outputs,
            num_samples: BLOCK,
            sample_rate: RATE.get(),
        }),
        ctx: &ctx,
        out: &mut out,
    };
    processor.process(&mut io).expect("the block renders");
    output.swap_remove(0)
}

const DRY_WET: u32 = 0;
const DELAY_TIME: u32 = 1;

/// The unit's own plain value of `id`.
fn plain(controller: &AuController, id: u32) -> f32 {
    tutti_au_host::parameters::get(controller.unit(), id).expect("the unit answers")
}

/// `AUDelay`'s Delay Time carries `kAudioUnitParameterFlag_NonRealTime`
/// ("changing the parameter in real-time will cause a glitch"), Dry/Wet does
/// not. The flag is read from the unit's own parameter info.
/// Spec: au.param.flag.NonRealTime
#[test]
fn the_non_realtime_flag_is_read_from_the_unit() {
    let au = delay_loaded();
    let list = au.get_parameter_list();
    let flag = |id: u32| list.iter().find(|p| p.id == id).unwrap().non_realtime;
    assert!(flag(DELAY_TIME), "Delay Time is flagged NonRealTime");
    assert!(!flag(DRY_WET), "Dry/Wet is not");
}

/// A block's point for a `NonRealTime` parameter is not written by the render:
/// the unit still holds its value when the block returns, and the control
/// side's tick (`flush_edits`) is what applies it. The point for an ordinary
/// parameter in the same block lands in the block.
#[test]
fn a_non_realtime_parameter_is_written_between_blocks_not_by_the_render() {
    use tutti_plugin_types::Control;
    let main = tutti_fixtures::main_thread();
    let (mut processor, mut controller) = activate(delay_loaded());
    let delay_before = plain(&controller, DELAY_TIME);
    let mix_before = plain(&controller, DRY_WET);

    let mut changes = ParameterChanges::reserved(2, 4);
    assert!(changes.add_change(ParamId::from(DELAY_TIME), 0, 1.0));
    assert!(changes.add_change(ParamId::from(DRY_WET), 0, 1.0));
    render_block(&mut processor, 0.0, &changes);

    assert_eq!(
        plain(&controller, DELAY_TIME),
        delay_before,
        "the render wrote a NonRealTime parameter"
    );
    assert_eq!(plain(&controller, DRY_WET), 100.0, "an ordinary one lands");
    assert_ne!(
        mix_before, 100.0,
        "the fixture moves the ordinary parameter"
    );

    Control::flush_edits(&mut controller, main);
    assert_eq!(
        plain(&controller, DELAY_TIME),
        2.0,
        "the control side's tick applies the deferred write, at the declared maximum"
    );
}

/// A block's automation does not reach the unit's other listeners (the plugin's
/// editor) by itself: only `AUParameterSet` and `AUParameterListenerNotify`
/// notify them (`AudioUnitUtilities.h`), and their listener machinery takes
/// locks and dispatches, so the render calls neither. The control side's tick
/// tells them, notifying with no write: they read the unit's value. A listener of our own
/// (not the host's, which `AUParameterSet` leaves out) hears nothing while the
/// block has only rendered, and then the block's last value after the tick.
#[test]
fn the_blocks_last_value_reaches_other_listeners_at_the_tick_not_the_render() {
    use std::sync::{Arc, Mutex};
    use tutti_au_ffi::{AuEvent, AuParameterListener, EventAddress};
    use tutti_plugin_types::Control;

    let main = tutti_fixtures::main_thread();
    let (mut processor, mut controller) = activate(delay_loaded());
    let heard: Arc<Mutex<Vec<(u32, f32)>>> = Arc::default();
    let sink = Arc::clone(&heard);
    let editor = AuParameterListener::new(controller.unit(), move |event| {
        if let AuEvent::ParameterChanged { id, value, .. } = event {
            sink.lock().unwrap().push((id, value));
        }
    })
    .expect("a listener registers");
    editor
        .watch_parameter(DRY_WET, EventAddress::GLOBAL)
        .unwrap();

    let mut changes = ParameterChanges::reserved(1, 4);
    assert!(changes.add_change(ParamId::from(DRY_WET), 100, 0.75));
    render_block(&mut processor, 0.0, &changes);
    assert_eq!(plain(&controller, DRY_WET), 75.0, "the automation landed");

    // Past the listener's notification interval.
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(
        heard.lock().unwrap().is_empty(),
        "the render notified a listener: {:?}",
        heard.lock().unwrap()
    );

    Control::flush_edits(&mut controller, main);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while heard.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let heard = heard.lock().unwrap();
    assert_eq!(
        heard.last().copied(),
        Some((DRY_WET, 75.0)),
        "the tick tells the listener the block's last value: {heard:?}"
    );
}

/// The tick tells the listeners of an automated value the unit already
/// applied; it never writes one. A block that ran after the one being told
/// about may have moved the parameter on, and a write of the older end value
/// would undo it. So the unit still holds the later value after the tick.
#[test]
fn the_tick_does_not_write_the_value_back_over_a_later_one() {
    use tutti_plugin_types::Control;

    let main = tutti_fixtures::main_thread();
    let (mut processor, mut controller) = activate(delay_loaded());

    let mut changes = ParameterChanges::reserved(1, 4);
    assert!(changes.add_change(ParamId::from(DRY_WET), 100, 0.75));
    render_block(&mut processor, 0.0, &changes);
    assert_eq!(plain(&controller, DRY_WET), 75.0, "the automation landed");

    // As if a later block had moved the parameter on.
    tutti_au_host::parameters::set(controller.unit(), DRY_WET, 10.0).unwrap();
    Control::flush_edits(&mut controller, main);
    assert_eq!(
        plain(&controller, DRY_WET),
        10.0,
        "the tick wrote the earlier block's end over the later value"
    );
}

/// A parameter automated in far more blocks than the old ring of ids held
/// (1024) between two ticks is still told to the listeners once at the tick,
/// each parameter exactly once and none dropped: a flag is a level, not a count.
#[test]
fn many_blocks_between_ticks_tell_each_parameter_once() {
    use std::sync::{Arc, Mutex};
    use tutti_au_ffi::{AuEvent, AuParameterListener, EventAddress};
    use tutti_plugin_types::Control;

    let main = tutti_fixtures::main_thread();
    let (mut processor, mut controller) = activate(delay_loaded());
    let heard: Arc<Mutex<Vec<(u32, f32)>>> = Arc::default();
    let sink = Arc::clone(&heard);
    let editor = AuParameterListener::new(controller.unit(), move |event| {
        if let AuEvent::ParameterChanged { id, value, .. } = event {
            sink.lock().unwrap().push((id, value));
        }
    })
    .expect("a listener registers");
    for id in [DRY_WET, DELAY_TIME] {
        editor.watch_parameter(id, EventAddress::GLOBAL).unwrap();
    }

    const BLOCKS: usize = 3000;
    for n in 0..BLOCKS {
        let mut changes = ParameterChanges::reserved(2, 4);
        let level = (n % 100) as f64 / 100.0;
        assert!(changes.add_change(ParamId::from(DRY_WET), 100, level));
        assert!(changes.add_change(ParamId::from(DELAY_TIME), 100, 0.5));
        render_block(&mut processor, 0.0, &changes);
    }
    let mut last = ParameterChanges::reserved(1, 4);
    assert!(last.add_change(ParamId::from(DRY_WET), 100, 0.75));
    render_block(&mut processor, 0.0, &last);

    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(
        heard.lock().unwrap().is_empty(),
        "the render notified a listener"
    );

    Control::flush_edits(&mut controller, main);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while heard.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Long enough for a surplus notification to arrive.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let heard = heard.lock().unwrap();
    for id in [DRY_WET, DELAY_TIME] {
        let told: Vec<_> = heard.iter().filter(|(i, _)| *i == id).collect();
        assert_eq!(
            told.len(),
            1,
            "parameter {id} told {} times: {heard:?}",
            told.len()
        );
    }
    assert!(
        heard.contains(&(DRY_WET, 75.0)),
        "the last value was told: {heard:?}"
    );
}

/// What a listener on another queue is given a moment to deliver.
fn wait_for_edits(controller: &mut AuController, want: usize) -> Vec<ParamEdit> {
    use tutti_plugin_types::Control;
    let main = tutti_fixtures::main_thread();
    let mut heard = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while heard.len() < want && std::time::Instant::now() < deadline {
        Control::take_reported_edits(controller, main, &mut heard);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    heard
}

/// A parameter the plugin's own editor moves is reported: the editor's write is
/// `AUParameterSet` with no sending listener, which tells every listener
/// (`AudioUnitUtilities.h`), and the report is the value on the declared range
/// (Dry/Wet is `[0, 100]`, so 25 is 0.25).
#[test]
fn an_edit_in_the_plugins_own_editor_is_reported() {
    let mut au = delay_loaded();
    assert!(au.watch_properties());
    let (_processor, mut controller) = activate(au);

    tutti_au_ffi::set_parameter_notifying(
        controller.unit(),
        DRY_WET,
        tutti_au_ffi::EventAddress::GLOBAL,
        25.0,
        0,
    )
    .unwrap();

    let heard = wait_for_edits(&mut controller, 1);
    assert_eq!(heard.len(), 1, "one edit: {heard:?}");
    assert_eq!(heard[0].id(), ParamId::from(DRY_WET));
    let value = heard[0].value().expect("a value change is a Change");
    assert!(
        (value.get() - 0.25).abs() < 1e-6,
        "normalized on the declared range: {value:?}"
    );
}

/// A gesture the plugin's own editor makes is reported as its three edits, in
/// the order it made them: the begin
/// (`kAudioUnitEvent_BeginParameterChangeGesture`), the change, the end
/// (`EndParameterChangeGesture`, `AudioUnitUtilities.h`), on Dry/Wet's declared
/// range `[0, 100]`. The begin and the end carry no value in AU. The listener's
/// queue is serial, so the order is the order of the notifications. One change,
/// not two: AudioToolbox coalesces the value changes of a parameter that arrive
/// within the listener's notification interval into the last, while the gesture
/// events are never merged with them.
#[test]
fn a_gesture_in_the_plugins_own_editor_is_reported_in_order() {
    use tutti_au_ffi::{EventAddress, emit_gesture, set_parameter_notifying};

    let mut au = delay_loaded();
    assert!(au.watch_properties());
    let (_processor, mut controller) = activate(au);
    let global = EventAddress::GLOBAL;

    emit_gesture(controller.unit(), DRY_WET, global, true).unwrap();
    set_parameter_notifying(controller.unit(), DRY_WET, global, 75.0, 0).unwrap();
    emit_gesture(controller.unit(), DRY_WET, global, false).unwrap();

    let heard = wait_for_edits(&mut controller, 3);
    let id = ParamId::from(DRY_WET);
    assert_eq!(heard.len(), 3, "the gesture, in order: {heard:?}");
    assert_eq!(heard[0], ParamEdit::Begin(id));
    assert_eq!(heard[2], ParamEdit::End(id));
    let ParamEdit::Change {
        id: changed,
        value,
        record,
    } = heard[1]
    else {
        panic!("the middle edit is a change: {heard:?}");
    };
    assert_eq!(changed, id);
    assert!(record);
    assert!(
        (value.get() - 0.75).abs() < 1e-6,
        "on the declared range: {value:?}"
    );
}

/// The host's own writes are not the plugin's edits: automation through the
/// processor and a `set_parameter` of the host both name the host's listener as
/// their sender, which `AUParameterSet` leaves out of the notification. Were
/// they heard, every recorded automation point would come back as an edit.
/// The plugin's edit of another parameter, written after them, is what proves
/// the listener was live and had delivered by then.
#[test]
fn the_hosts_own_writes_are_not_reported_as_the_plugins_edits() {
    let mut au = delay_loaded();
    assert!(au.watch_properties());
    // A host write of Feedback (id 2) before activation.
    au.set_parameter(2, 10.0).unwrap();
    let (mut processor, mut controller) = activate(au);

    let mut changes = ParameterChanges::reserved(1, 4);
    assert!(changes.add_change(ParamId::from(DRY_WET), 0, 0.5));
    render_block(&mut processor, 0.0, &changes);
    assert_eq!(plain(&controller, DRY_WET), 50.0, "the automation landed");

    // The plugin's own edit, of Lowpass Cutoff (id 3).
    tutti_au_ffi::set_parameter_notifying(
        controller.unit(),
        3,
        tutti_au_ffi::EventAddress::GLOBAL,
        1000.0,
        0,
    )
    .unwrap();
    let heard = wait_for_edits(&mut controller, 1);
    // Past the listener's notification interval, so a delayed report of a host
    // write would be here too.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let mut more = wait_for_edits(&mut controller, 0);
    let mut all = heard;
    all.append(&mut more);
    let ids: Vec<u32> = all.iter().map(|e| e.id().get()).collect();
    assert_eq!(ids, [3], "only the plugin's own edit: {all:?}");
}

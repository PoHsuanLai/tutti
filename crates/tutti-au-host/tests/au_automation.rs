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
    AudioBuffer, AudioBufferMut, Loaded, MainThread, MaxBlock, ParamId, ParameterChanges, PluginIo,
    Prepare, ProcessContext, ProcessOutput, Processor, SampleRate,
};

const RATE: f64 = 48_000.0;
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
    AuLoaded::new(comp, RATE, BLOCK as u32).unwrap()
}

fn activate(au: AuLoaded) -> (AuProcessor, AuController) {
    let prepare = Prepare::new(
        SampleRate(RATE),
        MaxBlock::of(tutti_types::Samples(BLOCK as _)),
    )
    .unwrap();
    au.activate(&MainThread::claim().unwrap(), prepare)
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
            sample_rate: RATE,
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
    let main = MainThread::claim().unwrap();
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

    Control::flush_edits(&mut controller, &main);
    assert_eq!(
        plain(&controller, DELAY_TIME),
        2.0,
        "the control side's tick applies the deferred write, at the declared maximum"
    );
}

/// What a listener on another queue is given a moment to deliver.
fn wait_for_edits(
    controller: &mut AuController,
    want: usize,
) -> Vec<(ParamId, tutti_plugin_types::Normalized)> {
    use tutti_plugin_types::Control;
    let main = MainThread::claim().unwrap();
    let mut heard = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while heard.len() < want && std::time::Instant::now() < deadline {
        Control::take_reported_edits(controller, &main, &mut heard);
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
    assert_eq!(heard[0].0, ParamId::from(DRY_WET));
    assert!(
        (heard[0].1.get() - 0.25).abs() < 1e-6,
        "normalized on the declared range: {:?}",
        heard[0].1
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
    let ids: Vec<u32> = all.iter().map(|(id, _)| id.get()).collect();
    assert_eq!(ids, [3], "only the plugin's own edit: {all:?}");
}

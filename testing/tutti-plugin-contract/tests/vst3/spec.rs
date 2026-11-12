//! What VST3 asks of a host that the other formats do not: controllers as
//! parameters (`IMidiMapping`), no event for the rest of MIDI 1.0, a non-null
//! parameter list, `outputParameterChanges` to the controller, the editor's
//! edits and the parameter flush. Each case runs against both VST3 plugins, the
//! Rust one (a combined component and controller) and the C++ one (separate
//! halves), through the host's typed activation.
//!
//! The plugins' side of each is the spec in `tutti-shared-plugin`'s `core`
//! module, "VST3 controllers and flushes".

use tutti_fixtures::{main_thread, midi};
use tutti_graph::{Offset, UmpAt};
use tutti_plugin_contract::spec::{Mode, Param};
use tutti_plugin_contract::{
    Active, BLOCK, Format, Kind, RATE, Render, Vst3, Vst3Cpp, prepare, select, set_mode, transcript,
};
use tutti_plugin_types::{Control, Normalized, ProcessContext, Processor, SampleFormat, Samples};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup};
use tutti_vst3_host::{Vst3Loaded, Vst3Processor};

fn active<F: Format<L = Vst3Loaded>>() -> Active<Vst3Loaded> {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Tag);
    Active::activate(loaded, prepare(RATE, BLOCK, SampleFormat::Float32))
        .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
}

fn at(frame: usize, ump: tutti_ump::Ump) -> UmpAt {
    (Offset::new(frame, Samples(BLOCK)).unwrap(), ump)
}

fn silent_block(a: &mut Active<Vst3Loaded>, midi: &[UmpAt]) {
    let ins = [vec![0.0f32; BLOCK], vec![0.0; BLOCK]];
    a.render(&ins, BLOCK, &ProcessContext::new().midi(midi));
}

fn cc(channel: u8, controller: u8, value: u8) -> tutti_ump::Ump {
    midi::cc(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        CCNumber::new(controller).unwrap(),
        midi::wide_value(value),
    )
}

/// `IMidiMapping` maps controller 74 on bus 0, channel 0 to `level`
/// (ivsteditcontroller.h: "any functionality that is to be controlled by MIDI
/// controllers must be exported as regular parameter. The host will transform
/// incoming MIDI controller data using this interface"). The table is read at
/// activation, not only after `kMidiCCAssignmentChanged`: the plugin here
/// never raises it. The controller reaches the plugin as a parameter point at
/// its frame, and as nothing else.
fn a_mapped_controller_reaches_the_plugin_as_a_parameter_point<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    silent_block(&mut a, &[at(9, cc(0, 74, 100))]);
    let t = transcript::<F, _>(a.controller_mut());
    let points: Vec<_> = t.of(Kind::Param).collect();
    assert_eq!(points.len(), 1, "one point for the controller: {points:?}");
    let p = points[0];
    assert_eq!((p.id, p.offset), (Param::Level.id().get(), 9));
    assert!((p.value - 100.0 / 127.0).abs() < 0.01, "value {}", p.value);
    assert_eq!(t.of(Kind::Data).count(), 0, "and not as a data event");
}

/// Everything else of MIDI 1.0 has no VST3 event: `DataEvent` has one type,
/// `kMidiSysEx` (ivstevents.h), and a plugin reads it as SysEx. A controller no
/// mapping takes (another number, another channel), a pitch bend, a program
/// change and channel pressure are not sent as one.
fn unmapped_channel_messages_are_not_sent_as_sysex_data<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    let events = [
        at(1, cc(0, 7, 90)),
        at(2, cc(1, 74, 90)),
        at(
            3,
            midi::pitch_bend(MidiGroup::FIRST, MidiChannel::of(0), 0x9000_0000),
        ),
        at(
            4,
            midi::program_change(MidiGroup::FIRST, MidiChannel::of(0), 5, None),
        ),
        at(
            5,
            midi::channel_pressure(MidiGroup::FIRST, MidiChannel::of(0), 0x4000_0000),
        ),
    ];
    silent_block(&mut a, &events);
    let t = transcript::<F, _>(a.controller_mut());
    let heard: Vec<_> = t.events.iter().collect();
    assert!(heard.is_empty(), "the plugin heard {heard:?}");
}

/// `inputParameterChanges` is not annotated optional in `ProcessData`
/// (ivstaudioprocessor.h): a block with no parameter changes passes an empty
/// list, never null.
fn the_input_parameter_list_is_never_null<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    silent_block(&mut a, &[]);
    silent_block(&mut a, &[]);
    let t = transcript::<F, _>(a.controller_mut());
    assert_eq!(t.calls, 2);
    assert_eq!(t.of(Kind::NullParams).count(), 0, "a block got a null list");
}

/// What the processor writes into `outputParameterChanges` reaches the edit
/// controller as `setParamNormalized` (the portal's "Processor and Controller":
/// the host transmits them). The plugin reports `aux = key / 127` for a note on
/// channel 15; its own `aux` does not move, so the controller reads it only if
/// the host delivered the point.
fn output_parameter_changes_reach_the_controller<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    let key = 100;
    let note = midi::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(15),
        key,
        midi::wide_velocity(90),
    );
    silent_block(&mut a, &[at(5, note)]);
    let main = main_thread();
    let before = a
        .controller()
        .parameter(&main, Param::Aux.id())
        .map(Normalized::get);
    a.controller_mut().flush_edits(&main);
    let after = a
        .controller()
        .parameter(&main, Param::Aux.id())
        .map(Normalized::get);
    assert_eq!(before, Some(0.25), "nothing is delivered before the tick");
    let want = f64::from(key) / 127.0;
    assert!(
        after.is_some_and(|v| (v - want).abs() < 1e-9),
        "the controller reads {after:?}, the plugin reported {want}"
    );
}

/// The edit a plugin's editor performs through the host's component handler
/// reaches the processor: `take_reported_edits` reports it, and the next block
/// carries it as a parameter point (a separate controller has no other way to
/// tell its component). The plugin performs one (`aux` to 0.75) on each state
/// load once told to.
fn an_editors_edit_reaches_the_processor<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    select::<F, _>(a.controller_mut(), 0, 1);
    let main = main_thread();
    let mut edits = Vec::new();
    a.controller_mut().take_reported_edits(&main, &mut edits);
    assert_eq!(
        edits,
        [(Param::Aux.id(), Normalized::clamped(0.75))],
        "reported to the host"
    );
    silent_block(&mut a, &[]);
    let t = transcript::<F, _>(a.controller_mut());
    let aux: Vec<_> = t
        .of(Kind::Param)
        .filter(|e| e.id == Param::Aux.id().get())
        .collect();
    assert!(
        aux.last().is_some_and(|e| e.value == 0.75),
        "the processor heard {aux:?}"
    );
}

/// A parameter written while the plugin is active but not processing reaches
/// the processor with a `process` call of no samples and no buses (the portal's
/// FAQ, "Processing"): applied at once, with no block counted. A Processor that
/// is processing does not flush: the next block carries the write.
fn a_parameter_written_while_not_processing_is_flushed<F: Format<L = Vst3Loaded>>() {
    let mut a = active::<F>();
    let main = main_thread();
    a.controller_mut()
        .set_parameter(&main, Param::Aux.id(), Normalized::clamped(0.9));
    assert!(
        !a.processor().flush_parameters().unwrap(),
        "it is processing"
    );
    a.processor().stop();
    assert!(a.processor().flush_parameters().unwrap(), "it is stopped");
    let t = transcript::<F, _>(a.controller_mut());
    assert_eq!(t.of(Kind::Flush).count(), 1);
    assert_eq!(
        (t.calls, t.aux),
        (0, 0.9),
        "no block, and the value applied"
    );
    let points: Vec<_> = t.of(Kind::Param).map(|e| (e.id, e.value)).collect();
    assert_eq!(points, [(Param::Aux.id().get(), 0.9)]);
    // Nothing is queued now: a second flush calls nothing.
    assert!(!a.processor().flush_parameters().unwrap());
}

macro_rules! vst3_cases {
    ($($case:ident),*) => {
        mod rust { use super::*; $( #[test] fn $case() { super::$case::<Vst3>() } )* }
        mod cpp { use super::*; $( #[test] fn $case() { super::$case::<Vst3Cpp>() } )* }
    };
}

vst3_cases!(
    a_mapped_controller_reaches_the_plugin_as_a_parameter_point,
    unmapped_channel_messages_are_not_sent_as_sysex_data,
    the_input_parameter_list_is_never_null,
    output_parameter_changes_reach_the_controller,
    an_editors_edit_reaches_the_processor,
    a_parameter_written_while_not_processing_is_flushed
);

#[allow(dead_code)]
fn _typed(_: &Vst3Processor<f32>) {}

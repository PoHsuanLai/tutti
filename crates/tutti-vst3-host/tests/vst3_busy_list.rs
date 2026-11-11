//! What a block does while a plugin thread holds the input event list.
//!
//! The host never waits on the plugin: `begin` and `clear` try the borrow, and so
//! does the staging of the parameter lists and their queues. A
//! list it could not stage still holds the last block's events, so the block
//! must not be processed (the plugin would replay them).

#![cfg(feature = "conformance")]

#[path = "support/active.rs"]
mod active;

use active::{Active, ActiveExt};
use tutti_vst3_host::{AudioBuffer, InputEvents, TransportInfo};

/// **A block skipped for a busy input list is silence, and counted.** The
/// shared plugin's default mode renders `input + tag`, so a processed block is
/// never zero and a skipped one is.
///
/// Mutation (run, fails): have `process` ignore the `None` from
/// `stage_input_events` (stage nothing, run the plugin anyway) -> the held
/// block comes out `input + tag`, not silence, and `lists_busy` is still
/// counted, so the silence assertion is the one that fails.
#[test]
fn a_block_is_skipped_while_the_plugin_holds_the_input_list() {
    let library = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let mut plugin = Active::<f32>::load(&library, 48_000.0, 16).expect("the plugin loads");
    let input = [[1.0f32; 16]; 2];
    let transport = TransportInfo::default();
    let render = |plugin: &mut Active| {
        let mut out = [[9.0f32; 16]; 2];
        let [l, r] = &mut out;
        let outs: &mut [&mut [f32]] = &mut [&mut l[..], &mut r[..]];
        let ins: &[&[f32]] = &[&input[0][..], &input[1][..]];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let _ = plugin.process(&mut buffer, &InputEvents::default(), None, &transport);
        out
    };

    let free = render(&mut plugin);
    assert!(
        free[0].iter().all(|&s| s > 1.0),
        "processed: input plus tag"
    );
    assert_eq!(plugin.processor().lists_busy(), 0);

    let list = plugin.processor().input_event_list();
    let held = {
        let _hold = list.hold_as_plugin();
        render(&mut plugin)
    };
    assert!(
        held.iter().flatten().all(|&s| s == 0.0),
        "a skipped block is silence"
    );
    assert_eq!(plugin.processor().lists_busy(), 1);

    let after = render(&mut plugin);
    assert!(
        after[0].iter().all(|&s| s > 1.0),
        "the next block is processed"
    );
    assert_eq!(plugin.processor().lists_busy(), 1, "and counts nothing");
}

/// **A block skipped for a parameter queue a plugin thread still holds is
/// silence, and counted, never a panic.** A plugin whose worker is still
/// reading a queue (`getPoint`) when the host stages the next block used to
/// panic the audio thread in the host's `borrow_mut`.
///
/// Mutation (run, fails): `ParamValueQueueImpl::try_stage` on a panicking
/// `borrow_mut` -> the held block panics ("concurrent borrow detected")
/// instead of coming out silent.
#[test]
fn a_block_is_skipped_while_the_plugin_holds_a_parameter_queue() {
    let library = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let mut plugin = Active::<f32>::load(&library, 48_000.0, 16).expect("the plugin loads");
    let input = [[1.0f32; 16]; 2];
    let transport = TransportInfo::default();
    let mut automation = tutti_plugin_types::ParameterChanges::reserved(1, 4);
    automation.add_change(0u32.into(), 0, 0.5);
    let render = |plugin: &mut Active| {
        let mut out = [[9.0f32; 16]; 2];
        let [l, r] = &mut out;
        let outs: &mut [&mut [f32]] = &mut [&mut l[..], &mut r[..]];
        let ins: &[&[f32]] = &[&input[0][..], &input[1][..]];
        let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
        let _ = plugin.process(
            &mut buffer,
            &InputEvents::default(),
            Some(&automation),
            &transport,
        );
        out
    };

    let free = render(&mut plugin);
    assert!(free[0].iter().all(|&s| s > 1.0), "processed");
    assert_eq!(plugin.processor().lists_busy(), 0);

    let list = plugin.processor().input_param_list();
    let held = {
        let _hold = list.hold_queue_as_plugin(0);
        render(&mut plugin)
    };
    assert!(
        held.iter().flatten().all(|&s| s == 0.0),
        "a skipped block is silence"
    );
    assert_eq!(plugin.processor().lists_busy(), 1);

    let after = render(&mut plugin);
    assert!(after[0].iter().all(|&s| s > 1.0), "the next is processed");
    assert_eq!(plugin.processor().lists_busy(), 1, "and counts nothing");
}

/// **A plugin that refuses `setProcessing(1)` fails the block that needs it.**
/// Processing resumes lazily after a stop, and the refusal used to be dropped
/// (`let _ =`), so the plugin rendered while the host believed it was
/// processing.
///
/// Mutation (run, fails): discard the result of `set_processing(true)` in
/// `Vst3Processor::process` again -> the block succeeds and the assertion on
/// the error fails.
#[test]
fn a_refused_set_processing_fails_the_block() {
    let library = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let mut plugin = Active::<f32>::load_class(
        &library,
        "Tutti Shared Test Plugin (processing refused)",
        48_000.0,
        16,
    )
    .expect("the plugin loads and activates");
    plugin.stop();
    let input = [[1.0f32; 16]; 2];
    let mut out = [[9.0f32; 16]; 2];
    let [l, r] = &mut out;
    let outs: &mut [&mut [f32]] = &mut [&mut l[..], &mut r[..]];
    let ins: &[&[f32]] = &[&input[0][..], &input[1][..]];
    let mut buffer = AudioBuffer::new(ins, outs, 48_000.0);
    let result = plugin.try_process(
        &mut buffer,
        &InputEvents::default(),
        None,
        &TransportInfo::default(),
    );
    assert!(result.is_err(), "the refusal fails the block");
    assert!(
        out.iter().flatten().all(|&s| s == 0.0),
        "and the block is silent"
    );
}

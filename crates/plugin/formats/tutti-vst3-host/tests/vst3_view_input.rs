//! Keyboard, wheel and focus delivery to a plugin editor: does the host pass
//! through exactly what it was handed, and does it read the plugin's answer?
//!
//! # Why the answer is the whole point
//!
//! `IPlugView::onKeyDown` returns `kResultTrue` when the plugin **consumed** the
//! key and something else when it did not. That single bit is VST3's arbitration
//! mechanism: it is how a host knows whether space toggled a plugin's own
//! playback or should still toggle the host transport. A host that ignores it
//! either double-handles every keystroke or steals keys from every plugin that
//! legitimately uses them — and it cannot know which plugins those are.
//!
//! So the assertions here are of two kinds: the three arguments arrive
//! unmangled, and `kResultTrue` maps to "consumed" while every other code —
//! `kResultFalse`, `kNotImplemented`, an error — maps to "not consumed". The
//! second is the half a naive `== kResultOk` or `!= kResultFalse` gets wrong,
//! and neither mistake is visible without a view that can be told what to
//! return.
//!
//! # Why a stub view rather than a real plugin
//!
//! A real plugin's answer is its own business — it may consume a key or not,
//! and both are correct — so it cannot pin the mapping. A view that records
//! what it was given and returns what the test tells it to can. The test drives
//! the host's **real** forwarding functions, the same ones the `Vst3Loaded`
//! methods call; only the `EditorState` around them is stubbed, because that
//! can only be built by `open_editor`.

#![cfg(feature = "conformance")]

use std::sync::{Arc, Mutex};

use tutti_vst3_host::host::{send_key_down, send_key_up, send_wheel, set_view_focus};

use vst3::Steinberg::{
    char16, int16, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue,
    tresult, FIDString, IPlugFrame, IPlugView, IPlugViewTrait, TBool, ViewRect,
};
use vst3::{Class, ComWrapper};

/// One input event, exactly as the view received it.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Input {
    KeyDown {
        key: char16,
        virtual_key: int16,
        modifiers: int16,
    },
    KeyUp {
        key: char16,
        virtual_key: int16,
        modifiers: int16,
    },
    Wheel {
        distance: f32,
    },
    Focus {
        state: bool,
    },
}

/// An `IPlugView` that records the input it is handed and answers with a
/// caller-chosen `tresult`, so the host's interpretation of that code is
/// observable.
struct RecordingView {
    inputs: Arc<Mutex<Vec<Input>>>,
    /// What every key and wheel entry point returns.
    answer: tresult,
}

impl Class for RecordingView {
    type Interfaces = (IPlugView,);
}

impl RecordingView {
    fn record(&self, input: Input) {
        self.inputs
            .lock()
            .expect("recording lock poisoned")
            .push(input);
    }
}

impl IPlugViewTrait for RecordingView {
    unsafe fn isPlatformTypeSupported(&self, _platform_type: FIDString) -> tresult {
        kResultOk
    }

    unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _ty: FIDString) -> tresult {
        kResultOk
    }

    unsafe fn removed(&self) -> tresult {
        kResultOk
    }

    unsafe fn onWheel(&self, distance: f32) -> tresult {
        self.record(Input::Wheel { distance });
        self.answer
    }

    unsafe fn onKeyDown(&self, key: char16, code: int16, modifiers: int16) -> tresult {
        self.record(Input::KeyDown {
            key,
            virtual_key: code,
            modifiers,
        });
        self.answer
    }

    unsafe fn onKeyUp(&self, key: char16, code: int16, modifiers: int16) -> tresult {
        self.record(Input::KeyUp {
            key,
            virtual_key: code,
            modifiers,
        });
        self.answer
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        unsafe {
            *size = ViewRect {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300,
            };
        }
        kResultOk
    }

    unsafe fn onSize(&self, _new_size: *mut ViewRect) -> tresult {
        kResultOk
    }

    unsafe fn onFocus(&self, state: TBool) -> tresult {
        self.record(Input::Focus { state: state != 0 });
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }

    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultFalse
    }

    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }
}

/// Build a view that answers `answer` to every key and wheel call, returning
/// it alongside the log it records into.
fn view_answering(answer: tresult) -> (ComWrapper<RecordingView>, Arc<Mutex<Vec<Input>>>) {
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let view = ComWrapper::new(RecordingView {
        inputs: Arc::clone(&inputs),
        answer,
    });
    (view, inputs)
}

fn recorded(log: &Arc<Mutex<Vec<Input>>>) -> Vec<Input> {
    log.lock().expect("recording lock poisoned").clone()
}

/// The three key arguments reach the plugin exactly as given.
///
/// A host that swapped the character and virtual-key arguments — they are
/// adjacent, and both are small integers — would still compile and still
/// deliver *something* for every keystroke. The plugin would see garbage.
#[test]
fn a_key_reaches_the_plugin_with_its_character_code_and_modifiers() {
    let (view, log) = view_answering(kResultFalse);
    let ptr = view.as_com_ref::<IPlugView>().unwrap().to_com_ptr();

    // A character keystroke: character set, virtual key zero.
    let _ = send_key_down(&ptr, 'k' as char16, 0, 0b0011);
    // A non-character keystroke: virtual key set, character zero.
    let _ = send_key_up(&ptr, 0, 42, 0b0100);

    assert_eq!(
        recorded(&log),
        vec![
            Input::KeyDown {
                key: 'k' as char16,
                virtual_key: 0,
                modifiers: 0b0011,
            },
            Input::KeyUp {
                key: 0,
                virtual_key: 42,
                modifiers: 0b0100,
            },
        ],
        "the host altered a key event on its way to the plugin"
    );
}

/// `kResultTrue` — and only `kResultTrue` — means the plugin consumed the key.
///
/// This is the assertion the whole arbitration rests on. `kResultOk` and
/// `kResultTrue` are **the same value** in VST3, so a host writing
/// `== kResultOk` happens to be right; one writing `!= kResultFalse` is wrong,
/// because it reads `kNotImplemented` from a plugin with no key handling as
/// "consumed" and swallows every shortcut the host owns.
#[test]
fn only_a_true_result_counts_as_consumed() {
    for (answer, expected, label) in [
        (kResultTrue, true, "kResultTrue"),
        (kResultFalse, false, "kResultFalse"),
        (kNotImplemented, false, "kNotImplemented"),
        (kInvalidArgument, false, "kInvalidArgument"),
    ] {
        let (view, _log) = view_answering(answer);
        let ptr = view.as_com_ref::<IPlugView>().unwrap().to_com_ptr();

        assert_eq!(
            send_key_down(&ptr, 'a' as char16, 0, 0),
            expected,
            "onKeyDown returning {label} should read as consumed={expected}"
        );
        assert_eq!(
            send_key_up(&ptr, 'a' as char16, 0, 0),
            expected,
            "onKeyUp returning {label} should read as consumed={expected}"
        );
        assert_eq!(
            send_wheel(&ptr, 1.0),
            expected,
            "onWheel returning {label} should read as consumed={expected}"
        );
    }
}

/// Wheel distance arrives with its sign and magnitude intact.
///
/// Scroll direction is a signed quantity and the two directions are otherwise
/// indistinguishable, so an inverted or truncated distance is invisible until
/// someone scrolls a plugin's knob the wrong way.
#[test]
fn wheel_distance_keeps_its_sign_and_magnitude() {
    let (view, log) = view_answering(kResultFalse);
    let ptr = view.as_com_ref::<IPlugView>().unwrap().to_com_ptr();

    for distance in [1.0f32, -1.0, 0.5, -2.25] {
        let _ = send_wheel(&ptr, distance);
    }

    assert_eq!(
        recorded(&log),
        vec![
            Input::Wheel { distance: 1.0 },
            Input::Wheel { distance: -1.0 },
            Input::Wheel { distance: 0.5 },
            Input::Wheel { distance: -2.25 },
        ],
        "the host altered a wheel distance on its way to the plugin"
    );
}

/// Both focus transitions reach the plugin, not just the gaining one.
///
/// A host that forwards only `true` leaves every plugin it has ever focused
/// believing it still has the keyboard — they draw focus rings that never
/// clear, and some keep handling keys they should have released.
#[test]
fn both_focus_transitions_are_delivered() {
    let (view, log) = view_answering(kResultOk);
    let ptr = view.as_com_ref::<IPlugView>().unwrap().to_com_ptr();

    set_view_focus(&ptr, true);
    set_view_focus(&ptr, false);

    assert_eq!(
        recorded(&log),
        vec![Input::Focus { state: true }, Input::Focus { state: false },],
        "a focus transition was dropped or its state inverted"
    );
}

/// The stub records what it claims to record.
///
/// Without this, every assertion above would also pass against a view whose
/// entry points were never reached at all — an empty log compared against an
/// empty expectation.
#[test]
fn the_recording_view_observes_input() {
    let (view, log) = view_answering(kResultFalse);
    assert!(
        recorded(&log).is_empty(),
        "the log should start empty, or the tests above prove nothing"
    );

    let ptr = view.as_com_ref::<IPlugView>().unwrap().to_com_ptr();
    let _ = send_key_down(&ptr, 'x' as char16, 0, 0);

    assert_eq!(
        recorded(&log).len(),
        1,
        "the view recorded nothing, so it cannot witness what the host sent"
    );
}

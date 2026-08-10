//! Editor lifecycle rules that need no display: the teardown call *order*, and
//! which `isPlatformTypeSupported` answers count as a refusal.
//!
//! # Teardown ordering
//!
//! Does the host retract its `IPlugFrame` before it tells the view it is being
//! removed?
//!
//! `editorhost`'s `WindowController::closePlugView` calls `setFrame(nullptr)`
//! and *then* `removed()`. The order matters because the host's frame object
//! dies with the editor session: a plugin still holding the pointer during
//! `removed()` — to report one last `resizeView`, say — would call into memory
//! the host is about to free.
//!
//! ## Why a stub view rather than a real plugin
//!
//! The two calls leave no trace a loaded plugin exposes: `host-checker` logs
//! that `setFrame` was *used* (`kLogIdIPlugViewsetFrameSupported`) and flags
//! unpaired `attached`/`removed`, but records nothing about their relative
//! order. Reading the order off the plugin side is therefore impossible.
//!
//! A view that records its own calls sees it directly and needs no display.
//! The test drives the host's **real** `detach_view` — the same function
//! `close_editor_unchecked` calls — rather than a copy of the sequence, so
//! reverting the fix fails the test. Only the `EditorState` around it is
//! stubbed, because that can only be built by `open_editor`.

#![cfg(feature = "conformance")]

use std::sync::{Arc, Mutex};

use tutti_vst3_host::host::{detach_view, platform_type_refused};

use vst3::Steinberg::{
    char16, int16, kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue,
    tresult, FIDString, IPlugFrame, IPlugView, IPlugViewTrait, TBool, ViewRect,
};
use vst3::{Class, ComWrapper};

/// One `IPlugView` entry point, as observed by [`RecordingView`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Call {
    IsPlatformTypeSupported,
    Attached,
    Removed,
    /// `setFrame` with a real frame pointer.
    SetFrame,
    /// `setFrame(nullptr)` — the retraction this test is about.
    ClearFrame,
}

/// An `IPlugView` that records the order in which the host calls it.
struct RecordingView {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Class for RecordingView {
    type Interfaces = (IPlugView,);
}

impl RecordingView {
    fn record(&self, call: Call) {
        self.calls
            .lock()
            .expect("recording lock poisoned")
            .push(call);
    }
}

impl IPlugViewTrait for RecordingView {
    unsafe fn isPlatformTypeSupported(&self, _type: FIDString) -> tresult {
        self.record(Call::IsPlatformTypeSupported);
        kResultTrue
    }

    unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _type: FIDString) -> tresult {
        self.record(Call::Attached);
        kResultOk
    }

    unsafe fn removed(&self) -> tresult {
        self.record(Call::Removed);
        kResultOk
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        self.record(if frame.is_null() {
            Call::ClearFrame
        } else {
            Call::SetFrame
        });
        kResultOk
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: char16, _code: int16, _modifiers: int16) -> tresult {
        kResultFalse
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

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }

    unsafe fn checkSizeConstraint(&self, _rect: *mut ViewRect) -> tresult {
        kResultFalse
    }
}

/// Run the host's real teardown against a recording view.
///
/// `detach_view` is the function `close_editor_unchecked` calls; nothing about
/// the sequence is reproduced here.
fn teardown(view: &ComWrapper<RecordingView>) {
    let view = view
        .as_com_ref::<IPlugView>()
        .expect("IPlugView is the only interface on RecordingView");
    // `detach_view` takes the owning `ComPtr` the host holds in `EditorState`.
    // `to_com_ptr` bumps the refcount, so the wrapper outlives the call.
    detach_view(&view.to_com_ptr());
}

/// The frame must be retracted before `removed()`, never after and never not
/// at all — the defect `editorhost` avoids by construction.
#[test]
fn clears_frame_before_removed() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let view = ComWrapper::new(RecordingView {
        calls: Arc::clone(&calls),
    });

    teardown(&view);

    let calls = calls.lock().expect("recording lock poisoned").clone();
    let clear = calls
        .iter()
        .position(|c| *c == Call::ClearFrame)
        .unwrap_or_else(|| panic!("teardown never called setFrame(nullptr); saw {calls:?}"));
    let removed = calls
        .iter()
        .position(|c| *c == Call::Removed)
        .unwrap_or_else(|| panic!("teardown never called removed(); saw {calls:?}"));

    assert!(
        clear < removed,
        "setFrame(nullptr) must precede removed(), as in editorhost's \
         closePlugView; saw {calls:?}"
    );
}

/// A view that recorded nothing would make the ordering assertion vacuously
/// true, so pin that the stub actually observes calls.
#[test]
fn recording_view_observes_calls() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let view = ComWrapper::new(RecordingView {
        calls: Arc::clone(&calls),
    });

    assert!(
        calls.lock().expect("recording lock poisoned").is_empty(),
        "no calls should be recorded before teardown"
    );
    teardown(&view);
    assert_eq!(
        *calls.lock().expect("recording lock poisoned"),
        vec![Call::ClearFrame, Call::Removed],
        "teardown should perform exactly these two calls, in this order"
    );
}

// ── isPlatformTypeSupported: which answers are refusals ───────────────────────

/// An explicit "no" must be honoured — this is the whole point of asking.
/// `kInvalidArgument` is what `VSTGUIEditor` returns for a platform type it
/// does not handle, so it is the realistic refusal, not a theoretical one.
#[test]
fn explicit_denial_is_a_refusal() {
    assert!(
        platform_type_refused(kResultFalse),
        "kResultFalse is an explicit no"
    );
    assert!(
        platform_type_refused(kInvalidArgument),
        "kInvalidArgument is how VSTGUIEditor refuses an unhandled type"
    );
}

/// `kNotImplemented` must **not** be treated as a refusal.
///
/// This is the regression that matters. The SDK's `CPluginView` returns it
/// unconditionally while its `attached` succeeds, so a host that rejects it
/// refuses to open editors that work — a strictly worse bug than the one the
/// check was added to fix. `editorhost` gets away with `!= kResultTrue` because
/// it is a test harness that may abort; a DAW may not.
#[test]
fn not_implemented_is_not_a_refusal() {
    assert!(
        !platform_type_refused(kNotImplemented),
        "kNotImplemented means the view declined to answer, not that it said no; \
         rejecting it breaks every plugin built on the SDK's CPluginView"
    );
}

/// Success, and any unrecognised code, must fall through to `attached`.
///
/// Unknown codes are treated as permission rather than denial deliberately: a
/// false "unsupported" costs the user their editor, while a false "supported"
/// only reaches the failure the check exists to catch.
#[test]
fn success_and_unknown_codes_are_not_refusals() {
    assert!(!platform_type_refused(kResultTrue), "kResultTrue is a yes");
    assert!(
        !platform_type_refused(kResultOk),
        "kResultOk aliases kResultTrue in VST3"
    );
    for code in [1234, -9999, i32::MAX, i32::MIN] {
        assert!(
            !platform_type_refused(code),
            "unrecognised code {code} must not block the editor"
        );
    }
}

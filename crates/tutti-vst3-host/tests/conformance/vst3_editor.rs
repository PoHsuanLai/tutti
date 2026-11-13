//! The editor lifecycle through the host: open and close, size, resize requests
//! and constraints, keyboard and wheel input.
//!
//! The witness is the shared test plugin (`testing/tutti-shared-plugin`), whose
//! VST3 shell has an editor view that draws nothing and ignores its parent, so
//! the host is handed `tutti_fixtures::StandInWindow` and no display is needed,
//! on every platform. Its behaviour is a byte of the plugin's state (the
//! `EDITOR_*` of its spec), set the way the contract sets a mode: save, patch,
//! load. Only the run-loop registrations are Linux's.

#![cfg(feature = "conformance")]

use tutti_fixtures::{StandInWindow, main_thread};
use tutti_plugin_types::{Control, EditorWindow};
use tutti_vst3_host::{ContentScale, EditorSize, KeyEvent, LoadOptions, Vst3Loaded};

/// `core::EDITOR_*`, from the plugin's spec.
const NONE: u32 = 0;
const TIDY: u32 = 1;
const LIVE: u32 = 5;

/// The size every view opens at, and what `EDITOR_LIVE` asks its frame for once
/// attached, clamps to at least and at most.
const OPEN: EditorSize = EditorSize {
    width: 200,
    height: 100,
};
const REQUESTED: EditorSize = EditorSize {
    width: 300,
    height: 150,
};
const SMALLEST: EditorSize = EditorSize {
    width: 100,
    height: 50,
};
const LARGEST: EditorSize = EditorSize {
    width: 800,
    height: 600,
};

/// The shared plugin, loaded, with its editor behaviour set through its state.
fn plugin_with_editor(editor: u32) -> Vst3Loaded {
    let main = main_thread();
    let module = tutti_fixtures::vst3_module();
    let mut plugin = Vst3Loaded::load(&main, &module, LoadOptions::new())
        .unwrap_or_else(|e| panic!("the shared plugin at {} loads: {e:?}", module.display()));
    let mut state = plugin.save_state(&main).expect("the plugin saves");
    // The plugin's blob is in the state verbatim; the second byte of its mode
    // word, 4 bytes after the magic, is the editor behaviour.
    let at = state
        .windows(4)
        .position(|w| w == b"TSP1")
        .expect("the plugin's blob is in its state")
        + 4;
    state[at..at + 4].copy_from_slice(&(editor << 8).to_le_bytes());
    plugin
        .load_state(&main, &state)
        .expect("the plugin takes its configuration");
    plugin
}

fn open(plugin: &mut Vst3Loaded) -> Result<EditorSize, tutti_plugin_types::PluginError> {
    plugin.open_editor(
        &main_thread(),
        StandInWindow.handle(),
        ContentScale::IDENTITY,
    )
}

/// What the host's run loop holds for the plugin: a timer and a descriptor on
/// Linux, where the view registers them, and nothing elsewhere.
fn registered() -> (usize, usize) {
    if cfg!(target_os = "linux") {
        (1, 1)
    } else {
        (0, 0)
    }
}

fn run_loop_holds(plugin: &Vst3Loaded) -> (usize, usize) {
    let a = plugin.run_loop_activity();
    (a.timers_registered, a.event_handlers_registered)
}

/// **Each open attaches the view once and each close removes it once.** A second
/// attach without a remove leaks a registration (counted on Linux), and a
/// `removed` without an `attached`, which closing an editor that never opened
/// would be, aborts the plugin.
#[test]
fn editor_open_close_pairs_attach_and_remove() {
    let mut plugin = plugin_with_editor(TIDY);
    let main = main_thread();

    // Nothing is open: a close does not reach the view.
    plugin.close_editor(&main);
    for cycle in 0..3 {
        let size = open(&mut plugin).unwrap_or_else(|e| panic!("cycle {cycle}: {e:?}"));
        assert_eq!(size, OPEN, "cycle {cycle}: the view's own size");
        assert_eq!(run_loop_holds(&plugin), registered(), "cycle {cycle}: open");
        plugin.close_editor(&main);
        assert_eq!(run_loop_holds(&plugin), (0, 0), "cycle {cycle}: closed");
    }
    // A close with nothing open is a no-op, not a stray `removed`.
    plugin.close_editor(&main);
}

/// A plugin with no editor is refused cleanly, and nothing is left open.
#[test]
fn editorless_plugin_is_refused_cleanly() {
    let mut plugin = plugin_with_editor(NONE);
    open(&mut plugin).expect_err("the plugin offers no view, so open_editor must fail");
    // A close afterwards is a no-op, not a `removed` without an `attached`.
    plugin.close_editor(&main_thread());
}

/// The host asks `isPlatformTypeSupported` before `attached`, and a view that
/// supports this platform's type is not rejected by the check. The inverse (a
/// view that refuses) is in `vst3_view_teardown.rs`.
#[test]
fn supported_platform_type_still_opens() {
    let mut plugin = plugin_with_editor(TIDY);
    assert_eq!(
        open(&mut plugin).expect("the view supports this platform's type"),
        OPEN
    );
    plugin.close_editor(&main_thread());
}

/// `has_editor()` agrees with whether an editor opens, in both states.
///
/// Answering it with "there is a controller" answers a different question:
/// nearly every VST3 has an edit controller, and only some publish a view.
/// `tutti-plugin-server` feeds this into `Features::EDITOR`, so a wrong answer
/// offers an "open editor" affordance that fails when taken. The two states are
/// the contrast: a `has_editor` hardcoded either way fails one of them.
#[test]
fn has_editor_agrees_with_opening_one() {
    for (editor, expected) in [(NONE, false), (TIDY, true)] {
        let mut plugin = plugin_with_editor(editor);
        let claims = plugin.has_editor(&main_thread());
        let opened = open(&mut plugin).is_ok();
        plugin.close_editor(&main_thread());
        assert_eq!(
            claims,
            opened,
            "editor behaviour {editor}: has_editor()={claims} but open_editor() {}",
            if opened { "succeeded" } else { "failed" }
        );
        assert_eq!(opened, expected, "editor behaviour {editor}");
    }
}

/// A resizable view's constraint is the plugin's own, and a resize is granted at
/// the size it snaps to.
///
/// `canResize` and `checkSizeConstraint` decide whether a host's editor window
/// may be dragged. The view clamps to 100x50 at least and 800x600 at most, so an
/// absurd request that a host forwarded raw, or a constraint a host ignored,
/// comes back as something other than the clamp.
#[test]
fn editor_resize_respects_capabilities() {
    let mut plugin = plugin_with_editor(LIVE);
    let main = main_thread();
    assert_eq!(open(&mut plugin).expect("the view opens"), OPEN);
    assert!(
        plugin.editor_capabilities(&main).resize.resizable,
        "the view says it resizes"
    );

    let absurd = EditorSize {
        width: 10_000,
        height: 10_000,
    };
    assert_eq!(
        plugin.check_editor_size_constraint(&main, absurd),
        Some(LARGEST),
        "the view clamps an oversized request to its largest"
    );
    assert_eq!(
        plugin
            .resize_editor(&main, absurd)
            .expect("a snapped size is legal"),
        LARGEST,
        "the host applies the constrained rect, not the raw request"
    );

    let tiny = EditorSize {
        width: 10,
        height: 10,
    };
    assert_eq!(
        plugin
            .resize_editor(&main, tiny)
            .expect("a snapped size is legal"),
        SMALLEST,
        "the view clamps an undersized request to its smallest"
    );

    let within = EditorSize {
        width: 400,
        height: 300,
    };
    assert_eq!(
        plugin
            .resize_editor(&main, within)
            .expect("an in-range size is legal"),
        within,
        "a request within the limits is granted as asked"
    );
    plugin.close_editor(&main);
}

/// A closed editor reports no pending resize request, and an open one reports
/// the size its frame was asked for.
///
/// `poll_editor_resize_request` takes the latest size the view asked its
/// `IPlugFrame` for (300x150, when attached). The last assertion pins the guard
/// that stops a request still pending at a close from being applied to the next
/// editor.
#[test]
fn a_closed_editor_reports_no_resize_request() {
    let mut plugin = plugin_with_editor(LIVE);
    let main = main_thread();

    assert_eq!(
        plugin.poll_editor_resize_request(&main),
        None,
        "an editor that was never opened reported a pending resize"
    );
    open(&mut plugin).expect("the view opens");
    assert_eq!(
        plugin.poll_editor_resize_request(&main),
        Some(REQUESTED),
        "the view asked its frame for 300x150 when attached"
    );

    plugin.close_editor(&main);

    // The next open queues its request again; closing before it is taken must
    // drop it.
    open(&mut plugin).expect("the view opens again");
    plugin.close_editor(&main);
    assert_eq!(
        plugin.poll_editor_resize_request(&main),
        None,
        "a closed editor still reported a pending resize request: it would be \
         applied to whichever editor is opened next"
    );
}

/// The view's keys, wheel and focus are reachable while it is open, and only
/// then.
///
/// The stub-view suite (`vst3_view_input.rs`) pins the mapping of the arguments;
/// this shows a real view's handler is reached with them. The view consumes the
/// key `a`, the left arrow and a wheel step forward, and declines every other.
#[test]
fn editor_accepts_keyboard_wheel_and_focus() {
    let mut plugin = plugin_with_editor(LIVE);
    let main = main_thread();
    let a = || KeyEvent::character('a' as u16);

    // Before the editor exists every forwarder declines rather than reaching
    // through a missing view.
    assert!(!plugin.send_key_down(&main, a()).is_consumed());
    assert!(!plugin.send_wheel(&main, 1.0));
    plugin.set_editor_focus(&main, true);

    open(&mut plugin).expect("the view opens");
    plugin.set_editor_focus(&main, true);

    assert!(plugin.send_key_down(&main, a()).is_consumed(), "a");
    assert!(plugin.send_key_up(&main, a()).is_consumed(), "a, released");
    assert!(
        !plugin
            .send_key_down(&main, KeyEvent::character('b' as u16))
            .is_consumed(),
        "b is declined"
    );
    // VKEY_LEFT.
    assert!(
        plugin
            .send_key_down(&main, KeyEvent::virtual_key(11))
            .is_consumed(),
        "the left arrow"
    );
    assert!(plugin.send_wheel(&main, 1.0), "a wheel step forward");
    assert!(
        !plugin.send_wheel(&main, -1.0),
        "a wheel step back is declined"
    );

    plugin.set_editor_focus(&main, false);
    plugin.close_editor(&main);

    // And the forwarders decline again once it is gone.
    assert!(!plugin.send_key_down(&main, a()).is_consumed());
    assert!(!plugin.send_wheel(&main, 1.0));
}

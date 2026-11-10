use super::*;
use vst3::Steinberg::Vst::RestartFlags_;

/// Everything queued so far, oldest first.
fn drain(handler: &ComponentHandler) -> Vec<PluginNotification> {
    std::iter::from_fn(|| handler.pop_notification()).collect()
}

/// The plugin's edit gestures must reach the host in order and carry their
/// operands: a DAW writes automation from `PerformEdit` and opens/closes an
/// undo transaction on the surrounding `Begin`/`EndEdit`.
#[test]
fn edit_gestures_forward_in_order_with_their_operands() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        assert_eq!(ptr.beginEdit(42), kResultOk);
        assert_eq!(ptr.performEdit(42, 0.75), kResultOk);
        assert_eq!(ptr.endEdit(42), kResultOk);
    }

    let got = drain(&handler);
    assert_eq!(got.len(), 3);
    assert!(matches!(
        got[0],
        PluginNotification::Edit(ParameterEditEvent::BeginEdit(42))
    ));
    assert!(matches!(
        got[1],
        PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id: 42, value })
            if (value - 0.75).abs() < 1e-3
    ));
    assert!(matches!(
        got[2],
        PluginNotification::Edit(ParameterEditEvent::EndEdit(42))
    ));
}

/// `restartComponent` carries the raw flag word through unchanged — the
/// host decodes it with [`RestartFlags::from_bits`], so swallowing or
/// masking bits here would silently drop a latency or I/O change. It does
/// not occupy the notification queue.
#[test]
fn restart_component_forwards_the_raw_flag_word() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        assert_eq!(ptr.restartComponent(0b1010), kResultOk);
    }

    assert_eq!(handler.take_restart_bits(), 0b1010);
    assert!(handler.pop_notification().is_none());
}

/// Restart flags are cumulative: several requests between two takes reach
/// the host as their union, and a take leaves nothing behind for the next.
///
/// Mutation: `fetch_or` replaced by `store` in `restartComponent` (the
/// last request wins) fails the union assertion — the latency change is
/// lost under the later I/O change. `swap(0, ..)` replaced by `load` fails
/// the "second take is empty" assertion.
#[test]
fn restart_flags_merge_across_calls_and_reset_on_take() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        ptr.restartComponent(RestartFlags_::kLatencyChanged);
        ptr.restartComponent(RestartFlags_::kIoChanged);
        ptr.restartComponent(RestartFlags_::kLatencyChanged);
    }

    let flags = RestartFlags::from_bits(handler.take_restart_bits());
    assert!(flags.latency_changed, "the earlier request survives");
    assert!(flags.io_changed, "the later request lands too");
    assert!(!flags.param_values_changed);

    assert_eq!(handler.take_restart_bits(), 0, "a take consumes the flags");

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe { ptr.restartComponent(RestartFlags_::kParamValuesChanged) };
    assert_eq!(
        handler.take_restart_bits(),
        RestartFlags_::kParamValuesChanged,
        "a request after a take starts from zero"
    );
}

/// A full queue drops the newest notification and counts it; it neither
/// grows nor evicts what is already queued. The drop raises the sticky
/// overflow flag, which one take reports and clears.
///
/// Mutations: removing the `fetch_add` in `push` leaves the count at 0 and
/// fails the first assertion; removing the `overflowed.store` fails the
/// flag assertion; `take_overflowed` as a `load` (never cleared) fails
/// the "reported once" assertion.
#[test]
fn overflow_is_counted_and_keeps_the_queued_events() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    let extra = 3;
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        for i in 0..(NOTIFICATION_CAPACITY + extra) as u32 {
            ptr.performEdit(i, 0.5);
        }
    }

    assert_eq!(handler.dropped_notifications(), extra as u64);
    assert_eq!(handler.pending_notifications(), NOTIFICATION_CAPACITY);
    assert!(handler.take_overflowed(), "a drop raises the flag");
    assert!(!handler.take_overflowed(), "a take reports it once");
    let got = drain(&handler);
    assert_eq!(got.len(), NOTIFICATION_CAPACITY);
    assert!(
        matches!(
            got[0],
            PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id: 0, .. })
        ),
        "the oldest event is kept; the overflow is the newest"
    );

    // Room again once drained; the count is cumulative, and a push that
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    // fits does not raise the flag.
    unsafe { ptr.endEdit(7) };
    assert_eq!(drain(&handler).len(), 1);
    assert_eq!(handler.dropped_notifications(), extra as u64);
    assert!(!handler.take_overflowed());
}

/// The point of this module: a plugin calling the handler from its
/// process thread must not allocate there. Every callback runs inside
/// `assert_no_alloc`, `IProgress::start` with a description past the
/// inline capacity included, as does a push into a full queue, and so
/// does the host's side (`take_restart_bits`, `pop_notification`, and
/// dropping what it pops) — which is what the plugin server's restart
/// poll calls.
///
/// Mutations: the queue swapped for `crossbeam_queue::SegQueue` (unbounded,
/// allocating a block per 31 pushes — the growth the original
/// `crossbeam_channel::unbounded` sender had) aborts inside the gate, and
/// so does `ProgressDescription` holding a `String` (the old
/// `utf16_to_string` decode).
#[test]
fn callbacks_and_the_host_drain_do_not_allocate() {
    let handler = ComponentHandler::new();
    let edit = handler.to_com_ptr::<IComponentHandler>().unwrap();
    let edit2 = handler.to_com_ptr::<IComponentHandler2>().unwrap();
    let bus = handler
        .to_com_ptr::<IComponentHandlerBusActivation>()
        .unwrap();
    let progress = handler.to_com_ptr::<IProgress>().unwrap();
    let unit = handler.to_com_ptr::<IUnitHandler>().unwrap();
    let unit2 = handler.to_com_ptr::<IUnitHandler2>().unwrap();
    // Longer than the inline capacity, so `start` also truncates.
    let long: Vec<u16> = std::iter::repeat_n(u16::from(b'x'), 300)
        .chain([0])
        .collect();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_no_alloc::assert_no_alloc(|| unsafe {
        let mut id: ID = 0;
        progress.start(0, long.as_ptr(), &mut id);
        // Three edits per pass: two thirds of them meet a full queue.
        for i in 0..NOTIFICATION_CAPACITY as u32 {
            edit.beginEdit(i);
            edit.performEdit(i, 0.25);
            edit.endEdit(i);
            edit.restartComponent(RestartFlags_::kLatencyChanged);
        }
        edit2.setDirty(1);
        edit2.startGroupEdit();
        edit2.finishGroupEdit();
        edit2.requestOpenEditor(std::ptr::null());
        bus.requestBusActivation(0, 0, 0, 1);
        progress.update(1, 0.5);
        progress.finish(1);
        unit.notifyUnitSelection(1);
        unit.notifyProgramListChange(2, -1);
        unit2.notifyUnitByBusChange();

        assert_eq!(handler.take_restart_bits(), RestartFlags_::kLatencyChanged);
        let mut n = 0;
        while handler.pop_notification().is_some() {
            n += 1;
        }
        assert_eq!(n, NOTIFICATION_CAPACITY);
    });
    assert!(handler.dropped_notifications() > 0, "the full path ran");
}

#[test]
fn restart_flags_decode_individual_bits() {
    let latency = RestartFlags::from_bits(RestartFlags_::kLatencyChanged);
    assert!(latency.latency_changed);
    assert!(!latency.io_changed);
    assert!(!latency.param_values_changed);

    let io = RestartFlags::from_bits(RestartFlags_::kIoChanged);
    assert!(io.io_changed);
    assert!(!io.latency_changed);

    assert!(
        RestartFlags::from_bits(RestartFlags_::kMidiCCAssignmentChanged).midi_cc_assignment_changed
    );
    assert!(RestartFlags::from_bits(RestartFlags_::kReloadComponent).reload_component);
    assert!(RestartFlags::from_bits(RestartFlags_::kParamTitlesChanged).param_titles_changed);
    assert!(RestartFlags::from_bits(RestartFlags_::kParamValuesChanged).param_values_changed);
}

#[test]
fn restart_flags_decode_combined_bits() {
    let flags = RestartFlags::from_bits(
        RestartFlags_::kLatencyChanged
            | RestartFlags_::kIoChanged
            | RestartFlags_::kParamValuesChanged,
    );
    assert!(flags.latency_changed);
    assert!(flags.io_changed);
    assert!(flags.param_values_changed);
    assert!(!flags.param_titles_changed);
    assert!(!flags.reload_component);
}

#[test]
fn restart_flags_decode_zero_is_empty() {
    assert_eq!(RestartFlags::from_bits(0), RestartFlags::default());
}

/// `IProgress::start` mints the id the plugin then quotes back on
/// `update`/`finish`, so the id must be returned through the out-param
/// *and* carried on the event — a host that loses it cannot match a
/// progress bar to its completion.
#[test]
fn progress_start_mints_an_id_that_update_and_finish_quote_back() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IProgress>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let out_id = unsafe {
        let mut out_id: ID = 0;
        let desc: [u16; 5] = [b'T' as u16, b'e' as u16, b's' as u16, b't' as u16, 0];
        assert_eq!(ptr.start(1, desc.as_ptr(), &mut out_id), kResultOk);
        assert_ne!(out_id, 0, "start must mint a non-zero id");

        assert_eq!(ptr.update(out_id, 0.5), kResultOk);
        assert_eq!(ptr.finish(out_id), kResultOk);
        out_id
    };

    let mut got = drain(&handler).into_iter();
    match got.next() {
        Some(PluginNotification::Progress(ProgressEvent::Started {
            id,
            progress_type,
            description,
        })) => {
            assert_eq!(id, out_id);
            assert_eq!(progress_type, 1);
            assert_eq!(description.as_str(), "Test");
        }
        other => panic!("expected Started, got {other:?}"),
    }
    match got.next() {
        Some(PluginNotification::Progress(ProgressEvent::Updated { id, progress })) => {
            assert_eq!(id, out_id);
            assert!((progress - 0.5).abs() < 1e-3);
        }
        other => panic!("expected Updated, got {other:?}"),
    }
    assert!(matches!(
        got.next(),
        Some(PluginNotification::Progress(ProgressEvent::Finished { id })) if id == out_id
    ));
}

/// A description longer than the inline capacity is cut at the last whole
/// character that fits, never mid-character; unpaired surrogates become
/// U+FFFD rather than ending the decode.
///
/// Mutations: `break` → `continue` on a character that does not fit
/// (packing a later, smaller one into the gap) fails the length
/// assertion; skipping an unpaired surrogate instead of replacing it
/// fails the prefix assertion.
#[test]
fn a_long_progress_description_is_truncated_at_a_char_boundary() {
    // An unpaired high surrogate, then 'é' (2 bytes) and '€' (3 bytes)
    // repeated well past the capacity.
    let mut utf16: Vec<u16> = vec![0xD800, u16::from(b'a')];
    for _ in 0..100 {
        utf16.push(0x00E9);
        utf16.push(0x20AC);
    }
    utf16.push(0);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let d = unsafe { ProgressDescription::from_utf16_nul(utf16.as_ptr()) };
    assert!(d.as_str().starts_with("\u{FFFD}a\u{e9}\u{20ac}"));
    assert!(d.len() <= PROGRESS_DESCRIPTION_CAPACITY);
    // Byte budget 128: "\u{FFFD}a" is 4, then 5-byte pairs. 4 + 24*5 = 124,
    // and the next 'é' (2) fits at 126 while '€' (3) would reach 129.
    assert_eq!(d.len(), 126, "filled to the last whole character");
    assert!(d.as_str().ends_with('\u{e9}'));
}

/// A null description is legal per the interface (`optionalDescription`);
/// the handler must read it as empty rather than dereferencing it.
#[test]
fn progress_start_accepts_a_null_description() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IProgress>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        let mut out_id: ID = 0;
        assert_eq!(ptr.start(0, std::ptr::null(), &mut out_id), kResultOk);
    }

    match handler.pop_notification() {
        Some(PluginNotification::Progress(ProgressEvent::Started { description, .. })) => {
            assert_eq!(description.as_str(), "")
        }
        other => panic!("expected Started, got {other:?}"),
    }
}

#[test]
fn unit_selection_and_program_list_changes_forward_their_ids() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IUnitHandler>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        assert_eq!(ptr.notifyUnitSelection(5), kResultOk);
        assert_eq!(ptr.notifyProgramListChange(10, 3), kResultOk);
    }

    let got = drain(&handler);
    assert!(matches!(
        got[0],
        PluginNotification::Unit(UnitEvent::UnitSelected(5))
    ));
    match got[1] {
        PluginNotification::Unit(UnitEvent::ProgramListChanged {
            list_id,
            program_index,
        }) => {
            assert_eq!(list_id, 10);
            assert_eq!(program_index, 3);
        }
        ref other => panic!("expected ProgramListChanged, got {other:?}"),
    }
}

/// The three kinds share one queue, so their relative order survives: a
/// unit change the plugin made *before* an edit is read before it.
///
/// Mutation: giving progress its own queue (drained after edits, as the
/// three separate channels were) puts `Progress` last and fails this.
#[test]
fn notifications_keep_their_order_across_kinds() {
    let handler = ComponentHandler::new();
    let edit = handler.to_com_ptr::<IComponentHandler>().unwrap();
    let progress = handler.to_com_ptr::<IProgress>().unwrap();
    let unit = handler.to_com_ptr::<IUnitHandler>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        unit.notifyUnitSelection(1);
        progress.update(9, 0.5);
        edit.performEdit(3, 0.1);
    }

    let got = drain(&handler);
    assert!(matches!(got[0], PluginNotification::Unit(_)));
    assert!(matches!(got[1], PluginNotification::Progress(_)));
    assert!(matches!(got[2], PluginNotification::Edit(_)));
}

/// The handler is shared with the plugin's GUI thread as a bare COM
/// pointer, so concurrent `performEdit` calls must all land: an event
/// dropped under contention is an automation write the DAW never sees.
#[test]
#[cfg_attr(miri, ignore = "threaded stress test")]
fn concurrent_edits_all_reach_the_host() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
    let ptr_addr = ptr.as_ptr() as usize;

    let handles: Vec<_> = (0..4)
        .map(|i| {
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            std::thread::spawn(move || unsafe {
                let r = vst3::ComRef::<IComponentHandler>::from_raw_unchecked(ptr_addr as *mut _);
                for j in 0..10 {
                    r.performEdit(i as u32, j as f64 * 0.1);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(drain(&handler).len(), 40);
    assert_eq!(handler.dropped_notifications(), 0);

    drop(ptr);
    drop(handler);
}

/// **A plugin's `performEdit` value reaches the host inside `0..=1`.** The
/// event documents a normalized value and a host writes automation from it, so
/// a plugin's `f64` is clamped at the edge, as `addPoint`'s is.
///
/// Mutation (run, fails): pass `value_normalized` through unclamped -> 7.5 and -3.0 come out
/// as sent and the test fails.
#[test]
fn an_edit_value_is_clamped_to_the_unit_interval() {
    let handler = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
    // SAFETY: COM calls on a live object the test owns, with valid arguments.
    unsafe {
        ptr.performEdit(1, 7.5);
        ptr.performEdit(2, -3.0);
    }
    let values: Vec<f64> = drain(&handler)
        .into_iter()
        .filter_map(|n| match n {
            PluginNotification::Edit(ParameterEditEvent::PerformEdit { value, .. }) => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(values, [1.0, 0.0]);
}

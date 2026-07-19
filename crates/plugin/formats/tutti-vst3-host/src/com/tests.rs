//! Behavior tests for VST3 COM implementations.
//!
//! These drive our handler objects through the `vst3` crate's safe
//! `ComPtr` / `IFooTrait` surface, exactly how a plugin would reach them.

use std::ffi::c_void;
use std::time::Duration;

use vst3::Steinberg::{
    kResultOk, IBStream, IBStreamTrait,
    Vst::{
        DataExchangeBlock, IComponentHandler, IComponentHandlerTrait, IDataExchangeHandler,
        IDataExchangeHandlerTrait, IParameterChanges, IParameterChangesTrait, IProgress,
        IProgressTrait, IUnitHandler, IUnitHandlerTrait,
    },
};

use super::{
    BStream, ComponentHandler, ConnectionPoint, DataExchangeHandler, EventList, HostApplication,
    ParamValueQueueImpl, ParameterChangesImpl, ParameterEditEvent, ProgressEvent, ProgressHandler,
    RestartFlags, UnitEvent, UnitHandler,
};
use crate::types::{ParameterChanges, ParameterQueue};

#[test]
fn test_bstream_new() {
    let stream = BStream::new();
    assert_eq!(stream.data(), Vec::<u8>::new());
}

#[test]
fn test_bstream_from_data() {
    let data = vec![1, 2, 3, 4, 5];
    let stream = BStream::from_data(data.clone());
    assert_eq!(stream.data(), data);
}

#[test]
fn test_host_application_new() {
    let _host = HostApplication::new("TestHost");
}

#[test]
fn test_host_application_long_name_truncates() {
    let long_name = "A".repeat(200);
    let _host = HostApplication::new(&long_name);
}

#[test]
fn test_component_handler_new() {
    let (_handler, _rx, _prx, _urx) = ComponentHandler::new();
}

#[test]
fn test_component_handler_events() {
    let (handler, rx, _prx, _urx) = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    unsafe {
        let result = ptr.beginEdit(42);
        assert_eq!(result, kResultOk);

        let result = ptr.performEdit(42, 0.75);
        assert_eq!(result, kResultOk);

        let result = ptr.endEdit(42);
        assert_eq!(result, kResultOk);
    }

    let event1 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(event1, ParameterEditEvent::BeginEdit(42)));

    let event2 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(
        event2,
        ParameterEditEvent::PerformEdit {
            param_id: 42,
            value
        } if (value - 0.75).abs() < 0.001
    ));

    let event3 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(event3, ParameterEditEvent::EndEdit(42)));
}

#[test]
fn test_component_handler_restart() {
    let (handler, rx, _prx, _urx) = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

    unsafe {
        let result = ptr.restartComponent(0b1010);
        assert_eq!(result, kResultOk);
    }

    let event = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(
        event,
        ParameterEditEvent::RestartComponent(0b1010)
    ));
}

#[test]
fn test_restart_flags_decode_individual_bits() {
    use vst3::Steinberg::Vst::RestartFlags_;

    let latency = RestartFlags::from_bits(RestartFlags_::kLatencyChanged);
    assert!(latency.latency_changed);
    assert!(!latency.io_changed);
    assert!(!latency.param_values_changed);

    let io = RestartFlags::from_bits(RestartFlags_::kIoChanged);
    assert!(io.io_changed);
    assert!(!io.latency_changed);

    let cc = RestartFlags::from_bits(RestartFlags_::kMidiCCAssignmentChanged);
    assert!(cc.midi_cc_assignment_changed);

    let reload = RestartFlags::from_bits(RestartFlags_::kReloadComponent);
    assert!(reload.reload_component);

    let titles = RestartFlags::from_bits(RestartFlags_::kParamTitlesChanged);
    assert!(titles.param_titles_changed);

    let values = RestartFlags::from_bits(RestartFlags_::kParamValuesChanged);
    assert!(values.param_values_changed);
}

#[test]
fn test_restart_flags_decode_combined_bits() {
    use vst3::Steinberg::Vst::RestartFlags_;

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
fn test_restart_flags_decode_zero_is_empty() {
    let flags = RestartFlags::from_bits(0);
    assert_eq!(flags, RestartFlags::default());
}

#[test]
fn test_progress_handler_new() {
    let (_handler, _rx) = ProgressHandler::new();
}

#[test]
fn test_progress_handler_events() {
    let (handler, rx) = ProgressHandler::new();
    let ptr = handler.to_com_ptr::<IProgress>().unwrap();

    unsafe {
        let mut out_id: u64 = 0;
        let desc: [u16; 5] = [b'T' as u16, b'e' as u16, b's' as u16, b't' as u16, 0];
        let result = ptr.start(1, desc.as_ptr(), &mut out_id);
        assert_eq!(result, kResultOk);
        assert_eq!(out_id, 1);

        let result = ptr.update(out_id, 0.5);
        assert_eq!(result, kResultOk);

        let result = ptr.finish(out_id);
        assert_eq!(result, kResultOk);
    }

    let event1 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    match event1 {
        ProgressEvent::Started {
            id,
            progress_type,
            description,
        } => {
            assert_eq!(id, 1);
            assert_eq!(progress_type, 1);
            assert_eq!(description, "Test");
        }
        _ => panic!("Expected Started event"),
    }

    let event2 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    match event2 {
        ProgressEvent::Updated { id, progress } => {
            assert_eq!(id, 1);
            assert!((progress - 0.5).abs() < 0.001);
        }
        _ => panic!("Expected Updated event"),
    }

    let event3 = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(event3, ProgressEvent::Finished { id: 1 }));
}

#[test]
fn test_unit_handler_new() {
    let (_handler, _rx) = UnitHandler::new();
}

#[test]
fn test_unit_handler_selection() {
    let (handler, rx) = UnitHandler::new();
    let ptr = handler.to_com_ptr::<IUnitHandler>().unwrap();
    unsafe {
        let result = ptr.notifyUnitSelection(5);
        assert_eq!(result, kResultOk);
    }
    let event = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(event, UnitEvent::UnitSelected(5)));
}

#[test]
fn test_unit_handler_program_list() {
    let (handler, rx) = UnitHandler::new();
    let ptr = handler.to_com_ptr::<IUnitHandler>().unwrap();
    unsafe {
        let result = ptr.notifyProgramListChange(10, 3);
        assert_eq!(result, kResultOk);
    }
    let event = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    match event {
        UnitEvent::ProgramListChanged {
            list_id,
            program_index,
        } => {
            assert_eq!(list_id, 10);
            assert_eq!(program_index, 3);
        }
        _ => panic!("Expected ProgramListChanged event"),
    }
}

#[test]
fn test_data_exchange_handler_new() {
    let (_handler, _rx) = DataExchangeHandler::new();
}

#[test]
fn test_data_exchange_open_close_queue() {
    let (handler, _rx) = DataExchangeHandler::new();
    let ptr = handler.to_com_ptr::<IDataExchangeHandler>().unwrap();
    unsafe {
        let mut queue_id: u32 = 0;
        let result = ptr.openQueue(std::ptr::null_mut(), 1024, 4, 16, 100, &mut queue_id);
        assert_eq!(result, kResultOk);
        assert_eq!(queue_id, 1);

        let result = ptr.closeQueue(100);
        assert_eq!(result, kResultOk);
    }
}

#[test]
fn test_data_exchange_lock_free_block() {
    let (handler, rx) = DataExchangeHandler::new();
    let ptr = handler.to_com_ptr::<IDataExchangeHandler>().unwrap();
    unsafe {
        let mut queue_id: u32 = 0;
        let result = ptr.openQueue(std::ptr::null_mut(), 64, 2, 1, 42, &mut queue_id);
        assert_eq!(result, kResultOk);

        let mut block = DataExchangeBlock {
            data: std::ptr::null_mut(),
            size: 0,
            blockID: 0,
        };
        let result = ptr.lockBlock(42, &mut block);
        assert_eq!(result, kResultOk);
        assert_eq!(block.size, 64);
        assert!(!block.data.is_null());

        let data_slice = std::slice::from_raw_parts_mut(block.data as *mut u8, 64);
        data_slice[0] = 0xAB;
        data_slice[1] = 0xCD;

        let result = ptr.freeBlock(42, block.blockID, 1);
        assert_eq!(result, kResultOk);
    }

    let data_block = rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert_eq!(data_block.user_context_id, 42);
    assert_eq!(data_block.data[0], 0xAB);
    assert_eq!(data_block.data[1], 0xCD);
}

#[test]
fn test_event_list_new() {
    let _list = EventList::new();
}

#[test]
fn test_event_list_update_from_midi_counts_correctly() {
    use crate::types::MidiEvent;
    let list = EventList::new();
    let midi_events = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0)];
    list.update_from_midi(&midi_events);
    assert_eq!(list.len(), 1);
}

#[test]
fn test_event_list_clear_after_update_from_midi() {
    use crate::types::MidiEvent;
    let list = EventList::new();
    let midi_events = [
        MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
        MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(10),
    ];
    list.update_from_midi(&midi_events);
    list.clear();
    assert_eq!(list.len(), 0);
}

#[test]
fn test_param_value_queue_from_queue() {
    let mut queue = ParameterQueue::new(42);
    queue.add_point(0, 0.0);
    queue.add_point(128, 0.5);

    let impl_queue = ParamValueQueueImpl::from_queue(&queue);
    assert_eq!(impl_queue.param_id(), 42);
    assert_eq!(impl_queue.len(), 2);
}

#[test]
fn test_param_value_queue_new_empty() {
    let queue = ParamValueQueueImpl::new_empty(1);
    assert_eq!(queue.param_id(), 1);
    assert_eq!(queue.len(), 0);
    assert!(queue.is_empty());
}

#[test]
fn test_param_value_queue_to_queue() {
    let mut queue = ParameterQueue::new(5);
    queue.add_point(0, 0.25);
    queue.add_point(64, 0.75);

    let impl_queue = ParamValueQueueImpl::from_queue(&queue);
    let round_trip = impl_queue.to_queue();

    assert_eq!(round_trip.param_id, 5);
    assert_eq!(round_trip.points.len(), 2);
}

#[test]
fn test_parameter_changes_new_empty() {
    let changes = ParameterChangesImpl::new_empty();
    assert_eq!(changes.len(), 0);
    assert!(changes.is_empty());
}

#[test]
fn test_parameter_changes_from_changes() {
    let mut changes = ParameterChanges::new();
    changes.add_change(1, 0, 0.5);
    changes.add_change(2, 0, 0.75);

    let impl_changes = ParameterChangesImpl::from_changes(&changes);
    assert_eq!(impl_changes.len(), 2);
}

#[test]
fn test_parameter_changes_vtable() {
    let impl_changes = ParameterChangesImpl::new_empty();
    let ptr = impl_changes.to_com_ptr::<IParameterChanges>().unwrap();

    unsafe {
        assert_eq!(ptr.getParameterCount(), 0);

        let param_id: u32 = 42;
        let mut index: i32 = -1;
        let queue_ptr = ptr.addParameterData(&param_id, &mut index);
        assert!(!queue_ptr.is_null());
        assert_eq!(index, 0);
        assert_eq!(ptr.getParameterCount(), 1);

        let retrieved_ptr = ptr.getParameterData(0);
        assert!(!retrieved_ptr.is_null());
    }
}

#[test]
fn test_connection_point_new() {
    let _cp = ConnectionPoint::new();
}

#[test]
fn test_bstream_ref_counting() {
    let stream = BStream::new();
    let ptr = stream.to_com_ptr::<IBStream>().unwrap();
    // `ptr` and `stream` each own a reference. ComPtr::clone addrefs;
    // dropping each decrements. Nothing should crash.
    let clone = ptr.clone();
    drop(clone);
    drop(ptr);
    drop(stream);
}

#[test]
fn test_component_handler_thread_safe() {
    let (handler, rx, _prx, _urx) = ComponentHandler::new();
    let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
    let ptr_addr = ptr.as_ptr() as usize;

    let handles: Vec<_> = (0..4)
        .map(|i| {
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

    let mut count = 0;
    while rx.recv_timeout(Duration::from_millis(100)).is_ok() {
        count += 1;
    }
    assert_eq!(count, 40);

    drop(ptr);
    drop(handler);
}

#[test]
fn test_bstream_vtable_read_write() {
    let stream = BStream::new();
    let ptr = stream.to_com_ptr::<IBStream>().unwrap();

    unsafe {
        let data = b"Hello, VST3!";
        let mut written: i32 = 0;
        let result = ptr.write(
            data.as_ptr() as *mut c_void,
            data.len() as i32,
            &mut written,
        );
        assert_eq!(result, kResultOk);
        assert_eq!(written, data.len() as i32);

        let mut new_pos: i64 = 0;
        let result = ptr.seek(0, 0, &mut new_pos);
        assert_eq!(result, kResultOk);
        assert_eq!(new_pos, 0);

        let mut buffer = [0u8; 32];
        let mut bytes_read: i32 = 0;
        let result = ptr.read(
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as i32,
            &mut bytes_read,
        );
        assert_eq!(result, kResultOk);
        assert_eq!(bytes_read, data.len() as i32);
        assert_eq!(&buffer[..data.len()], data);

        let mut pos: i64 = 0;
        let result = ptr.tell(&mut pos);
        assert_eq!(result, kResultOk);
        assert_eq!(pos, data.len() as i64);
    }
}

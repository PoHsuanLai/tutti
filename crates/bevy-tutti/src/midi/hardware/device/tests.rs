use bevy_ecs::message::Messages;
use tutti_midi_hardware::test_support::{FakeBackend, endpoint};
use tutti_midi_hardware::{DeviceEvent, Devices, Direction};
use tutti_session::SessionEvent;

use super::*;

/// A real `Added` event, from a fake OS.
fn added() -> DeviceEvent {
    let (backend, fake) = FakeBackend::build_with(Vec::new(), Vec::new(), 0);
    let devices = Devices::open_over(backend).unwrap();
    let events = devices.subscribe();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    events.try_iter().next().expect("an Added")
}

fn world() -> World {
    let mut world = World::new();
    world.init_resource::<Messages<MidiHotplug>>();
    world.init_resource::<MidiDeviceState>();
    world
}

fn run(world: &mut World, event: &SessionEvent) {
    translate(event, &mut world.commands());
    world.flush();
}

fn hotplugs(world: &World) -> Vec<MidiHotplug> {
    let messages = world.resource::<Messages<MidiHotplug>>();
    messages.iter_current_update_messages().cloned().collect()
}

// Mutation record, each seen to fail, then restored:
// - `translate` ignoring `MidiDevice`: `a_device_event_becomes_a_hotplug_message`
//   fails;
// - `translate` ignoring a lost port: `a_lost_port_clears_the_poll_timer` fails;
// - `translate` clearing the poll timer on any other event:
//   `other_events_write_nothing` fails.

#[test]
fn a_device_event_becomes_a_hotplug_message() {
    let mut world = world();
    let event = added();
    run(&mut world, &SessionEvent::MidiDevice(event.clone()));
    assert_eq!(hotplugs(&world), [MidiHotplug(event)]);
}

#[test]
fn a_lost_port_clears_the_poll_timer() {
    for lost in [true, false] {
        let mut world = world();
        let info = added().info().clone();
        world.resource_mut::<MidiDeviceState>().last_check = Some(std::time::Instant::now());
        let event = if lost {
            SessionEvent::MidiInputLost(info)
        } else {
            SessionEvent::MidiOutputLost(info)
        };
        run(&mut world, &event);
        assert_eq!(world.resource::<MidiDeviceState>().last_check, None);
        assert!(hotplugs(&world).is_empty(), "a lost port is not a hotplug");
    }
}

#[test]
fn other_events_write_nothing() {
    let mut world = world();
    world.resource_mut::<MidiDeviceState>().last_check = Some(std::time::Instant::now());
    run(&mut world, &SessionEvent::MidiInputDropped(3));
    assert!(hotplugs(&world).is_empty());
    assert!(world.resource::<MidiDeviceState>().last_check.is_some());
}

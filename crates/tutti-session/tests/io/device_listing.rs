//! The output-device listing runs off the update: an enumeration that blocks
//! holds nothing, and its answer reaches the host as an event in a later
//! update (the session half of `bevy-tutti`'s
//! `a_blocked_enumeration_does_not_hold_the_frame_and_lands_when_it_answers`,
//! moved here when the adapter was parked; the `AudioDeviceState` list it
//! also read stayed with the adapter).

use std::time::{Duration, Instant};

use tutti_cpal::{DeviceId, DeviceListing, ManualStreamDriver, OutputSpec};
use tutti_session::{DeviceEvent, Session, SessionEvent};
use tutti_types::{ChannelLayout, SampleRate};

/// **A blocked enumeration does not hold the update, and lands when it
/// answers.** While the enumerator is blocked, updates go by and report
/// nothing; once it answers, a later update reports the list
/// (`SessionEvent::OutputsListed`).
#[test]
fn a_blocked_enumeration_does_not_hold_the_update_and_lands_when_it_answers() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut s, _device) = Session::builder()
        .build_device_on(
            OutputSpec::new(
                SampleRate(48_000.0),
                ChannelLayout::STEREO,
                tutti_cpal::cpal::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device")
        .into_parts();
    let (release, held) = std::sync::mpsc::channel::<()>();
    s.list_outputs(
        DeviceListing::spawn_with(move || {
            held.recv().ok();
            Ok(["Card", "USB"].into_iter().map(DeviceId::new).collect())
        })
        .expect("the listing thread starts"),
    );
    for _ in 0..3 {
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::OutputsListed(_)))),
            "not answered yet: {events:?}"
        );
    }

    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let listed = s.update().into_iter().find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::OutputsListed(devices)) => Some(devices),
            _ => None,
        });
        if let Some(devices) = listed {
            let names: Vec<_> = devices.iter().map(|d| d.name().to_owned()).collect();
            assert_eq!(names, ["Card", "USB"]);
            break;
        }
        assert!(Instant::now() < deadline, "no list arrived");
        std::thread::yield_now();
    }
}

/// **A listing that fails reports the cpal error itself**, not its text: the
/// event carries the `tutti_cpal::Error` the enumeration returned.
#[test]
fn a_failed_listing_reports_the_typed_error() {
    let (driver, _stream) = ManualStreamDriver::new();
    let (mut s, _device) = Session::builder()
        .build_device_on(
            OutputSpec::new(
                SampleRate(48_000.0),
                ChannelLayout::STEREO,
                tutti_cpal::cpal::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device")
        .into_parts();
    s.list_outputs(
        DeviceListing::spawn_with(|| Err(tutti_cpal::Error::InvalidDevice("no card".into())))
            .expect("the listing thread starts"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let failed = s.update().into_iter().find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::OutputsListFailed(why)) => Some(why),
            _ => None,
        });
        if let Some(why) = failed {
            assert!(
                matches!(&why, tutti_cpal::Error::InvalidDevice(what) if what == "no card"),
                "{why:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "no failure arrived");
        std::thread::yield_now();
    }
}

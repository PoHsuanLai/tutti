//! Render notifications and sample-accurate parameter scheduling.
//!
//! The registration, the notification vocabulary and `schedule` are
//! `tutti-au-ffi`'s; they are re-exported here under the paths this crate has
//! always used.

#![cfg(target_os = "macos")]

pub use tutti_au_ffi::{
    ParamEvent, RenderNotification, RenderNotify, RenderPhase, RenderUnit, ScheduleAddress,
    schedule,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::find_component;
    use crate::controller::AuController;
    use crate::instance::AuLoaded;
    use crate::processor::AuProcessor;
    use tutti_plugin_types::{Loaded, MainThread, MaxBlock, Prepare, SampleRate};
    use crate::listener::EventAddress;
    use crate::parameters::ParamAddress;
    use crate::types::{AudioComponentDescription, K_AUDIO_UNIT_TYPE_EFFECT};

    /// `ScheduleAddress::GLOBAL` must name the same place the other two address
    /// types do. They are deliberately separate types, so nothing but a test
    /// keeps them agreeing — and an event scheduled on a different scope than
    /// the parameter lives on is silently dropped by the AU.
    ///
    /// Also pins `EventAddress::GLOBAL` against `ParamAddress::GLOBAL`: a
    /// listener registered on another scope than the writes go to would never
    /// fire, and AudioToolbox accepts any address without validating it.
    ///
    /// Mutation (run): change `ScheduleAddress::GLOBAL.element` to 1 -> fails.
    #[test]
    fn the_global_address_agrees_with_the_other_address_types() {
        assert_eq!(ScheduleAddress::GLOBAL.scope, ParamAddress::GLOBAL.scope);
        assert_eq!(
            ScheduleAddress::GLOBAL.element,
            ParamAddress::GLOBAL.element
        );
        assert_eq!(ScheduleAddress::GLOBAL.scope, EventAddress::GLOBAL.scope);
        assert_eq!(EventAddress::GLOBAL.element, ParamAddress::GLOBAL.element);
        assert_eq!(ScheduleAddress::default(), ScheduleAddress::GLOBAL);
        assert_eq!(EventAddress::default(), EventAddress::GLOBAL);
    }

    fn delay() -> (AuProcessor, AuController) {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = find_component(&desc).expect("AUDelay ships with macOS");
        let au = AuLoaded::new(comp, 48_000.0, 512).unwrap();
        let prepare = Prepare::new(SampleRate(48_000.0), MaxBlock::of(tutti_types::Samples(512)))
            .unwrap();
        au.activate(&MainThread::claim().unwrap(), prepare)
            .unwrap_or_else(|e| panic!("AUDelay activates: {}", e.reason))
    }

    fn render_once(au: &mut AuProcessor) {
        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        au.render_f32(&ins, &mut outs, 512).unwrap();
    }

    /// The dispatch counter rises by exactly the pre/post pair per render while
    /// the notify is installed, a fresh handle starts at zero, and rendering
    /// after a drop stays safe.
    ///
    /// Mutation (run): skip the `fetch_add` in the callback -> the count stays 0.
    #[test]
    fn the_dispatch_counter_stops_when_the_handle_is_dropped() {
        let (mut au, controller) = delay();
        let notify = controller
            .add_render_notify(|_| {})
            .expect("AUDelay accepts a notify");
        assert_eq!(notify.deliveries(), 0, "nothing rendered yet");
        render_once(&mut au);
        assert_eq!(
            notify.deliveries(),
            2,
            "one render dispatches the pre/post pair"
        );

        drop(notify);
        for _ in 0..4 {
            render_once(&mut au);
        }

        let fresh = controller.add_render_notify(|_| {}).unwrap();
        assert_eq!(fresh.deliveries(), 0, "a new handle starts from zero");
        render_once(&mut au);
        assert_eq!(
            fresh.deliveries(),
            2,
            "only the fresh registration delivers"
        );
    }

    /// An empty schedule is `Ok` without an AudioToolbox call.
    ///
    /// No mutation fails this: AudioToolbox also answers `noErr` for a zero
    /// count, so the test pins the observable contract, not the early return.
    #[test]
    fn scheduling_nothing_is_a_no_op() {
        let (_p, au) = delay();
        assert!(
            au.schedule_parameters(0, ScheduleAddress::GLOBAL, &[])
                .is_ok()
        );
    }
}

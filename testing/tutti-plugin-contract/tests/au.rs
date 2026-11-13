//! The contract against AU, from the shared plugin's Audio Unit shell
//! (registered in-process; there is no served run, an in-process registration
//! being visible to this process only).
#![cfg(target_os = "macos")]

tutti_plugin_contract::contract!(
    tutti_plugin_contract::Au,
    gaps = 3,
    hostings = [InProcess, OutOfProcess]
);

mod ramped {
    use tutti_au_host::render_notify::{ParamEvent, ScheduleAddress};
    use tutti_plugin_contract::spec::{Mode, Param};
    use tutti_plugin_contract::{
        Active, Au, BLOCK, Format, Kind, RATE, Render, prepare, set_mode, transcript,
    };
    use tutti_plugin_types::SampleFormat;

    fn level_unit() -> Active<<Au as Format>::L> {
        let mut loaded = Au::load();
        set_mode::<Au, _>(&mut loaded, Mode::Level);
        Active::activate(loaded, prepare(RATE, BLOCK, SampleFormat::Float32))
            .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
    }

    fn ramp(a: &Active<<Au as Format>::L>, offset: i32, duration: usize, from: f32, to: f32) {
        a.controller()
            .schedule_parameters(
                Param::Level.id().get(),
                ScheduleAddress::GLOBAL,
                &[ParamEvent::Ramped {
                    start_buffer_offset: offset,
                    duration_frames: duration,
                    start_value: from,
                    end_value: to,
                }],
            )
            .expect("the unit takes a ramped event");
    }

    /// A ramped event reaches the unit as sent, and the level moves linearly from
    /// the start value over the duration and holds the end value: `AUComponent.h`,
    /// `AudioUnitParameterEvent`, `kParameterEvent_Ramped`.
    /// Spec: au.select.AudioUnitScheduleParameters, au.param.event.Ramped
    #[test]
    fn a_ramped_event_moves_the_level_linearly_over_its_duration() {
        let mut a = level_unit();
        ramp(&a, 8, 16, 0.25, 0.75);
        let out = a.silent(BLOCK);
        for (i, &v) in out[0].iter().enumerate() {
            let want = match i {
                0..8 => 0.5,
                8..24 => 0.25 + 0.5 * (i - 8) as f32 / 16.0,
                _ => 0.75,
            };
            assert_eq!(v, want, "frame {i}");
        }
        let t = transcript::<Au, _>(a.controller_mut());
        let ramps: Vec<_> = t.of(Kind::Ramp).collect();
        assert_eq!(ramps.len(), 1, "one ramp arrived");
        let r = ramps[0].ramp.expect("a ramp carries its span");
        assert_eq!(
            (
                ramps[0].offset,
                ramps[0].id,
                ramps[0].value,
                r.duration,
                r.end
            ),
            (8, Param::Level.id().get(), 0.25, 16, 0.75)
        );
        assert_eq!(t.of(Kind::Param).count(), 0, "a ramp is not a point");
        assert_eq!(t.level, 0.75, "the level the ramp ends on");
    }

    /// A ramp that outlasts its block goes on in the next one, from where the
    /// first left it.
    /// Spec: au.param.event.Ramped
    #[test]
    fn a_ramp_continues_into_the_next_block() {
        let mut a = level_unit();
        ramp(&a, 56, 16, 0.0, 1.0);
        let first = a.silent(BLOCK);
        for (i, &v) in first[0].iter().enumerate() {
            let want = if i < 56 { 0.5 } else { (i - 56) as f32 / 16.0 };
            assert_eq!(v, want, "first block, frame {i}");
        }
        let second = a.silent(BLOCK);
        for (i, &v) in second[0].iter().enumerate() {
            let want = if i < 8 { (8 + i) as f32 / 16.0 } else { 1.0 };
            assert_eq!(v, want, "second block, frame {i}");
        }
    }
}

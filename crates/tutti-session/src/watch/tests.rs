use super::*;

fn t0() -> Instant {
    Instant::now()
}

fn at(base: Instant, ms: u64) -> Instant {
    base + Duration::from_millis(ms)
}

fn id(name: &str) -> DeviceId {
    DeviceId::new(name)
}

fn view(outputs: &[&str], default: Option<&str>) -> Snapshot {
    Snapshot::from_names(outputs.iter().copied(), [], default, None)
}

fn on() -> DevicePolicy {
    DevicePolicy {
        follow_default: true,
        reconnect: true,
        retry_startup: true,
        ..DevicePolicy::default()
    }
}

fn wanted(r: Option<DeviceRestart>) -> Option<DeviceSelector> {
    r.map(|r| r.device)
}

/// Nothing moves the stream with the policy at its defaults, whatever
/// happens to the devices.
///
/// Mutation (run): `Default` made `follow_default: true` and the `Id` arm
/// `reconnect: true` → this fails.
#[test]
fn every_automatic_behaviour_is_off_by_default() {
    let now = t0();
    for want in [DeviceSelector::Default, DeviceSelector::named("A")] {
        let mut f = Follow::running(DevicePolicy::default(), want, Some(id("A")));
        f.step(FollowEvent::Lost, &view(&["B"], Some("B")), now);
        assert_eq!(
            wanted(f.step(FollowEvent::Look, &view(&["B"], Some("B")), at(now, 60_000))),
            None
        );
        assert_eq!(
            wanted(f.step(
                FollowEvent::Look,
                &view(&["A", "B"], Some("A")),
                at(now, 120_000)
            )),
            None
        );
    }
}

/// An explicit choice is never replaced by another device: when it goes
/// the policy waits, and it reopens only that device, when it is back.
///
/// Mutation (run): the `Id` arm returning `DeviceSelector::Default` → fails.
#[test]
fn an_explicit_choice_is_never_overridden() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::named("A"), Some(id("A")));
    // It vanishes and another card is the default: nothing.
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&["B"], Some("B")), now)),
        None
    );
    assert_eq!(
        wanted(f.step(FollowEvent::Lost, &view(&["B"], Some("B")), at(now, 5_000))),
        None
    );
    // The default moves again: still nothing.
    assert_eq!(
        wanted(f.step(
            FollowEvent::Look,
            &view(&["B", "C"], Some("C")),
            at(now, 9_000)
        )),
        None
    );
    // It returns: that device, and only that.
    assert_eq!(
        wanted(f.step(
            FollowEvent::Look,
            &view(&["B", "A"], Some("B")),
            at(now, 10_000)
        )),
        Some(DeviceSelector::named("A"))
    );
    // Without `reconnect` it would stay silent for good.
    let mut f = Follow::running(
        DevicePolicy {
            reconnect: false,
            ..on()
        },
        DeviceSelector::named("A"),
        Some(id("A")),
    );
    f.step(FollowEvent::Lost, &view(&["B"], Some("B")), now);
    assert_eq!(
        wanted(f.step(
            FollowEvent::Look,
            &view(&["A", "B"], Some("A")),
            at(now, 9_000)
        )),
        None
    );
}

/// A selector of `Default` follows the default, and the device it runs on
/// vanishing.
#[test]
fn the_default_is_followed_when_it_moves_or_its_device_goes() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::Default, Some(id("A")));
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&["A", "B"], Some("A")), now)),
        None
    );
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&["A", "B"], Some("B")), at(now, 1))),
        Some(DeviceSelector::Default),
        "the default moved"
    );
    f.step(
        FollowEvent::Opened(Some(id("B"))),
        &view(&["A", "B"], Some("B")),
        at(now, 2),
    );
    // B is unplugged, and the default falls back to A.
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&["A"], Some("A")), at(now, 60_000))),
        Some(DeviceSelector::Default),
        "its device vanished"
    );
    // With no default at all there is nothing to open: it waits.
    let mut f = Follow::running(on(), DeviceSelector::Default, Some(id("A")));
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&[], None), now)),
        None
    );
}

/// A device that flaps is not retried on every change: the wait doubles to
/// `max` and strikes clear only after `settle`.
///
/// Mutation (run): the `not_before` gate removed → a request on every
/// change, and this fails at the first flap.
#[test]
fn a_flapping_device_is_held_by_the_backoff() {
    let policy = DevicePolicy {
        backoff: Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(4),
            settle: Duration::from_secs(10),
        },
        ..on()
    };
    let now = t0();
    let mut f = Follow::running(policy, DeviceSelector::named("A"), Some(id("A")));
    let gone = view(&["B"], Some("B"));
    let back = view(&["A", "B"], Some("B"));
    let mut asked = Vec::new();
    // Flap every 100 ms for 20 s.
    for k in 0..200u64 {
        let ms = k * 100;
        let (ev, v) = if k % 2 == 0 {
            (FollowEvent::Lost, &gone)
        } else {
            (FollowEvent::Look, &back)
        };
        if f.step(ev, v, at(now, ms)).is_some() {
            asked.push(ms);
            // The restart "succeeds" and the card drops out again at once.
            f.step(FollowEvent::Opened(Some(id("A"))), v, at(now, ms));
        }
    }
    assert!(asked.len() < 12, "{asked:?}: far fewer than the 100 flaps");
    for pair in asked.windows(2) {
        assert!(
            pair[1] - pair[0] >= 500,
            "{asked:?}: never closer than base"
        );
    }
    let waits: Vec<_> = asked.windows(2).map(|p| p[1] - p[0]).collect();
    assert!(
        waits.iter().any(|&w| w >= 2_000),
        "{waits:?}: the wait grew"
    );
    assert!(
        waits.iter().all(|&w| w <= 4_000 + 100),
        "{waits:?}: capped at max"
    );
}

/// After `settle` of quiet the strikes clear: a loss after a long calm is
/// answered at once, and the wait that follows is the base again, not the
/// one the old strikes had grown.
///
/// Mutation (run): the reset of `strikes` removed → the second request after
/// the calm is held for the grown wait, and this fails.
#[test]
fn the_backoff_clears_after_the_device_settles() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::named("A"), Some(id("A")));
    let gone = view(&["B"], Some("B"));
    let back = view(&["A", "B"], Some("B"));
    // Four attempts in a row: the waits grow to 4 s.
    let mut ms = 0;
    for _ in 0..4 {
        f.step(FollowEvent::Lost, &gone, at(now, ms));
        while f.step(FollowEvent::Look, &back, at(now, ms)).is_none() {
            ms += 100;
        }
    }
    // A long calm, then it flaps once more.
    let calm = ms + 120_000;
    f.step(FollowEvent::Opened(Some(id("A"))), &back, at(now, calm));
    f.step(FollowEvent::Lost, &gone, at(now, calm));
    assert!(
        f.step(FollowEvent::Look, &back, at(now, calm)).is_some(),
        "at once"
    );
    f.step(FollowEvent::Opened(Some(id("A"))), &back, at(now, calm));
    f.step(FollowEvent::Lost, &gone, at(now, calm + 600));
    assert!(
        f.step(FollowEvent::Look, &back, at(now, calm + 600))
            .is_some(),
        "the base wait, not the grown one"
    );
}

/// A refused restart that left the stream playing is not a loss; one that
/// stopped it is, and the policy asks again, no sooner than its backoff.
#[test]
fn a_failed_restart_is_retried_after_the_backoff_only() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::Default, Some(id("A")));
    let v = view(&["A", "B"], Some("B"));
    assert!(f.step(FollowEvent::Look, &v, now).is_some());
    // Stopped: the old device is broken until something opens.
    assert_eq!(
        wanted(f.step(
            FollowEvent::RestartFailed { running: false },
            &v,
            at(now, 10)
        )),
        None,
        "waiting"
    );
    assert!(f.step(FollowEvent::Look, &v, at(now, 400)).is_none());
    assert!(
        f.step(FollowEvent::Look, &v, at(now, 500)).is_some(),
        "base elapsed"
    );
    // Still running after the refusal: nothing broke, and the default
    // being B is what a retry is for (it is not on B).
    let mut g = Follow::running(on(), DeviceSelector::Default, Some(id("B")));
    assert!(
        g.step(
            FollowEvent::RestartFailed { running: true },
            &view(&["B"], Some("B")),
            now
        )
        .is_none()
    );
}

/// A start that failed is retried when its device is listed, if asked to.
#[test]
fn a_failed_start_is_retried_when_a_device_appears() {
    let now = t0();
    let mut f = Follow::not_started(on(), DeviceSelector::Default);
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&[], None), now)),
        None
    );
    assert_eq!(
        wanted(f.step(FollowEvent::Look, &view(&["A"], Some("A")), at(now, 1))),
        Some(DeviceSelector::Default)
    );
    let mut off = Follow::not_started(DevicePolicy::default(), DeviceSelector::Default);
    assert_eq!(
        wanted(off.step(FollowEvent::Look, &view(&["A"], Some("A")), now)),
        None
    );
    let mut named = Follow::not_started(on(), DeviceSelector::named("X"));
    assert_eq!(
        wanted(named.step(FollowEvent::Look, &view(&["A"], Some("A")), now)),
        None,
        "X is not listed"
    );
    assert!(
        named
            .step(FollowEvent::Look, &view(&["A", "X"], Some("A")), at(now, 1))
            .is_some()
    );
}

/// Property: over any sequence of events and views, two restarts are never
/// closer than the base wait, and none is asked while the policy is off.
#[test]
fn restarts_are_never_closer_than_the_base_wait() {
    let mut state = 0x1234_5678_u32;
    let mut rnd = move |n: u32| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 16) % n
    };
    let names = ["A", "B", "C"];
    for want in [DeviceSelector::Default, DeviceSelector::named("A")] {
        let now = t0();
        let mut f = Follow::running(on(), want, Some(id("A")));
        let mut last: Option<u64> = None;
        let mut ms = 0u64;
        for _ in 0..2_000 {
            ms += u64::from(rnd(400));
            let outs: Vec<&str> = names.iter().copied().filter(|_| rnd(2) == 0).collect();
            let default = outs.first().copied();
            let v = view(&outs, default);
            let ev = match rnd(5) {
                0 => FollowEvent::Lost,
                1 => FollowEvent::Opened(Some(id(names[rnd(3) as usize]))),
                2 => FollowEvent::RestartFailed {
                    running: rnd(2) == 0,
                },
                _ => FollowEvent::Look,
            };
            if f.step(ev, &v, at(now, ms)).is_some() {
                if let Some(prev) = last {
                    assert!(ms - prev >= 500, "restarts at {prev} and {ms}");
                }
                last = Some(ms);
            }
        }
        assert!(last.is_some(), "the walk did ask for something");
    }
}

/// A stream on a device whose identity is unknown is not on a different
/// one: a healthy stream is not restarted.
///
/// Mutation (run): `moved` made `running_on != Some(default)` → this fails.
#[test]
fn an_unknown_device_is_not_a_different_one() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::Default, None);
    for k in 0..10 {
        let v = view(&["A", "B"], Some("A"));
        assert!(f.step(FollowEvent::Look, &v, at(now, k * 60_000)).is_none());
    }
}

/// Absence from the list is not a lost stream (ALSA's probe hides a device
/// this process holds), whatever the ordinals do; only a fault is.
///
/// Mutation (run): a latch of `broken` on absence in `decide` → this fails.
#[test]
fn absence_from_the_list_does_not_end_a_stream() {
    let now = t0();
    let usb1 = DeviceId::nth("USB", 1);
    let mut f = Follow::running(on(), DeviceSelector::Id(usb1.clone()), Some(usb1));
    let gone = view(&["Built-in"], Some("Built-in"));
    let renumbered = Snapshot::from_names(["USB"], [], Some("USB"), None);
    let back = Snapshot::from_names(["USB", "USB"], [], Some("USB"), None);
    for (k, v) in [&gone, &renumbered, &back].into_iter().enumerate() {
        assert!(
            f.step(FollowEvent::Look, v, at(now, k as u64 * 60_000))
                .is_none()
        );
    }
    f.step(FollowEvent::Lost, &back, at(now, 200_000));
    assert!(
        f.step(FollowEvent::Look, &back, at(now, 300_000)).is_some(),
        "a fault is"
    );
}

/// Held: nothing is asked for, and it resumes when released.
///
/// Mutation (run): the `held` check removed → this fails.
#[test]
fn a_held_policy_asks_for_nothing() {
    let now = t0();
    let mut f = Follow::running(on(), DeviceSelector::Default, Some(id("A")));
    f.hold(true);
    let v = view(&["A", "B"], Some("B"));
    assert!(f.step(FollowEvent::Look, &v, now).is_none());
    f.hold(false);
    assert!(f.step(FollowEvent::Look, &v, at(now, 1)).is_some());
}

// Mutations (run, each restored): the `not_before` gate removed fails the
// flapping, failed-restart and spacing tests; `Lost` ignored fails the
// flapping and explicit-choice tests; `RestartFailed` always marking broken
// fails the failed-restart test; the strike reset after `settle` removed
// fails the settle test; `follow_default`/`reconnect` defaulting on fails the
// off-by-default test.

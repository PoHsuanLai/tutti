use super::*;

/// Counts its drops, to show a value is dropped once and only where it should be.
struct Counted(std::rc::Rc<std::cell::Cell<u32>>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

// `Counted` holds an `Rc`, which is not `Send`; these tests stay on one thread
// and reach the ends' methods, which ask nothing of `T`.
fn pair<T>() -> (Hand<T>, Rt<T>) {
    let shared = Arc::new(Shared {
        state: AtomicU32::new(EMPTY),
        slot: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (
        Hand {
            shared: Arc::clone(&shared),
        },
        Rt { shared, held: None },
    )
}

/// A lend reaches the audio end at its poll, and a recall brings it back
/// through `take` after the next poll, with `last` run on it once.
#[test]
fn a_lent_value_comes_back_after_a_recall() {
    let (mut hand, mut rt) = handoff::<u32>();
    hand.lend(7).unwrap();
    assert_eq!(rt.poll(|_| unreachable!()).copied(), Some(7), "taken");
    assert_eq!(hand.take(), None, "nothing returned yet");
    assert_eq!(hand.recall(), None, "held by the audio end");
    let mut stops = 0;
    assert_eq!(
        rt.poll(|v| {
            stops += 1;
            *v += 1;
        }),
        None,
        "returned"
    );
    assert_eq!(stops, 1);
    assert_eq!(hand.take(), Some(8), "back, after `last`");
    assert_eq!(hand.take(), None, "once");
}

/// A recall before the audio end polled withdraws the offer at once, and the
/// audio end then finds nothing.
#[test]
fn a_recall_withdraws_an_offer_not_yet_taken() {
    let (mut hand, mut rt) = handoff::<u32>();
    hand.lend(3).unwrap();
    assert_eq!(hand.recall(), Some(3));
    assert_eq!(rt.poll(|_| unreachable!()), None);
    assert_eq!(hand.take(), None);
}

/// A second lend while the first is still on offer is refused with its value;
/// so is one while a returned value waits to be taken.
#[test]
fn a_lend_into_a_full_slot_is_refused() {
    let (mut hand, mut rt) = handoff::<u32>();
    hand.lend(1).unwrap();
    assert_eq!(hand.lend(2), Err(2), "an offer is waiting");
    rt.poll(|_| {});
    assert_eq!(hand.recall(), None);
    rt.poll(|_| {});
    assert_eq!(hand.lend(3), Err(3), "a return is waiting");
    assert_eq!(hand.take(), Some(1));
    hand.lend(3).unwrap();
    assert_eq!(rt.poll(|_| {}).copied(), Some(3));
}

/// A lend while a recall is raised is refused: the audio end may still hold
/// the value the recall is for (Core's review of #494: lend A, the audio end
/// takes it, recall, lend B; clearing the bit there left A running with
/// nothing asking for it). That value then comes back at the next poll.
#[test]
fn a_lend_is_refused_while_a_recall_is_raised() {
    let (mut hand, mut rt) = handoff::<u32>();
    hand.lend(1).unwrap();
    assert_eq!(rt.poll(|_| unreachable!()).copied(), Some(1));
    assert_eq!(hand.recall(), None);
    assert_eq!(hand.lend(2), Err(2), "the recall is pending");
    assert_eq!(rt.poll(|_| {}), None, "returned");
    assert_eq!(hand.take(), Some(1));
    hand.lend(2).unwrap();
    assert_eq!(rt.poll(|_| unreachable!()).copied(), Some(2));
}

/// A recall with nothing lent is stale: the audio end clears it at its next
/// poll, and a lend after that is taken and kept, not sent straight back.
#[test]
fn the_audio_end_clears_a_stale_recall() {
    let (mut hand, mut rt) = handoff::<u32>();
    assert_eq!(hand.recall(), None);
    assert_eq!(hand.lend(5), Err(5), "refused until the audio end polls");
    assert_eq!(rt.poll(|_| unreachable!()), None);
    hand.lend(5).unwrap();
    assert_eq!(rt.poll(|_| unreachable!()).copied(), Some(5));
    assert_eq!(rt.poll(|_| unreachable!()).copied(), Some(5), "kept");
}

/// A value left in the slot drops with the last end, once; one the audio end
/// holds drops with that end.
#[test]
fn a_value_in_the_slot_drops_with_the_last_end() {
    let drops = std::rc::Rc::new(std::cell::Cell::new(0));
    let (mut hand, rt) = pair::<Counted>();
    assert!(hand.lend(Counted(drops.clone())).is_ok());
    drop(rt);
    assert_eq!(drops.get(), 0, "the hand still shares the slot");
    drop(hand);
    assert_eq!(drops.get(), 1);

    let (mut hand, mut rt) = pair::<Counted>();
    assert!(hand.lend(Counted(drops.clone())).is_ok());
    assert!(rt.poll(|_| {}).is_some());
    drop(hand);
    assert_eq!(drops.get(), 1, "the audio end holds it");
    drop(rt);
    assert_eq!(drops.get(), 2);
}

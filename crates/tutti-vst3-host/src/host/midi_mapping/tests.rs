use super::*;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_types::Samples;
use tutti_ump::{CCNumber, KeyEvent, MidiChannel, MidiGroup};

/// A packet from its words, zero-padded to four, at offset 0.
fn raw(words: &[u32]) -> UmpAt {
    let mut packet = [0; 4];
    packet[..words.len()].copy_from_slice(words);
    (Offset::ZERO, Ump(packet))
}

/// A block longer than any offset these tests place an event at.
const TEST_BLOCK: usize = 1 << 16;

/// The offset `k` frames into a test block.
fn at(k: u32) -> Offset {
    Offset::new(usize::try_from(k).unwrap(), Samples(TEST_BLOCK)).unwrap()
}

#[test]
fn empty_mapping_returns_none() {
    let m = MidiCcMapping::empty();
    assert!(m.is_empty());
    assert_eq!(m.lookup(0, 1), None);
    assert_eq!(m.lookup(0, CTRL_PITCH_BEND), None);
}

#[test]
fn lookup_resolves_mapped_slots() {
    let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
    // Channel 0, mod wheel (CC 1) -> param 100.
    table[1] = 100;
    // Channel 3, pitch bend -> param 200.
    table[3 * NUM_CONTROLLERS + CTRL_PITCH_BEND] = 200;
    let m = MidiCcMapping {
        table,
        any_mapped: true,
    };
    assert!(!m.is_empty());
    assert_eq!(m.lookup(0, 1), Some(100));
    assert_eq!(m.lookup(3, CTRL_PITCH_BEND), Some(200));
    // Unmapped slot on a channel that has other mappings.
    assert_eq!(m.lookup(0, 7), None);
    // Out-of-range channel / controller.
    assert_eq!(m.lookup(16, 1), None);
    assert_eq!(m.lookup(0, NUM_CONTROLLERS), None);
}

/// Run a `(Offset, Ump)` through `midi_to_mapped_controller`.
fn mapped(event: &UmpAt) -> Option<(u8, usize, f64)> {
    midi_to_mapped_controller(&event.1)
}

#[test]
fn cc_decodes_to_controller_and_normalized_value() {
    // CC 74 (brightness) = 64 on channel 2.
    let (ch, ctrl, value) = mapped(&(
        Offset::ZERO,
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::of(2),
            CCNumber::BRIGHTNESS,
            tutti_fixtures::midi::wide_value(64),
        ),
    ))
    .unwrap();
    assert_eq!(ch, 2);
    assert_eq!(ctrl, 74);
    assert!((value - 64.0 / 127.0).abs() < 0.01);
}

#[test]
fn channel_pressure_decodes_to_aftertouch() {
    let (ch, ctrl, value) = mapped(&(
        Offset::ZERO,
        midi::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            tutti_fixtures::midi::wide_value(127),
        ),
    ))
    .unwrap();
    assert_eq!(ch, 0);
    assert_eq!(ctrl, CTRL_AFTERTOUCH);
    assert!((value - 1.0).abs() < 0.01);
}

#[test]
fn pitch_bend_center_is_half() {
    let (ch, ctrl, value) = mapped(&(
        Offset::ZERO,
        midi::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(4),
            tutti_fixtures::midi::wide_bend(8192),
        ),
    ))
    .unwrap();
    assert_eq!(ch, 4);
    assert_eq!(ctrl, CTRL_PITCH_BEND);
    // Center bend maps to the middle of the unit range.
    assert!((value - 0.5).abs() < 0.01);
}

#[test]
fn note_on_is_not_a_mapped_controller() {
    assert_eq!(
        mapped(&(
            Offset::ZERO,
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
        )),
        None
    );
}

/// Builds a table with a single channel-0 mod-wheel (CC 1) → param mapping.
fn mod_wheel_mapping(param_id: ParamID) -> MidiCcMapping {
    let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
    table[1] = param_id; // channel 0, CC 1
    MidiCcMapping {
        table,
        any_mapped: true,
    }
}

#[test]
fn route_cc_events_splits_mapped_cc_into_params() {
    let mapping = mod_wheel_mapping(500);
    // Mod wheel (mapped) + a note-on (passes through) + an unmapped CC.
    let events = [
        (
            at(8),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(64),
            ),
        ),
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        (
            at(0),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                tutti_fixtures::midi::wide_value(100),
            ),
        ),
    ];

    let mut filtered = FilteredMidi::new();
    let mut params = ParameterChanges::reserved(1, 1);
    route_cc_events(&mapping, &events, &mut filtered, &mut params);

    // Mod wheel went to params.
    let queue = params
        .queue(500u32.into())
        .expect("mod wheel mapped to param 500");
    assert_eq!(queue.points.len(), 1);
    assert_eq!(queue.points[0].sample_offset, 8);
    // Value decoded at MIDI-2 width then normalized; ~64/127, not bit-exact
    // (the source CC was a MIDI-1→MIDI-2 promotion).
    assert!((queue.points[0].value.get() - 64.0 / 127.0).abs() < 0.01);

    // Note-on and the unmapped CC 74 stayed as MIDI events.
    assert_eq!(filtered.len(), 2);
    assert!(
        filtered
            .iter()
            .any(|e| matches!(e.1.key_event(), Some(KeyEvent::On(_))))
    );
}

#[test]
fn route_cc_events_preserves_host_params() {
    let mapping = mod_wheel_mapping(500);
    let events = [(
        at(0),
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            tutti_fixtures::midi::wide_value(127),
        ),
    )];

    let mut filtered = FilteredMidi::new();
    let mut params = ParameterChanges::reserved(2, 1);
    // Pre-seed with a host automation point (as the audio path does).
    params.add_change(42u32.into(), 0, 0.25);
    route_cc_events(&mapping, &events, &mut filtered, &mut params);

    assert!(params.queue(42u32.into()).is_some(), "host param survived");
    assert!(params.queue(500u32.into()).is_some(), "mapped CC added");
    assert!(filtered.is_empty(), "the only event was a mapped CC");
}

/// Regression: seeding host automation first, then appending CC-derived
/// points in MIDI-arrival order, can leave a queue out of sample-offset
/// order. `sort_param_points` must restore the ascending order VST3
/// requires — across BOTH sources merged into one param.
#[test]
fn sort_param_points_orders_merged_host_and_cc_points() {
    let mapping = mod_wheel_mapping(500);
    // Two mapped mod-wheel CCs arriving OUT of frame order (100 before 10).
    let events = [
        (
            at(100),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(100),
            ),
        ),
        (
            at(10),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(10),
            ),
        ),
    ];

    let mut filtered = FilteredMidi::new();
    let mut params = ParameterChanges::reserved(2, 3);
    // Seed the SAME param (500) with host automation at a middle offset,
    // plus a separate param to prove per-queue sorting.
    params.add_change(500u32.into(), 50, 0.5);
    params.add_change(42u32.into(), 30, 0.1);
    params.add_change(42u32.into(), 5, 0.2);
    route_cc_events(&mapping, &events, &mut filtered, &mut params);

    // Before sorting, param 500 is [50 (host), 100 (cc), 10 (cc)] — unsorted.
    sort_param_points(&mut params);

    for param_id in [500, 42] {
        let q = params.queue(param_id.into()).expect("queue present");
        let offsets: Vec<i32> = q.points.iter().map(|p| p.sample_offset).collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        assert_eq!(offsets, sorted, "param {param_id} points not ascending");
    }
    // Param 500 carries all three merged points (1 host + 2 CC).
    assert_eq!(params.queue(500u32.into()).unwrap().points.len(), 3);
}

/// Points sharing a `sample_offset` keep the order they were added in.
///
/// VST3 spells a discontinuity as two points — the old value, then the new
/// one — and lets them share an offset. Which comes first *is* the jump's
/// direction, so a sort that reorders equal keys turns a rise into a fall.
///
/// The sibling test above cannot catch this: its offsets are all distinct
/// (50, 100, 10), and with no equal keys the two sorts are
/// indistinguishable. It asserts only that the result ascends, which stays
/// true either way.
///
/// Size and shape here are deliberate, not padding. Rust's unstable sort is
/// a pattern-defeating quicksort that runs insertion sort on short slices
/// and detects already-ordered runs, so it happens to preserve order on
/// small or tidy inputs — measured: a handful of tied points, or ties in
/// ascending order, come out identical under both sorts at every size
/// tried. The two diverge from roughly 32 points when the offsets arrive
/// *descending*, which is what this builds. That is a reachable shape:
/// `add_point` has no cap, and a dense automation curve merged with CC
/// traffic on one param exceeds it. A smaller fixture would pass against
/// either sort and pin nothing.
#[test]
fn points_sharing_a_sample_offset_keep_their_insertion_order() {
    let mut params = ParameterChanges::reserved(1, 80);
    let param = 500u32.into();

    // 40 offsets arriving high-to-low, each carrying a two-point jump.
    // `value` encodes arrival order so any transposition is visible.
    //
    // The encoding has to stay inside `0..=1`: a point's value is a
    // `Normalized`, which *clamps* rather than rejecting, so a raw counter
    // would land every point on 1.0 and compare equal no matter how the
    // sort transposed them — the assertion would hold against a sort that
    // shuffled ties freely. Dividing by the point count keeps all 80 values
    // distinct and ordered, which is the property being pinned.
    const OFFSETS: i32 = 40;
    const POINTS: f64 = (OFFSETS * 2) as f64;
    let mut expected = Vec::new();
    for i in 0..OFFSETS {
        let offset = (OFFSETS - i) * 4;
        let old = f64::from(i * 2) / POINTS;
        let new = f64::from(i * 2 + 1) / POINTS;
        params.add_change(param, offset, old);
        params.add_change(param, offset, new);
        expected.push((offset, old, new));
    }
    expected.sort_by_key(|(offset, _, _)| *offset);

    sort_param_points(&mut params);

    let q = params.queue(param).expect("queue present");
    let got: Vec<(i32, f64)> = q
        .points
        .iter()
        .map(|p| (p.sample_offset, p.value.get()))
        .collect();

    let want: Vec<(i32, f64)> = expected
        .iter()
        .flat_map(|(offset, old, new)| [(*offset, *old), (*offset, *new)])
        .collect();

    assert_eq!(
        got, want,
        "each offset carries a jump old -> new; transposing a tied pair \
         inverts that jump's direction"
    );
}

/// A route with no CC mapping reserves no param scratch, and the scratch
/// is reserved when [`CcRoute::set_mapping`] (what
/// `rebuild_midi_cc_mapping` calls on the control thread) installs a
/// non-empty mapping, and released when it installs an empty one. The
/// sizes are the production cap (16384 queues) and a 4096-frame block's
/// chunk room, where reserving regardless costs tens of megabytes per
/// instance. The first block after the rebuild is then routed inside
/// the no-alloc gate with no warm-up, which the reservation makes
/// allocation-free.
///
/// Mutations (run): `new` reserving regardless of the mapping (the
/// previous code) → the empty route's `capacity()` is 16384 → fails;
/// `set_mapping` not reserving when the mapping becomes non-empty →
/// `capacity()` stays 0 → fails (and the next test, whose `new` goes
/// through `set_mapping`, aborts in its gate); `set_mapping` keeping the
/// scratch when the mapping empties → the last `capacity()` is 16384 →
/// fails.
#[test]
fn cc_route_reserves_only_while_mapped() {
    const QUEUES: usize = 1 << 14;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(4096);

    let mut route = CcRoute::new(MidiCcMapping::empty(), QUEUES, room);
    assert_eq!(route.param_changes.capacity(), 0, "no mapping, no queues");
    assert_eq!(route.param_changes.point_storage(), 0, "no spare queues");
    assert!(route.route(&[], None).is_none());

    route.set_mapping(mod_wheel_mapping(500));
    assert_eq!(route.param_changes.capacity(), QUEUES);
    assert_eq!(
        route.param_changes.point_storage(),
        ParameterChanges::point_budget(QUEUES, room)
    );

    let events = [(
        at(3),
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            tutti_fixtures::midi::wide_value(64),
        ),
    )];
    let mut host = ParameterChanges::reserved(1, 1);
    host.add_change(7u32.into(), 0, 0.25);
    assert_no_alloc::assert_no_alloc(|| {
        let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
        assert!(midi.is_empty(), "the only event was a mapped CC");
        assert_eq!(params.queues.len(), 2, "host automation + the CC");
    });

    route.set_mapping(MidiCcMapping::empty());
    assert_eq!(route.param_changes.capacity(), 0, "released when unmapped");
}

/// **A route that follows the control thread's mapping routes against what
/// is published**, from the next block, and an empty publication hands the
/// controllers back to the plugin as MIDI. The Processor cannot own a
/// mapping the control thread changes, so it lends the current one per
/// block; the scratch was reserved at activation.
///
/// Mutation (run): `route` reading its own `mapping` instead of the lent
/// one (`shared` ignored) → the published table is never used → the first
/// assertion fails.
#[test]
fn a_following_route_uses_the_published_mapping() {
    use tutti_rt::ArcLend;

    let cell = std::sync::Arc::new(ArcLend::new(MidiCcCell(MidiCcMapping::empty())));
    let mut route = CcRoute::following(cell.reader(), true, 4, 4);
    let events = [(
        at(3),
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            tutti_fixtures::midi::wide_value(64),
        ),
    )];

    assert!(
        route.route(&events, None).is_none(),
        "nothing is mapped yet, so the MIDI passes through untouched"
    );

    cell.publish(MidiCcCell(mod_wheel_mapping(500)));
    assert_no_alloc::assert_no_alloc(|| {
        let (midi, params) = route.route(&events, None).expect("mapped");
        assert!(midi.is_empty(), "the CC became a parameter point");
        assert_eq!(params.queues.len(), 1);
    });

    cell.publish(MidiCcCell(MidiCcMapping::empty()));
    assert!(route.route(&events, None).is_none(), "unmapped again");
}

/// The very first block is allocation-free with no warm-up: every queue
/// it opens is a reserved spare with room for `points` points, so the
/// host automation it seeds (several points on one parameter) and the
/// CC point it appends push into existing storage. This is what the
/// `points` argument buys; the warm-up test below cannot see it, because
/// its warm-up grows the queues before the gate.
///
/// Mutation (run): `CcRoute::new(mapping, 4, 0)` (reserved queues with
/// no chunk room, only the carried value's point) → the host queue
/// keeps one point (its latest), not four, and the assertion inside the
/// gate aborts → fails. (Reworded with
/// `PointBuf`: the seeded points allocated inside the gate before.)
#[test]
fn cc_route_first_block_is_allocation_free_without_warmup() {
    let events = [
        (
            at(16),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(64),
            ),
        ),
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
    ];
    let mut host = ParameterChanges::reserved(1, 4);
    for (offset, value) in [(0, 0.1), (8, 0.2), (24, 0.3), (40, 0.4)] {
        host.add_change(7u32.into(), offset, value);
    }

    let mut route = CcRoute::new(mod_wheel_mapping(500), 4, 4);
    assert_no_alloc::assert_no_alloc(|| {
        let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
        assert_eq!(midi.len(), 1, "the note passes through");
        assert_eq!(params.queues.len(), 2, "host automation + the CC");
        let host_queue = params.queue(7u32.into()).expect("host queue");
        assert_eq!(host_queue.points.len(), 4);
    });
}

/// **Merged host automation and CC points never grow the reserved
/// scratch, past its pool too.** 1 000 parameters at a 512-frame block,
/// reserved as activation reserves the route. Warmed with one pattern
/// (the host's chunk moving parameters `0..256` at a full room, one CC
/// for the mapped parameter), then measured with another inside the
/// no-alloc gate (parameters `256..512`, so no warm-up left them storage,
/// and four CCs for the mapped parameter, whose fourth is past its floor
/// of `FLOOR_POINTS + 1` with the scratch's pool spent): nothing
/// allocates, the mapped parameter ends on its latest CC value, and one
/// point is counted. (Reworded with `PointBuf` and the carried layout:
/// three CCs, past a floor of `FLOOR_POINTS`, before.)
///
/// Mutation (run): `PointBuf::replace_latest` dropping the point (the
/// queue keeping its earlier points) → the mapped parameter ends on its
/// third CC's value → fails.
#[test]
fn cc_points_past_the_scratch_pool_give_way_without_allocating() {
    const QUEUES: usize = 1_000;
    const MAPPED: u32 = 999;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let pooled = ParameterChanges::POOLED_QUEUES as u32;
    let chunk = |moving: core::ops::Range<u32>| {
        let mut host = ParameterChanges::reserved(QUEUES, room);
        for id in moving {
            for k in 0..room {
                assert!(host.add_change(id.into(), k as i32, 0.5));
            }
        }
        host
    };
    let cc = |frame: u32, value: u32| {
        (
            at(frame),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(value),
            ),
        )
    };
    let (warm, measured) = (chunk(0..pooled), chunk(pooled..2 * pooled));
    let one = [cc(0, 0x1400_0000)];
    let four = [
        cc(0, 0x1400_0000),
        cc(8, 0x2800_0000),
        cc(16, 0x3C00_0000),
        cc(24, 0xFFFF_FFFF),
    ];
    let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), QUEUES, room);
    let _ = route.route(&one, Some(&warm)).expect("mapped");
    assert_eq!(route.points_overflowed(), 0);
    assert_no_alloc::assert_no_alloc(|| {
        let _ = route.route(&four, Some(&measured)).expect("mapped");
    });
    assert_eq!(route.points_overflowed(), 1);
    let (_, params) = route.route(&four, Some(&measured)).expect("mapped");
    let q = params.queue(MAPPED.into()).expect("the CC");
    assert_eq!(q.points.len(), ParameterChanges::FLOOR_POINTS + 1);
    assert_eq!(
        q.points.last().map(|p| (p.sample_offset, p.value.get())),
        Some((24, 1.0))
    );
    assert_eq!(route.points_overflowed(), 2);
}

/// **The host's chunk reaches the route whole, carried values and all,
/// wherever its full queues are.** 2 000 parameters at a 512-frame
/// block, the route reserved as activation reserves it. The host's
/// chunk as the bridge hands it on, moving every parameter but the
/// mapped one: every floor queue carrying three points (two of its own
/// and the skipped block's carried value) and the *last*
/// `POOLED_QUEUES` moved at their room and the carried value. The route
/// copies it in queue order; nothing is counted, with or without a CC
/// for the mapped parameter (which takes a spare queue), and nothing
/// allocates.
///
/// Mutation (run): the seed pushing into each queue's own room only
/// (`try_push` on the queue, no block borrowed) → the full queues'
/// points past their floor are counted → fails. (Reworded with the one
/// layout: the mutation was the scratch in the plain layout, which no
/// longer exists.)
#[test]
fn a_bridged_chunk_reaches_the_route_whole() {
    const QUEUES: usize = 2_000;
    const MAPPED: u32 = QUEUES as u32 - 1;
    let moved = QUEUES - 1;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let full = moved - ParameterChanges::POOLED_QUEUES..moved;
    let mut host = ParameterChanges::reserved_bridged(QUEUES, room);
    for id in 0..moved {
        let points = if full.contains(&id) {
            room
        } else {
            ParameterChanges::FLOOR_POINTS
        };
        for k in 0..points {
            assert!(host.add_change((id as u32).into(), 1 + k as i32, 0.5));
        }
        assert!(host.prepend_carried(
            id,
            tutti_plugin_types::ParameterPoint {
                sample_offset: 0,
                value: tutti_plugin_types::Normalized::of(0.25),
            },
        ));
    }
    let mapped = (
        Offset::ZERO,
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            tutti_fixtures::midi::wide_value(64),
        ),
    );
    for events in [&[][..], &[mapped][..]] {
        let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), QUEUES, room);
        assert_no_alloc::assert_no_alloc(|| {
            let _ = route.route(events, Some(&host)).expect("mapped");
        });
        assert_eq!(route.points_overflowed(), 0, "{} CC events", events.len());
        let (_, params) = route.route(events, Some(&host)).expect("mapped");
        for (got, want) in params.queues.iter().zip(&host.queues) {
            assert_eq!(got.param_id, want.param_id);
            assert_eq!(got.points.len(), want.points.len());
        }
        let cc = params.queue(MAPPED.into());
        assert_eq!(
            cc.map(|q| q.points.len()),
            (!events.is_empty()).then_some(1)
        );
    }
}

/// **The seed copies the host's chunk queue by queue, and counts what
/// it cannot hold.** A route reserved for two queues of a room of two
/// (three points each), seeded inside the no-alloc gate from a chunk of
/// three queues: the first two are copied, the first's fourth point
/// keeping its latest value (one counted), and the third queue, past
/// the reservation, is dropped with its two points counted.
///
/// Mutations (run), each fails: the seed not counting a point past a
/// queue's room → the count is 2; not counting a queue past the
/// reservation → the count is 1. Not covered: the seed's cost, linear in
/// the chunk (through `add_change`, as it was, it scanned the open
/// queues for every point); nothing but time observes it.
#[test]
fn the_seed_copies_the_host_chunk_and_counts_what_it_cannot_hold() {
    let mut host = ParameterChanges::reserved(3, 4);
    for (id, points) in [(1u32, 4), (2, 3), (3, 2)] {
        for k in 0..points {
            assert!(host.add_change(id.into(), 10 * k, 0.5));
        }
    }
    let mut route = CcRoute::new(mod_wheel_mapping(500), 2, 2);
    assert_no_alloc::assert_no_alloc(|| {
        let _ = route.route(&[], Some(&host)).expect("mapped");
    });
    let got: Vec<(u32, Vec<i32>)> = route
        .param_changes
        .queues
        .iter()
        .map(|q| {
            let id = q.param_id.get();
            (id, q.points.iter().map(|p| p.sample_offset).collect())
        })
        .collect();
    assert_eq!(got, [(1, vec![0, 10, 30]), (2, vec![0, 10, 20])]);
    assert_eq!(route.points_overflowed(), 3);
}

/// **A queue with two sources ends on its latest value.** The host's
/// chunk gives a parameter a full queue, its last point late in the
/// block; a mapped CC for it arrives earlier, with the scratch's room
/// spent. The CC is earlier than the host's latest point, so the host's
/// point stays and the queue ends on it; a CC later than every host
/// point takes the latest's place and the queue ends on the CC.
///
/// Mutation (run): the merge replacing the queue's last point whatever
/// the new point's time (`PointBuf::replace_latest` without its `>=`
/// check, as the `pop` and push did) → the early CC evicts the host's
/// frame-400 point and the sorted queue ends on frame 300 → fails. (The
/// last point *added* rather than the latest in time, `.last()` in
/// `replace_latest`, survives here, since the host's points arrive in
/// order; `push_or_replace_last_keeps_the_latest_value` kills it.)
#[test]
fn a_queue_with_host_and_cc_points_ends_on_its_latest_value() {
    const MAPPED: u32 = 7;
    // One queue, at the full carried room (`room + 1`), the host's chunk
    // filling it.
    let room = 4;
    let mut host = ParameterChanges::reserved(1, room + 1);
    for (offset, value) in [(0, 0.0), (100, 0.25), (200, 0.5), (300, 0.625), (400, 0.75)] {
        assert!(host.add_change(MAPPED.into(), offset, value));
    }
    let cc = |frame: u32, value: u32| {
        (
            at(frame),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(value),
            ),
        )
    };
    let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), 1, room);
    let mut ends = |events: &[UmpAt]| {
        let (_, params) = route.route(events, Some(&host)).expect("mapped");
        let q = params.queue(MAPPED.into()).expect("queued");
        assert!(
            q.points
                .windows(2)
                .all(|w| w[0].sample_offset <= w[1].sample_offset)
        );
        q.points.last().map(|p| (p.sample_offset, p.value.get()))
    };
    assert_eq!(ends(&[cc(150, 0xFFFF_FFFF)]), Some((400, 0.75)));
    assert_eq!(ends(&[cc(450, 0xFFFF_FFFF)]), Some((450, 1.0)));
    assert_eq!(route.points_overflowed(), 2);
}

/// **The merge sorts a queue at a chunk's full room without
/// allocating, stably.** One parameter at a 4096-frame block's carried
/// room (`chunk_room(4096) + 1` = 386 points): the host's chunk fills
/// all but eight with points every 8 frames, and eight mapped CCs land
/// on frames among them (four on a host point's frame), so the merged
/// queue is out of order and must be sorted, inside the no-alloc gate.
/// The result is in time order, holds every point, and a CC that shares
/// a frame with a host point comes after it (the host's points are
/// seeded first).
///
/// Mutation (run): `sort_param_points` back to `queue.points
/// .sort_by_key(|point| point.sample_offset)` → std's stable sort
/// allocates its scratch for 386 points → the gate aborts. (Before the
/// raised room this could not be seen: a queue held at most 133 points,
/// under std's 256-point stack buffer.)
#[test]
fn the_merge_sorts_a_full_room_queue_without_allocating() {
    const MAPPED: u32 = 7;
    const CCS: usize = 8;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(4096);
    let full = room + 1;
    assert_eq!(full, 386);
    let mut host = ParameterChanges::reserved(1, room);
    for k in 0..full - CCS {
        assert!(host.add_change(MAPPED.into(), 8 * k as i32, 0.25));
    }
    let cc = |frame: u32, value: u32| {
        (
            at(frame),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(value),
            ),
        )
    };
    // On host frames (multiples of 8) and between them.
    let events: Vec<UmpAt> = (0..CCS as u32)
        .map(|k| cc(100 * k + if k % 2 == 0 { 0 } else { 3 }, 0xFFFF_FFFF))
        .collect();
    let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), 1, room);
    let sorted = assert_no_alloc::assert_no_alloc(|| {
        let (_, params) = route.route(&events, Some(&host)).expect("mapped");
        let q = params.queue(MAPPED.into()).expect("queued");
        q.points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .fold((0usize, true, i32::MIN), |(n, ok, last), (o, _)| {
                (n + 1, ok && last <= o, o)
            })
    });
    assert_eq!(sorted, (full, true, 8 * (full - CCS - 1) as i32));
    assert_eq!(route.points_overflowed(), 0);
    let (_, params) = route.route(&events, Some(&host)).expect("mapped");
    let q = params.queue(MAPPED.into()).expect("queued");
    // A shared frame: the host's point, then the CC's.
    for k in (0..CCS as i32).step_by(2) {
        let at: Vec<f64> = q
            .points
            .iter()
            .filter(|p| p.sample_offset == 100 * k)
            .map(|p| p.value.get())
            .collect();
        assert_eq!(at, [0.25, 1.0], "frame {}", 100 * k);
    }
}

/// RT regression: once the scratch buffers are warmed, repeated routing
/// must not allocate. Drives [`CcRoute::route`] itself, host automation
/// seeded and all, rather than a copy of its clear discipline: the
/// discipline is what changed when the queues stopped holding points
/// inline (a queue dropped by `queues.clear()` and rebuilt has no room,
/// so its first point allocates), and a copy would not have followed it.
///
/// Mutation (run): `route` clearing with `queues.clear()` (dropping the
/// queues, the old discipline) instead of `ParameterChanges::clear` →
/// every block rebuilds its queues inside the gate → fails.
#[test]
fn route_cc_events_is_allocation_free_after_warmup() {
    let mapping = mod_wheel_mapping(500);
    let events = [
        (
            at(0),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                tutti_fixtures::midi::wide_value(64),
            ),
        ),
        (
            at(0),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000),
        ),
        (
            at(64),
            midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
        (
            at(32),
            midi::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                tutti_fixtures::midi::wide_value(10),
            ),
        ),
    ];

    let mut route = CcRoute::new(mapping, 4, 16);
    let mut host = ParameterChanges::reserved(1, 2);
    host.add_change(7u32.into(), 0, 0.25);
    host.add_change(7u32.into(), 48, 0.75);

    // Warm up once, outside the gate.
    let _ = route.route(&events, Some(&host));

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
            assert_eq!(midi.len(), 3, "the mapped CC is taken out");
            assert_eq!(params.queues.len(), 2, "host automation + the CC");
        }
    });
}

/// The SysEx messages `list` hands the plugin.
fn sysex_in(list: &EventList) -> Vec<Vec<u8>> {
    list.staged_sysex()
}

fn sysex_packets(payload: &[u8]) -> Vec<UmpAt> {
    let mut out = Vec::new();
    out.extend(
        tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, payload).expect("a 7-bit payload"),
    );
    out.into_iter().map(|ump| (Offset::ZERO, ump)).collect()
}

/// **A plugin with a CC mapping gets a SysEx spanning blocks whole**: a
/// block of the node's whole MIDI cap (a mapped CC and 255 SysEx packets)
/// and the rest of the message in the next block arrive as one complete
/// message, nothing dropped.
///
/// Mutation (run): the route's MIDI scratch back to `RtMidiEvents` (64
/// events) → the first block's packets past 64 are lost, the message is
/// dropped → fails.
#[test]
fn a_mapped_route_passes_a_sysex_spanning_blocks_whole() {
    // 266 packets: 255 in the first block, 11 in the second.
    let payload: Vec<u8> = (0..1596).map(|i| (i % 0x7F) as u8).collect();
    let packets = sysex_packets(&payload);
    assert_eq!(packets.len(), 266);
    let cc = (
        Offset::ZERO,
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            tutti_fixtures::midi::wide_value(64),
        ),
    );
    let first: Vec<UmpAt> = std::iter::once(cc)
        .chain(packets[..MIDI_STACK_CAPACITY - 1].iter().copied())
        .collect();
    assert_eq!(first.len(), MIDI_STACK_CAPACITY);
    let mut route = CcRoute::new(mod_wheel_mapping(500), 1, 64);
    let list = EventList::input();
    let mut stager = Stager::default();

    let one = InputEvents {
        midi: &first,
        ..Default::default()
    };
    let params =
        stage_input_events(&mut route, &mut stager, &list, &one, None).expect("the list is free");
    assert_eq!(
        params.map(|p| p.queues.len()),
        Some(1),
        "the CC became a param"
    );
    assert!(sysex_in(&list).is_empty(), "the message is still open");

    let two = InputEvents {
        midi: &packets[MIDI_STACK_CAPACITY - 1..],
        ..Default::default()
    };
    stage_input_events(&mut route, &mut stager, &list, &two, None).expect("the list is free");
    let mut framed = vec![0xF0];
    framed.extend_from_slice(&payload);
    framed.push(0xF7);
    assert_eq!(sysex_in(&list), vec![framed]);
    assert_eq!(list.sysex_dropped(), 0);
}

/// **More MIDI than the route holds drops the open SysEx, counted, rather
/// than splicing its later packets onto the kept ones.** A direct caller
/// may stage more than a block's cap; the route keeps the first
/// [`MIDI_STACK_CAPACITY`].
///
/// Mutation (run): the `lose_sysex_tail` call removed from
/// `stage_input_events` → the next block's packets complete the message
/// with the lost ones missing → fails.
#[test]
fn a_route_overflow_drops_the_open_sysex() {
    let payload: Vec<u8> = (0..6 * 400).map(|i| (i % 0x7F) as u8).collect();
    let packets = sysex_packets(&payload);
    assert_eq!(packets.len(), 400);
    let mut route = CcRoute::new(mod_wheel_mapping(500), 1, 64);
    let list = EventList::input();
    let mut stager = Stager::default();
    // 300 packets in one call: the route keeps 256 and loses 44.
    let one = InputEvents {
        midi: &packets[..300],
        ..Default::default()
    };
    stage_input_events(&mut route, &mut stager, &list, &one, None).expect("the list is free");
    assert_eq!(list.sysex_dropped(), 1);
    let two = InputEvents {
        midi: &packets[300..],
        ..Default::default()
    };
    stage_input_events(&mut route, &mut stager, &list, &two, None).expect("the list is free");
    assert!(sysex_in(&list).is_empty(), "no message with a gap in it");
    assert_eq!(list.sysex_dropped(), 1);
}

/// A MIDI 1.0 controller and a MIDI 2.0 one map to the same slot, and a
/// packet that is not channel voice, or that `decode` cannot frame, maps to
/// nothing.
///
/// Mutation (run): the `Err(Unknown(_))` arm of `midi_to_mapped_controller`
/// returning `Some((0, 1, 0.0))` → the reserved packet maps → fails; dropping
/// the `widen` of a `Midi1` message → the MIDI 1.0 CC is `None` → fails.
#[test]
fn a_packet_is_mapped_by_what_it_decodes_to() {
    use tutti_ump::{Address, Message, U7, Ump, Voice1};

    let address = Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(2),
    };
    let packet = |message| (Offset::ZERO, Ump::from(message));
    let one_cc = packet(Message::Midi1(
        address,
        Voice1::ControlChange {
            controller: CCNumber::MOD_WHEEL,
            value: U7::MAX,
        },
    ));
    // Full scale widens to 0xFFFF_FFFF, which is exactly 1.0.
    assert_eq!(mapped(&one_cc), Some((2, 1, 1.0)));
    assert_eq!(
        mapped(&(Offset::ZERO, midi::timing_clock(MidiGroup::FIRST))),
        None
    );
    assert_eq!(mapped(&raw(&[0x6000_0000])), None);
    assert_eq!(mapped(&raw(&[0x4070_0000, 0])), None);
}

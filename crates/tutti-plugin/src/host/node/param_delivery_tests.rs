//! The delivery's tests that need the process edge or a format host: they live
//! here because `tutti-plugin-host` names neither.
//!
//! The rest of the delivery's tests are `tutti-plugin-host`'s own
//! (`param_delivery`'s `tests` module), whose `more_lanes_than_the_pool_end_on_
//! their_latest_values_counted` the first test below extends past the plugin
//! server's input list.

use std::sync::Arc;
use tutti_plugin_host::{DeliveryCounters, ParamDelivery};
use tutti_plugin_types::{ParamId, ParameterQueue};

#[cfg(feature = "clap")]
fn op(id: u32) -> ParamId {
    ParamId::new(id)
}

#[cfg(feature = "clap")]
fn delivery(n: u32) -> (ParamDelivery, Arc<DeliveryCounters>) {
    let counters = Arc::new(DeliveryCounters::default());
    (
        ParamDelivery::new((0..n).map(op), Arc::clone(&counters)),
        counters,
    )
}

/// One chunk of `more_lanes_than_the_pool`'s shape (`tutti-plugin-host`) for
/// `lanes` lanes of a delivery prepared for 512-frame chunks: in time order,
/// frame by frame and lane by lane, a grid point on every 32nd frame, a corner
/// 7 frames after each, and a corner on the chunk's last frame. Each lane's
/// values step through sevenths of a cycle shifted by `phase`, so every point
/// is a change. Writes each lane's latest value into `latest` (one a lane).
#[cfg(feature = "clap")]
fn lane_chunk(d: &mut ParamDelivery, latest: &mut [f32], phase: usize) {
    let lanes = latest.len() as u32;
    let mut k = 0;
    d.begin();
    for f in 0..512i32 {
        let grid = f % 32 == 0;
        if !(grid || f % 32 == 7 || f == 511) {
            continue;
        }
        for id in 0..lanes {
            let v = ((k + id as usize + 3 * phase) % 7 + 1) as f32 / 8.0;
            if grid {
                d.grid_point(op(id), f, v);
            } else {
                d.point(op(id), f, v);
            }
            latest[id as usize] = v;
        }
        k += 1;
    }
}

/// **The pool-spent chunk reaches a CLAP plugin whole and in time
/// order**, without allocating: the chunk of
/// `more_lanes_than_the_pool_end_on_their_latest_values_counted`
/// (`tutti-plugin-host`'s `param_delivery` tests),
/// as the bridge hands it on, is flattened into a CLAP input list
/// reserved as the plugin-server reserves it at activation
/// (`ParameterChanges::point_budget` of the plugin's parameters at the
/// chunk's room) inside the no-alloc gate. Every point is handed on,
/// in time order, and every lane's last event is its latest value on
/// the chunk's last frame.
#[cfg(feature = "clap")]
#[test]
fn a_pool_spent_chunk_reaches_a_clap_plugin_whole() {
    const PARAMS: u32 = 600;
    const LANES: u32 = 300;
    let (mut d, _) = delivery(PARAMS);
    let room = d.prepare(512);
    let mut latest = vec![0.0; LANES as usize];
    lane_chunk(&mut d, &mut latest, 0);
    let budget = tutti_plugin_types::ParameterChanges::point_budget(PARAMS as usize, room);
    let mut list = tutti_clap_host::InputEventList::new();
    list.reserve(budget);
    list.set_param_point_room(budget);
    let carried: usize = d.changes().queues.iter().map(|q| q.points.len()).sum();
    assert_no_alloc::assert_no_alloc(|| {
        list.add_param_changes(&*d.changes(), &[], false);
        list.clamp_times(512);
        list.sort_by_time();
    });
    assert_eq!(list.param_points_dropped(), 0);
    assert_eq!(list.events().len(), carried);
    let mut last = vec![None; LANES as usize];
    let mut time = 0;
    for e in list.events() {
        let tutti_clap_host::ClapEvent::ParamValue(p) = e else {
            panic!("only parameter events");
        };
        assert!(p.header.time >= time, "time order");
        time = p.header.time;
        last[p.param_id as usize] = Some((p.header.time, p.value));
    }
    for (id, &v) in latest.iter().enumerate() {
        assert_eq!(last[id], Some((511, f64::from(v))), "lane {id}");
    }
}

// Moved from `tutti-plugin-ipc`'s `payload_pool` tests with the crate split
// (design 021 step 3): it fills the bridge's buffers through this node's
// delivery.
/// **A buffer of the room before a re-prepare is dropped on its way
/// back, never handed to the node** (the sequence that broke it). A
/// 2 000-parameter node is prepared for 4096-frame chunks and its pool
/// reserved to match; a chunk goes, so a 4096-room buffer is in flight;
/// the node is re-prepared for 512 frames and the pool re-reserved; the
/// audio thread takes a buffer meanwhile, so the free list has a place
/// when the old one comes back. Then chunk after chunk, each swapped for
/// a free buffer as `submit` does and returned as the bridge does, with
/// 20 lanes offering a point on every frame (512 a queue, past the room
/// of 273). Every buffer the node fills must be of its own room: the
/// grid marks (`GridBits`) are laid out for it, and the server's lists
/// are reserved for it.
#[test]
fn a_buffer_of_the_room_before_a_re_prepare_is_dropped_not_reused() {
    let counters = Arc::new(DeliveryCounters::default());
    let mut d = ParamDelivery::new((0..2000).map(ParamId::new), counters);
    let pool = tutti_plugin_ipc::PayloadPool::new();
    let lanes = 20u32;
    let load = |d: &mut ParamDelivery, phase: usize| {
        d.begin();
        for f in 0..512i32 {
            for id in 0..lanes {
                let v = ((f as usize + id as usize + phase) % 7 + 1) as f32 / 8.0;
                let address = ParamId::new(id);
                if f % 32 == 0 {
                    d.grid_point(address, f, v);
                } else {
                    d.point(address, f, v);
                }
            }
        }
    };
    // One chunk at 4096 frames goes: its buffer is in flight.
    let points = d.prepare(4096);
    pool.reserve_params(d.room(), points);
    load(&mut d, 0);
    let mut in_flight = pool.take_params().expect("a free buffer");
    std::mem::swap(d.changes(), &mut in_flight);
    d.handed_off(true);
    assert_eq!(in_flight.point_room(), points);

    // Re-prepared for 512, and re-reserved.
    let points = d.prepare(512);
    assert_eq!(points, ParameterQueue::chunk_room(512));
    pool.reserve_params(d.room(), points);
    // The audio thread takes one, so the list has a place free...
    let taken = pool.take_params().expect("a free buffer");
    // ...when the 4096-room buffer reaches the bridge thread.
    pool.return_params(in_flight);
    pool.return_params(taken);

    for phase in 1..=3 * tutti_plugin_ipc::PARAM_BUFFERS {
        load(&mut d, phase);
        let mut sent = pool.take_params().expect("a free buffer");
        assert_eq!(
            sent.point_room(),
            points,
            "a free buffer of the node's room"
        );
        std::mem::swap(d.changes(), &mut sent);
        d.handed_off(true);
        assert_eq!(sent.len(), lanes as usize);
        for q in &sent.queues {
            assert!(
                q.points.len() <= points,
                "chunk {phase}: {} points, room {points}",
                q.points.len()
            );
        }
        pool.return_params(sent);
    }
}

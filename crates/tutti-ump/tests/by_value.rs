//! Every message fits by value: `Message` is `Copy`, owns no heap and holds no
//! reference, and its size is a small multiple of the packet it decodes from.
//!
//! Mutation: add a `String` or a `&'static str` field to any message type ->
//! `Message` stops being `Copy` and this file stops compiling; add a
//! `[u8; 64]` to a variant -> `a_message_is_at_most_three_packets` fails.

use tutti_ump::{Message, Ump, Unknown};

fn copy_send_sync_static<T: Copy + Send + Sync + 'static>() {}

#[test]
fn a_message_is_a_plain_value() {
    copy_send_sync_static::<Message>();
    copy_send_sync_static::<Unknown>();
    copy_send_sync_static::<Ump>();
}

#[test]
fn a_message_is_at_most_three_packets() {
    let size = core::mem::size_of::<Message>();
    eprintln!("size_of::<Message>() = {size}");
    assert!(size <= 3 * core::mem::size_of::<Ump>(), "{size} bytes");
}

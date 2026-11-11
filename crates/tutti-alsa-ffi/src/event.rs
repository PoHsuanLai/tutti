//! What one read from the sequencer returns, copied out of alsa-lib's buffer.

use std::time::Duration;

use crate::sys;

/// One UMP event read from a port, owned: alsa-lib's buffer is valid only
/// until the next read, so nothing here borrows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// The event's four UMP words; a message is 1 to 4 of them and the rest
    /// are padding.
    pub ump: [u32; 4],
    /// When the port stamped the event, as the time since its queue started;
    /// `None` when the port is not stamping.
    pub stamp: Option<Duration>,
    kind: u8,
}

impl Received {
    /// Copies what the backend needs out of `ev`.
    pub(crate) fn from_event(ev: &sys::snd_seq_ump_event_t) -> Self {
        Self {
            ump: ev.ump,
            stamp: ev
                .real_time()
                .map(|t| Duration::new(u64::from(t.tv_sec), t.tv_nsec)),
            kind: ev.type_,
        }
    }

    /// What this event announces, if it came from the system announce port and
    /// says a client or port started, exited or changed. The event's data
    /// begins with the address of the client (and port) it is about.
    #[must_use]
    pub fn announced(&self) -> Option<Announced> {
        let data = self.ump[0].to_ne_bytes();
        let (client, port) = (i32::from(data[0]), i32::from(data[1]));
        match self.kind {
            sys::SND_SEQ_EVENT_CLIENT_START => Some(Announced::ClientStart(client)),
            sys::SND_SEQ_EVENT_CLIENT_EXIT => Some(Announced::ClientExit(client)),
            sys::SND_SEQ_EVENT_CLIENT_CHANGE => Some(Announced::ClientChange(client)),
            sys::SND_SEQ_EVENT_PORT_START => Some(Announced::PortStart(client, port)),
            sys::SND_SEQ_EVENT_PORT_EXIT => Some(Announced::PortExit(client, port)),
            sys::SND_SEQ_EVENT_PORT_CHANGE => Some(Announced::PortChange(client, port)),
            _ => None,
        }
    }
}

/// An announcement from the system announce port, with the client (and port)
/// it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Announced {
    /// A client opened.
    ClientStart(i32),
    /// A client closed.
    ClientExit(i32),
    /// A client set its info (its name among it).
    ClientChange(i32),
    /// A port was created: `(client, port)`.
    PortStart(i32, i32),
    /// A port was deleted: `(client, port)`.
    PortExit(i32, i32),
    /// A port's info was set, its name or capabilities: `(client, port)`.
    PortChange(i32, i32),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u8, client: u8, port: u8) -> Received {
        Received {
            ump: [u32::from_ne_bytes([client, port, 0, 0]), 0, 0, 0],
            stamp: None,
            kind,
        }
    }

    /// Each announcement kind names its client, and a port kind its port too.
    /// Mutations: swapping `client` and `port` in `PortStart` fails the port
    /// assertions; mapping `PORT_EXIT` to `PortChange` fails the exit one.
    #[test]
    fn an_announcement_names_its_client_and_port() {
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_START, 130, 9).announced(),
            Some(Announced::ClientStart(130))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_START, 130, 9).announced(),
            Some(Announced::PortStart(130, 9))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_EXIT, 4, 1).announced(),
            Some(Announced::PortExit(4, 1))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_CHANGE, 4, 0).announced(),
            Some(Announced::ClientChange(4))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_CLIENT_EXIT, 4, 0).announced(),
            Some(Announced::ClientExit(4))
        );
        assert_eq!(
            event(sys::SND_SEQ_EVENT_PORT_CHANGE, 4, 2).announced(),
            Some(Announced::PortChange(4, 2))
        );
    }

    /// Any other event (a note, say) is no announcement, whatever its first
    /// word holds. Mutation: a catch-all arm mapping to `ClientStart` fails.
    #[test]
    fn another_event_announces_nothing() {
        assert_eq!(event(0, 130, 9).announced(), None);
        assert_eq!(event(66, 130, 9).announced(), None);
    }

    /// The stamp is the event's real time as a duration, and none when the
    /// flags say the stamp is a tick. Mutation: dropping `.map` for a fixed
    /// zero fails the first assertion.
    #[test]
    fn a_real_time_stamp_becomes_a_duration() {
        let mut ev = sys::snd_seq_ump_event_t::default();
        ev.time[..4].copy_from_slice(&3u32.to_ne_bytes());
        ev.time[4..].copy_from_slice(&500u32.to_ne_bytes());
        ev.flags = sys::SND_SEQ_TIME_STAMP_REAL;
        assert_eq!(Received::from_event(&ev).stamp, Some(Duration::new(3, 500)));
        ev.flags = 0;
        assert_eq!(Received::from_event(&ev).stamp, None);
    }
}

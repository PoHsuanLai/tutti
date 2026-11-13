//! The in-process host's pass over what a plugin wants waited on: where the
//! plugin server's main loop blocks, a host without a loop of its own looks
//! without waiting, each frame, and hands the plugin what fired
//! ([`EditorWindow::wake`]).
//!
//! The look at the descriptors is the platform's, chosen at the `mod` lines
//! below: unix polls (through the process edge's one OS layer), elsewhere no
//! plugin has a descriptor and the pass is timers alone.

use std::time::Instant;

use tutti_plugin_types::{EditorWindow, MainThread, Ready, Wakeups};

#[cfg(unix)]
#[path = "wakeups/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "wakeups/other.rs"]
mod sys;

thread_local! {
    /// The buffers [`service_now`] reuses, so a plugin with descriptors costs no
    /// allocation per pass. Main thread only: `service_now` takes the token.
    static SCRATCH: std::cell::Cell<Option<Box<(Wakeups, Vec<Ready>)>>> =
        const { std::cell::Cell::new(None) };
}

/// One non-blocking pass over what `plugin` wants waited on: asks it
/// ([`EditorWindow::wakeups`]), looks at its descriptors and the clock without
/// waiting, and hands it what fired ([`EditorWindow::wake`]).
///
/// The handle's `editor_idle` calls it each frame. Cheap when the plugin has
/// nothing registered: one empty description and no clock read.
pub fn service_now(plugin: &mut dyn EditorWindow, main: &MainThread) {
    // Taken out for the pass, so a handler that re-enters finds it empty and
    // works on a fresh pair.
    let mut scratch = SCRATCH.take().unwrap_or_default();
    let (wakeups, ready) = &mut *scratch;
    wakeups.clear();
    plugin.wakeups(main, wakeups);
    if !wakeups.is_empty() {
        sys::poll_now(wakeups, ready);
        let now = Instant::now();
        if !ready.is_empty() || wakeups.is_due(now) {
            plugin.wake(main, ready, now);
        }
    }
    SCRATCH.set(Some(scratch));
}

// Descriptors are POSIX's: the tests are one `cfg` on this module.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    mod unix {
        use super::*;
        use std::time::Duration;
        use tutti_plugin_types::{FdId, Interest};

        /// A plugin that asks for one descriptor and a timer, and records what it
        /// is woken with.
        struct Asks {
            fd: FdId,
            deadline: Instant,
            woken: Vec<(Vec<Ready>, Instant)>,
        }

        impl EditorWindow for Asks {
            fn has_editor(&self, _: &MainThread) -> bool {
                false
            }
            fn open_editor(
                &mut self,
                _: &MainThread,
                _: tutti_plugin_types::WindowHandle,
                _: tutti_plugin_types::ContentScale,
            ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
                unreachable!("no editor")
            }
            fn close_editor(&mut self, _: &MainThread) {}
            fn editor_idle(&mut self, _: &MainThread) {}
            fn editor_capabilities(
                &mut self,
                _: &MainThread,
            ) -> tutti_plugin_types::EditorCapabilities {
                tutti_plugin_types::EditorCapabilities::default()
            }
            fn set_editor_size(
                &mut self,
                _: &MainThread,
                _: tutti_plugin_types::EditorSize,
            ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
                unreachable!("no editor")
            }
            fn poll_editor_resize_request(
                &mut self,
                _: &MainThread,
            ) -> Option<tutti_plugin_types::EditorSize> {
                None
            }
            fn wakeups(&mut self, _: &MainThread, out: &mut Wakeups) {
                out.watch(self.fd, Interest::READ);
                out.due_at(self.deadline);
            }
            fn wake(&mut self, _: &MainThread, ready: &[Ready], now: Instant) {
                self.woken.push((ready.to_vec(), now));
            }
        }

        /// **A pass wakes the plugin when a descriptor it asked for is readable
        /// or its timer is due, and only then.**
        #[test]
        fn a_pass_wakes_the_plugin_with_what_fired_and_not_otherwise() {
            use std::io::Write;
            use std::os::fd::AsRawFd;
            let main = MainThread::claim().expect("one test, one process");
            let (reader, mut writer) = std::io::pipe().unwrap();
            let mut plugin = Asks {
                fd: FdId(reader.as_raw_fd()),
                deadline: Instant::now() + Duration::from_secs(3600),
                woken: Vec::new(),
            };
            service_now(&mut plugin, &main);
            assert!(plugin.woken.is_empty(), "nothing readable, nothing due");
            writer.write_all(b"x").unwrap();
            service_now(&mut plugin, &main);
            let (ready, _) = &plugin.woken[0];
            assert_eq!(
                ready,
                &[Ready {
                    fd: plugin.fd,
                    events: Interest::READ
                }]
            );
            // Drained, and its timer due: it is woken with nothing ready.
            let mut byte = [0u8; 1];
            std::io::Read::read_exact(&mut &reader, &mut byte).unwrap();
            service_now(&mut plugin, &main);
            assert_eq!(plugin.woken.len(), 1, "drained and not due");
            plugin.deadline = Instant::now();
            service_now(&mut plugin, &main);
            assert_eq!(plugin.woken.len(), 2, "a due timer wakes it too");
            assert!(plugin.woken[1].0.is_empty());
        }

        /// **The shared interest bits are the OS layer's**, so a host hands
        /// them across as they are.
        #[test]
        fn the_interest_bits_are_the_os_layers() {
            assert_eq!(Interest::READ.bits(), tutti_plugin_ipc_ffi::READ);
            assert_eq!(Interest::WRITE.bits(), tutti_plugin_ipc_ffi::WRITE);
            assert_eq!(Interest::ERROR.bits(), tutti_plugin_ipc_ffi::ERROR);
        }
    }
}

//! The `HostCallback` and `Plugin` tests that need a plugin built by `main`.

use std::ptr;

use crate::plugin;

/// Create a plugin instance.
///
/// This is a macro to allow you to specify attributes on the created struct.
macro_rules! make_plugin {
        ($($attr:meta) *) => {
            use std::convert::TryFrom;
            use std::os::raw::c_void;

            use crate::main;
            use crate::api::AEffect;
            use crate::host::{Host, OpCode};
            use crate::plugin::{HostCallback, Info, Plugin};

            $(#[$attr]) *
            struct TestPlugin {
                host: HostCallback
            }

            impl Plugin for TestPlugin {
                fn get_info(&self) -> Info {
                    Info {
                        name: "Test Plugin".to_string(),
                        ..Default::default()
                    }
                }

                fn new(host: HostCallback) -> TestPlugin {
                    TestPlugin {
                        host
                    }
                }

                fn init(&mut self) {
                    info!("Loaded with host vst version: {}", self.host.vst_version());
                    assert_eq!(2400, self.host.vst_version());
                    assert_eq!(9876, self.host.get_plugin_id());
                    // Callback will assert these.
                    self.host.begin_edit(123);
                    self.host.automate(123, 12.3);
                    self.host.end_edit(123);
                    self.host.idle();
                }
            }

            #[allow(dead_code)]
            fn instance() -> *mut AEffect {
                extern "C" fn host_callback(
                    _effect: *mut AEffect,
                    opcode: i32,
                    index: i32,
                    _value: isize,
                    _ptr: *mut c_void,
                    opt: f32,
                ) -> isize {
                    match OpCode::try_from(opcode) {
                        Ok(OpCode::BeginEdit) => {
                            assert_eq!(index, 123);
                            0
                        },
                        Ok(OpCode::Automate) => {
                            assert_eq!(index, 123);
                            assert_eq!(opt, 12.3);
                            0
                        },
                        Ok(OpCode::EndEdit) => {
                            assert_eq!(index, 123);
                            0
                        },
                        Ok(OpCode::Version) => 2400,
                        Ok(OpCode::CurrentId) => 9876,
                        Ok(OpCode::Idle) => 0,
                        _ => 0
                    }
                }

                main::<TestPlugin>(host_callback)
            }
        }
    }

make_plugin!(derive(Default));

#[test]
#[should_panic]
fn null_panic() {
    make_plugin!(/* no `derive(Default)` */);

    impl Default for TestPlugin {
        fn default() -> TestPlugin {
            let plugin = TestPlugin {
                host: Default::default(),
            };

            // Should panic
            let version = plugin.host.vst_version();
            info!("Loaded with host vst version: {}", version);

            plugin
        }
    }

    TestPlugin::default();
}

#[test]
fn host_callbacks() {
    let aeffect = instance();
    // SAFETY: `aeffect` is the non-null `AEffect` that `main` just built for `TestPlugin` and
    // leaves alive for the process; reading its `dispatcher` field copies an `Option<fn>`.
    let dispatcher = unsafe { (*aeffect).dispatcher }.expect("this crate installs dispatcher");
    // SAFETY: `aeffect` is this crate's own instance and `dispatcher` the
    // entry point it installs; `Initialize` reads none of the pointer args.
    unsafe {
        dispatcher(
            aeffect,
            plugin::OpCode::Initialize.into(),
            0,
            0,
            ptr::null_mut(),
            0.0,
        );
    }
}

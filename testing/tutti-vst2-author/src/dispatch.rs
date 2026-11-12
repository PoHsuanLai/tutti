//! The plugin-side entry points of the VST 2.4 API: what a host calls into a plugin through its `AEffect`.

use std::cell::Cell;
use std::os::raw::c_void;
use std::{mem, slice};

use tutti_vst2_ffi::{
    api::{self, AEffect, consts::*},
    buffer::AudioBuffer,
    editor::{Key, KeyCode, KnobMode, Rect},
    interfaces::{copy_string, read_string},
};

use crate::aeffect::AEffectExt as _;

/// Deprecated process function.
pub extern "C" fn process_deprecated(
    _effect: *mut AEffect,
    _raw_inputs: *const *const f32,
    _raw_outputs: *mut *mut f32,
    _samples: i32,
) {
}

/// VST2.4 replacing function.
pub extern "C" fn process_replacing(
    effect: *mut AEffect,
    raw_inputs: *const *const f32,
    raw_outputs: *mut *mut f32,
    samples: i32,
) {
    // Handle to the VST
    // SAFETY: VST2 ABI: `effect` is the `AEffect` this library created (it is the one passed back to its own entry points) and the host serialises calls per instance. `effect` is not null-checked.
    let plugin = unsafe { (*effect).get_plugin() };
    // SAFETY: as above; `user` holds the `PluginCache` installed at creation.
    let info = unsafe { (*effect).get_info() };
    let (input_count, output_count) = (info.inputs as usize, info.outputs as usize);
    // SAFETY: VST2 ABI: the host passes `numInputs`/`numOutputs` channel pointers,
    // each addressing `samples` elements, with distinct output buffers. The counts are
    // the ones this plugin declared; `samples` is not checked for being negative.
    let mut buffer = unsafe {
        AudioBuffer::from_raw(
            input_count,
            output_count,
            raw_inputs,
            raw_outputs,
            samples as usize,
        )
    };
    plugin.process(&mut buffer);
}

/// VST2.4 replacing function with `f64` values.
pub extern "C" fn process_replacing_f64(
    effect: *mut AEffect,
    raw_inputs: *const *const f64,
    raw_outputs: *mut *mut f64,
    samples: i32,
) {
    // SAFETY: as in `process_replacing`: the host passes this library's own `effect`.
    let plugin = unsafe { (*effect).get_plugin() };
    // SAFETY: as above.
    let info = unsafe { (*effect).get_info() };
    let (input_count, output_count) = (info.inputs as usize, info.outputs as usize);
    // SAFETY: VST2 ABI: the host passes `numInputs`/`numOutputs` channel pointers,
    // each addressing `samples` elements, with distinct output buffers. The counts are
    // the ones this plugin declared; `samples` is not checked for being negative.
    let mut buffer = unsafe {
        AudioBuffer::from_raw(
            input_count,
            output_count,
            raw_inputs,
            raw_outputs,
            samples as usize,
        )
    };
    plugin.process_f64(&mut buffer);
}

/// VST2.4 set parameter function.
pub extern "C" fn set_parameter(effect: *mut AEffect, index: i32, value: f32) {
    // SAFETY: VST2 ABI: `effect` is the `AEffect` this library created (it is the one passed back to its own entry points) and the host serialises calls per instance.
    unsafe { (*effect).get_params() }.set_parameter(index, value);
}

/// VST2.4 get parameter function.
pub extern "C" fn get_parameter(effect: *mut AEffect, index: i32) -> f32 {
    // The C ABI has no way to say "absent", and a plugin implementing this
    // trait always has a value — `None` here means the plugin did not override
    // `get_parameter` at all, for which 0.0 is the SDK's historical answer.
    // SAFETY: VST2 ABI: `effect` is the `AEffect` this library created (it is the one passed back to its own entry points) and the host serialises calls per instance.
    unsafe { (*effect).get_params() }
        .get_parameter(index)
        .unwrap_or(0.0)
}

/// VST2.4 dispatch function. This function handles dispatching all opcodes to the VST plugin.
///
/// `extern "C"`, so a panic escaping into the host's C++ frame is undefined
/// behaviour. The body is wrapped in `catch_unwind` and a panic degrades to
/// `0` — the VST2 "unhandled" answer for every effect opcode.
pub extern "C" fn dispatch(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch_inner(effect, opcode, index, value, ptr, opt)
    })) {
        Ok(v) => v,
        Err(_) => {
            error!("plugin dispatch panicked (opcode {opcode}); reporting unhandled");
            0
        }
    }
}

fn dispatch_inner(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    use crate::plugin::{CanDo, OpCode};

    // Convert passed in opcode to enum
    let opcode = OpCode::try_from(opcode);
    // Only query plugin or editor when needed to avoid creating multiple
    // concurrent mutable references to the same object.
    // SAFETY: VST2 ABI: `effect` is the `AEffect` this library created and the host
    // serialises calls per instance. Each call hands out a fresh `&mut`, and callers
    // take only one at a time (see the comment above).
    let get_plugin = || unsafe { (*effect).get_plugin() };
    // SAFETY: as for `get_plugin`.
    let get_editor = || unsafe { (*effect).get_editor() };
    // SAFETY: as above; `params` is a shared `&Arc` into the `PluginCache`, which
    // lives until `Shutdown`.
    let params = unsafe { (*effect).get_params() };

    match opcode {
        Ok(OpCode::Initialize) => get_plugin().init(),
        // SAFETY: `effClose` arrives once per instance; `effect` was made by
        // `Box::into_raw` in `VSTPluginMain`, so it is freed exactly once, after its
        // contents. Nothing in this arm uses `params` or the plugin afterwards.
        Ok(OpCode::Shutdown) => unsafe {
            (*effect).drop_plugin();
            drop(Box::from_raw(effect))
        },

        Ok(OpCode::ChangePreset) => params.change_preset(value as i32),
        Ok(OpCode::GetCurrentPresetNum) => return params.get_preset_num() as isize,
        Ok(OpCode::SetCurrentPresetName) => params.set_preset_name(read_string(ptr)),
        Ok(OpCode::GetCurrentPresetName) => {
            let num = params.get_preset_num();
            return copy_string(ptr, &params.get_preset_name(num), MAX_PRESET_NAME_LEN);
        }

        Ok(OpCode::GetParameterLabel) => {
            return copy_string(ptr, &params.get_parameter_label(index), MAX_PARAM_STR_LEN);
        }
        Ok(OpCode::GetParameterDisplay) => {
            return copy_string(ptr, &params.get_parameter_text(index), MAX_PARAM_STR_LEN);
        }
        Ok(OpCode::GetParameterName) => {
            return copy_string(ptr, &params.get_parameter_name(index), MAX_PARAM_STR_LEN);
        }

        Ok(OpCode::SetSampleRate) => {
            get_plugin().set_sample_rate(opt);
            // Let the plugin publish a latency it could not know at
            // construction. Real plugins do this constantly — a linear-phase EQ
            // or an oversampling limiter sizes its filter from the sample rate,
            // so `initialDelay` is 0 until `effSetSampleRate` lands. The trait
            // has no way to reach the `AEffect`, so the dispatch writes back
            // whatever `get_info()` now reports.
            //
            // SAFETY: `effect` is the live `AEffect` this dispatch was invoked
            // through; `initialDelay` is a plain `i32` field.
            unsafe {
                (*effect).initialDelay = get_plugin().get_info().initial_delay;
            }
        }
        Ok(OpCode::SetBlockSize) => get_plugin().set_block_size(value as i64),
        Ok(OpCode::StateChanged) => {
            if value == 1 {
                get_plugin().resume();
            } else {
                get_plugin().suspend();
            }
        }

        Ok(OpCode::EditorGetRect) => {
            if let Some(editor) = get_editor() {
                let size = editor.size();
                let pos = editor.position();

                // The `Rect` the host reads through, owned by this plugin and
                // reused on every call.
                //
                // `effEditGetRect` hands the host a `Rect**` and VST 2.4 gives
                // it no way to release what it points at — there is no matching
                // "free this rect" opcode, and the host cannot know the
                // allocator. So a `Box::into_raw` here leaked one `Rect` per
                // call, and hosts call this repeatedly: once before opening an
                // editor, and again on every resize.
                //
                // Thread-local rather than a `static mut`: the pointer must stay
                // valid after this returns, and a host may drive editors for
                // several plugin instances. Per-thread is the right scope
                // because `effEditGetRect` is a main-thread opcode, so the host
                // reads the value on the same thread that wrote it, before it
                // can call again.
                thread_local! {
                    static EDITOR_RECT: Cell<Rect> = const {
                        Cell::new(Rect { left: 0, top: 0, right: 0, bottom: 0 })
                    };
                }

                EDITOR_RECT.with(|slot| {
                    slot.set(Rect {
                        left: pos.0 as i16,              // x coord of position
                        top: pos.1 as i16,               // y coord of position
                        right: (pos.0 + size.0) as i16,  // x coord of pos + x coord of size
                        bottom: (pos.1 + size.1) as i16, // y coord of pos + y coord of size
                    });
                    // SAFETY: `ptr` is the `Rect**` out-parameter the opcode
                    // documents; the pointer written stays valid for the
                    // thread's lifetime.
                    unsafe {
                        *(ptr as *mut *mut c_void) = slot.as_ptr() as *mut c_void;
                    }
                });

                return 1;
            }
        }
        Ok(OpCode::EditorOpen) => {
            if let Some(editor) = get_editor() {
                // `ptr` is a window handle to the parent window.
                // See the documentation for `Editor::open` for details.
                if editor.open(ptr) {
                    return 1;
                }
            }
        }
        Ok(OpCode::EditorClose) => {
            if let Some(editor) = get_editor() {
                editor.close();
            }
        }

        Ok(OpCode::EditorIdle) => {
            if let Some(editor) = get_editor() {
                editor.idle();
            }
        }

        Ok(OpCode::GetData) => {
            let mut chunks = if index == 0 {
                params.get_bank_data()
            } else {
                params.get_preset_data()
            };

            chunks.shrink_to_fit();
            let len = chunks.len() as isize; // eventually we should be using ffi::size_t

            // SAFETY: VST2 ABI: for `effGetChunk`, `ptr` is the host's `void**` out-param.
            // The data is then leaked below so the pointer stays valid; not null-checked.
            unsafe {
                *(ptr as *mut *mut c_void) = chunks.as_ptr() as *mut c_void;
            }

            mem::forget(chunks);
            return len;
        }
        Ok(OpCode::SetData) => {
            // SAFETY: VST2 ABI: for `effSetChunk`, `ptr` addresses `value` bytes owned by
            // the host for the call. Not checked for null or a negative `value`.
            let chunks = unsafe { slice::from_raw_parts(ptr as *mut u8, value as usize) };

            // `aeffect.h` documents no return value for `effSetChunk`, and a JUCE plugin
            // returns 0 after loading successfully, so this answers 0 as practice does:
            // a host that reads the 0 as a refusal is the one this plugin exists to catch.
            if index == 0 {
                params.load_bank_data(chunks);
            } else {
                params.load_preset_data(chunks);
            }
            return 0;
        }

        Ok(OpCode::ProcessEvents) => {
            // SAFETY: VST2 ABI: for `effProcessEvents`, `ptr` is a valid `VstEvents` for the
            // call. Not checked for null.
            get_plugin().process_events(unsafe { &*(ptr as *const api::Events) });
        }
        Ok(OpCode::CanBeAutomated) => return params.can_be_automated(index) as isize,
        Ok(OpCode::StringToParameter) => {
            return params.string_to_parameter(index, read_string(ptr)) as isize;
        }

        Ok(OpCode::GetPresetName) => {
            return copy_string(ptr, &params.get_preset_name(index), MAX_PRESET_NAME_LEN);
        }

        Ok(OpCode::GetInputInfo) => {
            if index >= 0 && index < get_plugin().get_info().inputs {
                // SAFETY: VST2 ABI: for `effGetInputProperties`, `ptr` is a host-owned
                // `VstPinProperties` to fill. Not checked for null.
                unsafe {
                    let ptr = ptr as *mut api::ChannelProperties;
                    *ptr = get_plugin().get_input_info(index).into();
                }
            }
        }
        Ok(OpCode::GetOutputInfo) => {
            if index >= 0 && index < get_plugin().get_info().outputs {
                // SAFETY: VST2 ABI: for `effGetOutputProperties`, `ptr` is a host-owned
                // `VstPinProperties` to fill. Not checked for null.
                unsafe {
                    let ptr = ptr as *mut api::ChannelProperties;
                    *ptr = get_plugin().get_output_info(index).into();
                }
            }
        }
        Ok(OpCode::GetCategory) => {
            return get_plugin().get_info().category.into();
        }

        Ok(OpCode::BeginSetPreset) => {
            get_plugin().begin_set_preset();
            return 1;
        }
        Ok(OpCode::EndSetPreset) => {
            get_plugin().end_set_preset();
            return 1;
        }

        // `value` is the flag: 1 = bypass, 0 = resume processing.
        Ok(OpCode::SoftBypass) => return isize::from(get_plugin().set_bypass(value != 0)),

        Ok(OpCode::GetEffectName) => {
            // Leave the buffer alone when the plugin declines. An unanswered
            // optional opcode is a no-write, which is how a host tells "no
            // effect name" from one — copying the product string in here would
            // make every plugin look like it implements the opcode.
            return match get_plugin().get_effect_name() {
                Some(name) => copy_string(ptr, &name, MAX_EFFECT_NAME_LEN),
                None => 0,
            };
        }

        Ok(OpCode::GetVendorName) => {
            return copy_string(ptr, &get_plugin().get_info().vendor, MAX_VENDOR_STR_LEN);
        }
        Ok(OpCode::GetProductName) => {
            return copy_string(ptr, &get_plugin().get_info().name, MAX_PRODUCT_STR_LEN);
        }
        Ok(OpCode::GetVendorVersion) => return get_plugin().get_info().version as isize,
        Ok(OpCode::VendorSpecific) => return get_plugin().vendor_specific(index, value, ptr, opt),
        Ok(OpCode::CanDo) => {
            let can_do = CanDo::from_str(&read_string(ptr));
            return get_plugin().can_do(can_do).into();
        }
        Ok(OpCode::GetTailSize) => {
            if get_plugin().get_tail_size() == 0 {
                return 1;
            } else {
                return get_plugin().get_tail_size();
            }
        }

        //OpCode::GetParamInfo => { /*TODO*/ }
        Ok(OpCode::GetApiVersion) => return 2400,

        Ok(OpCode::EditorKeyDown) => {
            if let Some(editor) = get_editor()
                && let Ok(key) = Key::try_from(value)
            {
                editor.key_down(KeyCode {
                    character: index as u8 as char,
                    key,
                    modifier: opt.to_bits() as u8,
                });
            }
        }
        Ok(OpCode::EditorKeyUp) => {
            if let Some(editor) = get_editor()
                && let Ok(key) = Key::try_from(value)
            {
                editor.key_up(KeyCode {
                    character: index as u8 as char,
                    key,
                    modifier: opt.to_bits() as u8,
                });
            }
        }
        Ok(OpCode::EditorSetKnobMode) => {
            if let Some(editor) = get_editor()
                && let Ok(knob_mode) = KnobMode::try_from(value)
            {
                editor.set_knob_mode(knob_mode);
            }
        }

        Ok(OpCode::StartProcess) => get_plugin().start_process(),
        Ok(OpCode::StopProcess) => get_plugin().stop_process(),
        // `value` is the width: 0 = 32-bit, anything else = 64-bit.
        Ok(OpCode::SetPrecision) => get_plugin().set_precision(value != 0),

        Ok(OpCode::GetNumMidiInputs) => {
            // SAFETY: as for `params` above: `user` is the live `PluginCache`.
            return unsafe { (*effect).get_info() }.midi_inputs as isize;
        }
        Ok(OpCode::GetNumMidiOutputs) => {
            // SAFETY: as for `params` above: `user` is the live `PluginCache`.
            return unsafe { (*effect).get_info() }.midi_outputs as isize;
        }

        _ => {
            debug!("Unimplemented opcode ({:?})", opcode);
            trace!(
                "Arguments; index: {}, value: {}, ptr: {:?}, opt: {}",
                index, value, ptr, opt
            );
        }
    }

    0
}

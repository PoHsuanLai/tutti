//! Plugin lifecycle: initialization, activation and the audio half's hand-off, and the teardown order `Drop` runs.

use super::*;

impl Plugin {
    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// `IHostApplication` upcast to `FUnknown`, **borrowed**: the returned
    /// pointer carries no reference for the caller to hand over or drop. It is
    /// valid only while `self.host.application` is alive, which is why this is
    /// private and its result never outlives the calling method.
    ///
    /// `IPluginBase::initialize` does **not** take ownership of the context.
    /// The plugin retains it itself if it keeps it: `ComponentBase::hostContext`
    /// is an `IPtr<FUnknown>` (`public.sdk/source/vst/vstcomponentbase.h:84`),
    /// so the bare `hostContext = context;` in `ComponentBase::initialize`
    /// (`vstcomponentbase.cpp:42`) runs `IPtr::operator=`, which addRefs — and
    /// the `hostContext = nullptr;` in `terminate` (same file, line 51) releases.
    /// `IPluginBase::terminate`'s own contract says as much:
    /// "You have to release all references to any host application interfaces"
    /// (`pluginterfaces/base/ipluginbase.h:48`).
    ///
    /// So the host must lend, not give. Passing an owned `+1` here (a
    /// `to_com_ptr().into_raw()`) leaks one reference per load, unbounded across
    /// load/unload cycles. The matching trap is the inverse: because the plugin
    /// never consumed a reference, a host that "balances" this with a
    /// `FUnknown::release` on any path frees a reference it does not own — a
    /// use-after-free, not a leak. Steinberg's own reference host borrows and
    /// releases nothing on either path
    /// (`public.sdk/source/vst/hosting/plugprovider.cpp:140,178`).
    pub(super) fn host_context(&self) -> Result<*mut FUnknown, Vst3Error> {
        Ok(self
            .application
            .as_com_ref::<IHostApplication>()
            .ok_or(Vst3Error::Other(0))?
            .upcast::<FUnknown>()
            .as_ptr())
    }

    /// `IPluginBase::initialize` on the component, with the host application.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn initialize_component(&mut self, main: &MainThread) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let host = self.host_context()?;
        // SAFETY: `host` is the live host application, which `self` keeps
        // alive; `initialize` borrows it.
        check(unsafe { self.component.initialize(host) })?;
        self.component_initialized = true;
        Ok(())
    }

    /// Reads `IProcessContextRequirements` into
    /// [`process_context_requirements`](Self::process_context_requirements):
    /// everything for a plugin without the interface. The SDK allows the query
    /// once the component is initialized.
    pub fn read_process_context_requirements(&mut self) {
        self.process_context_requirements =
            match self.processor.cast::<IProcessContextRequirements>() {
                // SAFETY: a plain getter on a live interface.
                Some(reqs) => unsafe { reqs.getProcessContextRequirements() },
                None => u32::MAX,
            };
    }

    /// Whether the controller is a separate class from the component.
    #[must_use]
    pub fn has_separate_controller(&self) -> bool {
        matches!(self.controller, ControllerLink::Separate(_))
    }

    /// `IPluginBase::initialize` on a separate controller (a same-object
    /// controller was initialized with the component). Nothing for a plugin
    /// without one.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn initialize_controller(&mut self, main: &MainThread) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let host = self.host_context()?;
        if let ControllerLink::Separate(ctrl) = &self.controller {
            // SAFETY: `host` is the live host application, which `self` keeps
            // alive.
            check(unsafe { ctrl.initialize(host) })?;
            self.controller_initialized = true;
        }
        Ok(())
    }

    /// Wires the component's connection point directly to the separate
    /// controller's. No-op unless both ends expose `IConnectionPoint`.
    ///
    /// The host deliberately does **not** interpose its own
    /// `IConnectionPoint` proxy between the two halves: it direct-wires the
    /// plugin's component and controller to each other, so their private
    /// messages pass straight through without the host inspecting or
    /// reformatting them.
    /// Returns whether both halves accepted the connection. A refusal is not an
    /// error — the plugin simply runs unconnected, as it does when either half
    /// declines to expose `IConnectionPoint` at all.
    ///
    /// The two `connect` calls succeed or fail together: a plugin that accepted
    /// the first and refused the second would be wired one way only, which
    /// surfaces much later as an editor that does not track the processor.
    #[must_use]
    pub fn connect_controller(&self, main: &MainThread) -> bool {
        let _scope = self.scoped(main);
        let ControllerLink::Separate(ctrl) = &self.controller else {
            return false;
        };
        let (Some(comp_conn), Some(ctrl_conn)) = (
            self.component.cast::<IConnectionPoint>(),
            ctrl.cast::<IConnectionPoint>(),
        ) else {
            return false;
        };
        // SAFETY: both are live connection points, each given the other as the
        // peer; `connect` does not take ownership.
        unsafe {
            if comp_conn.connect(ctrl_conn.as_ptr()) != kResultOk {
                return false;
            }
            if ctrl_conn.connect(comp_conn.as_ptr()) != kResultOk {
                comp_conn.disconnect(ctrl_conn.as_ptr());
                return false;
            }
        }
        true
    }

    pub(super) fn disconnect_controller(&self) {
        let ControllerLink::Separate(ctrl) = &self.controller else {
            return;
        };
        let (Some(comp_conn), Some(ctrl_conn)) = (
            self.component.cast::<IConnectionPoint>(),
            ctrl.cast::<IConnectionPoint>(),
        ) else {
            return;
        };
        // SAFETY: both are live connection points, each named as the peer it
        // was connected to.
        unsafe {
            comp_conn.disconnect(ctrl_conn.as_ptr());
            ctrl_conn.disconnect(comp_conn.as_ptr());
        }
    }

    /// Gives the controller the host's component handler.
    pub fn attach_component_handler(&self, main: &MainThread) {
        let _scope = self.scoped(main);
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let handler_ptr = self
            .handler
            .as_com_ref::<IComponentHandler>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        // SAFETY: `handler_ptr` is the host's handler, which `self` keeps alive
        // until `Drop` clears it again.
        unsafe {
            let _ = ctrl.setComponentHandler(handler_ptr);
        }
    }

    /// `IComponent::setActive`. Whether it is on is remembered, so that
    /// dropping the plugin deactivates it and an audio half is handed out only
    /// for an active plugin.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal, after which a failed activation leaves the plugin inactive.
    pub fn set_active(&mut self, main: &MainThread, active: bool) -> Result<(), Vst3Error> {
        let _scope = self.scoped(main);
        let mut state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live component, under the claim, with no
        // audio half out.
        let result = check(unsafe { self.component.setActive(u8::from(active)) });
        // Deactivating is recorded whatever the plugin answers: there is no
        // third state to recover to, and teardown must not retry it.
        if result.is_ok() || !active {
            state.active = active;
            // The layout `process` holds the bound buses to is read now, from
            // the plugin as it stands once active (it may settle its buses
            // only then), and forgotten on deactivation.
            state.buses = (active && result.is_ok()).then(|| Arrangement {
                inputs: self.audio_bus_channels(main, BusDirection::Input).into(),
                outputs: self.audio_bus_channels(main, BusDirection::Output).into(),
            });
        }
        result
    }

    /// Whether `set_active(true)` succeeded and `set_active(false)` has not.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.shared.lock().active
    }

    /// `IAudioProcessor::setProcessing`, from the main thread, at activation.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal.
    pub fn set_processing(&self, on: bool) -> Result<(), Vst3Error> {
        let _state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live processor, under the claim, with no
        // audio half out.
        check(unsafe { self.processor.setProcessing(u8::from(on)) })
    }

    /// The audio half of this plugin, once: `None` while an earlier one lives,
    /// and unless the plugin is set up ([`setup_processing`](Self::setup_processing))
    /// and active ([`set_active`](Self::set_active)).
    ///
    /// The half names no main-thread entry, and its `process` and
    /// `set_processing` run under the instance's claim, which `Drop` also takes,
    /// so a plugin that is torn down waits out a block in flight and the half
    /// then answers [`Vst3Error::NotInitialized`]. While it exists, the state
    /// changes of this type (`set_active`, `set_processing`, `setup_processing`,
    /// `set_bus_arrangements`, `activate_bus`) answer [`Vst3Error::Busy`].
    #[must_use]
    pub fn audio_side(&self) -> Option<AudioSide> {
        self.shared.take_audio_side()
    }

    /// What `IProcessContextRequirements` asked for; everything until
    /// [`read_process_context_requirements`](Self::read_process_context_requirements)
    /// runs and for a plugin without the interface.
    #[must_use]
    pub fn process_context_requirements(&self) -> u32 {
        self.process_context_requirements
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        // Retire first: the audio half refuses every later block, and this waits
        // out one in flight. The state is cleared under the lock, so an audio
        // thread that gets the lock later finds neither an active nor a set-up
        // plugin (and `retired`, checked again under the lock).
        // An editor open and no main thread under us: the view cannot be
        // detached here (a toolkit's `removed` on another thread is a crash),
        // and `setActive(0)`/`terminate` are main-thread calls too, as is
        // closing a module whose view is still alive. So the whole plugin is
        // leaked: every COM object gets one extra reference that is never
        // released, the module one extra owner, and no `terminate` runs. The
        // run loop is purged, so nothing of it is ever called. Memory and a
        // loaded library are the cost; the control side has no retirement path
        // for a `Plugin` to route this to.
        if self.editor.is_some() && MainThread::current().is_none() {
            self.purge_run_loop();
            eprintln!(
                "tutti-vst3-ffi: a plugin with an open editor was dropped off the main thread; \
                 leaking the plugin and its module rather than tear it down on this thread"
            );
            std::mem::forget(self.component.clone());
            std::mem::forget(self.processor.clone());
            if let Some(controller) = self.controller.get() {
                std::mem::forget(controller.clone());
            }
            std::mem::forget(self.application.clone());
            std::mem::forget(self.handler.clone());
            if let Some(editor) = &self.editor {
                std::mem::forget(editor.view.clone());
                std::mem::forget(editor.plug_frame.clone());
            }
            std::mem::forget(self.module.clone());
            return;
        }
        let shared = self.shared.clone();
        let (state, was_active) = shared.teardown();
        // A plugin dropped while activated (its halves were never rejoined)
        // stops and deactivates first: `terminate` is for an inactive component.
        if was_active {
            // SAFETY: plain calls on live interfaces; the claim is held, so the
            // audio half is not inside `process`.
            unsafe {
                self.processor.setProcessing(0);
                let _ = self.component.setActive(0);
            }
        }
        // The claim is not needed past this point: every audio-half entry
        // refuses on `retired`, which it checks under the lock, and the rest is
        // control-side teardown that must not hold it.
        drop(state);
        // Drop has no token and may run on any thread: the editor is detached
        // only if this is the main thread (`release_editor`).
        self.release_editor(MainThread::current().as_ref());
        // Registered with the run loop outside any editor (a plugin may do so
        // from `initialize`): gone with the plugin.
        self.purge_run_loop();
        // Mirror the load-time connect in reverse: for a separate controller,
        // tear the component↔controller connection down before terminating
        // either half. No-op for same-object / no controller.
        self.disconnect_controller();
        // Retract the handler before terminating. Unlike `initialize`, which
        // borrows the host context, `setComponentHandler` *retains* — so a
        // plugin that overrides `terminate` without chaining up to the base
        // class (which resets it) would hold the handler past its own teardown.
        // The order — retract the handler, terminate a separate controller, then
        // the component — is the SDK wrapper's (`basewrapper.cpp`, "Terminate
        // Controller Component" then "Terminate Processor Component"). The
        // hosting sample's `PlugProvider::terminatePlugin` terminates the
        // component first; both terminate a single-component plugin once.
        // `terminate` follows a successful `initialize` and nothing else: a
        // plugin that never initialized, or refused to, has nothing to undo and
        // some crash on it.
        if let Some(ctrl) = self.controller.get() {
            let initialized = match self.controller {
                ControllerLink::Separate(_) => self.controller_initialized,
                _ => self.component_initialized,
            };
            if initialized {
                // SAFETY: the controller is live; a null handler clears the one
                // the host attached.
                unsafe {
                    let _ = ctrl.setComponentHandler(std::ptr::null_mut());
                }
            }
            // A component that is its own controller is terminated once, below.
            if self.controller_initialized && matches!(self.controller, ControllerLink::Separate(_))
            {
                // SAFETY: a separate controller is a distinct live object,
                // terminated once, before the component.
                unsafe { ctrl.terminate() };
            }
        }
        if self.component_initialized {
            // SAFETY: the component is live and is terminated once, last.
            unsafe {
                self.component.terminate();
            }
        }
    }
}

/// The controller of a component: the component itself, a separate class it
/// names, or none.
pub(super) fn query_controller(module: &Module, component: &ComPtr<IComponent>) -> ControllerLink {
    if let Some(ctrl) = component.cast::<IEditController>() {
        return ControllerLink::Same(ctrl);
    }
    let mut controller_cid: TUID = [0; 16];
    // SAFETY: `controller_cid` is a valid out-parameter.
    let result = unsafe { component.getControllerClassId(&mut controller_cid) };
    if result == kResultOk && controller_cid != [0i8; 16] {
        match module.create_controller(&controller_cid) {
            Ok(ctrl) => ControllerLink::Separate(ctrl),
            Err(_) => ControllerLink::None,
        }
    } else {
        ControllerLink::None
    }
}

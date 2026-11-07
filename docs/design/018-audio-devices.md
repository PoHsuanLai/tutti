# 018: audio devices that come and go

A sound card that is unplugged, replugged or replaced as the system default
leaves the engine silent today, and what the host must do about it is not
written anywhere. This design gives the engine a model of which devices exist,
a policy for what to do when that changes, and the same behaviour headless and
in Bevy. It shares a pattern, `Phase::Poll` and the `SessionEvent` family with
the MIDI design (017) and no code: MIDI identity is an endpoint with function
blocks, audio identity is a name and a direction.

## 1. What is wrong today

Read in the code on `main`; nothing was run against a real card.

- The device list is enumerated once (`device_state_init_system`) and never
  refreshed.
- Nothing watches the default device. `DeviceSelector::Default` is resolved
  only inside `start` and `restart`. cpal 0.15.3 has `Device::name` and no id,
  and no change notification on any host, so polling is the only portable way.
- An output that vanishes sets `StreamFaults::disconnected`, and
  `SessionEvent::DeviceFault` is reported once. Then the callback stops, the
  transport and graph freeze, and nothing restarts, even though the device
  layer supports a restart after a disconnect.
- A startup failure is terminal (`AudioEngineState::Failed`): an app launched
  with no card never gets audio when one is plugged in.
- `DeviceSelector::Index` is the position in one enumeration. A hot-plug
  renumbers it, so a saved index can open a different card.
- `MicIn` is opened at the graph's rate by the host. The session never sees it
  and `Session::restart` cannot reopen it, so after a 44.1 to 48 kHz restart
  its old-rate frames feed the new-rate graph: pitch and speed shift, no
  error.

## 2. Four layers

1. **Pure core, `tutti-cpal::DeviceRoster`.** `apply(Snapshot) ->
   Vec<DeviceChange>` with no OS, thread or clock. It sits in `tutti-cpal`:
   `tutti-types` is vocabulary only, `tutti-core` has no device concept, and
   `tutti-session` (the policy) and `bevy-tutti` already reach the device
   through `tutti-cpal`. A new crate for one table would be a layer with one
   resident. The module imports nothing from cpal.
2. **OS layer, `tutti-cpal::DevicePoller`.** A thread owned by the poller
   enumerates the host on a cadence and publishes the latest `Snapshot`;
   `SnapshotSource::poll` takes it without blocking. Enumeration can block on
   ALSA and WASAPI, so it never runs on the audio thread or in `update`. The
   thread opens its own `DeviceHost`, waits on a stop channel with a timeout
   (no sleeps), and ends when the poller drops.
3. **Policy, `tutti-session`.** `DeviceWatch` owns a source, a roster and a
   `Follow`. `Follow::step(Input, Instant) -> Option<DeviceRestart>` is pure:
   the time is a value. The session runs the watch in `Phase::Poll`.
4. **Adapter, `bevy-tutti`.** Translates `SessionEvent`s into
   `AudioDeviceState` and messages, and calls the one restart function the
   session offers. No timers, no diffs.

## 3. API

```rust
// tutti-cpal
pub struct DeviceId { name: String, ordinal: usize }   // among same-named, same direction
pub enum DeviceSelector { Default, Id(DeviceId) }      // Index and Name are removed
pub enum Direction { Output, Input }
pub struct Snapshot { outputs, inputs: Vec<DeviceId>, default_output, default_input: Option<DeviceId> }
pub enum DeviceChange { Appeared(Direction, DeviceId), Vanished(Direction, DeviceId),
                        DefaultChanged(Direction, Option<DeviceId>) }
pub trait SnapshotSource { fn poll(&mut self) -> Option<Snapshot>; }

// tutti-session
pub struct DevicePolicy { follow_default, reconnect, retry_startup: bool, poll_interval, backoff: Backoff }
pub struct Follow;   // step(FollowEvent, &Snapshot, Instant) -> Option<DeviceRestart>, pure
pub struct DeviceWatch;   // source + roster + Follow; poll(now, &mut Vec<SessionEvent>)
Session::watch_devices(source, policy, want, running_on)
SessionEvent::{DeviceLost, DeviceAppeared, DefaultChanged, RestartWanted(DeviceRestart),
               DeviceRestarted(DeviceRestarted { previous_rate, rate }), RestartFailed(String)}

// bevy-tutti
TuttiPlugin { output_device: DeviceSelector, watch_devices: Option<DevicePolicy>, .. }
AudioDeviceEvent (message), AudioDeviceState::{output_devices, default_output}
```

## 4. Decisions

- **Identity is name, direction and ordinal.** cpal gives a name and nothing
  else, so two identical USB cards are told apart by their order among
  devices with that name. The ordinal is stable while the set of same-named
  devices is, which is the best an id-less API allows; it is documented as
  such. `Index` goes, `Name(s)` becomes `Id(DeviceId::new(s))`. The default
  device is matched by name, ordinal 0.
- **All automatic behaviour is opt-in and off by default.** `follow_default`
  moves a stream whose selector is `Default` when the default changes or the
  device it runs on vanishes. `reconnect` reopens the device the user chose
  explicitly when it returns. `retry_startup` retries a failed start. The
  session never replaces an explicit choice with another device: a lost
  explicit device waits, silent, for its own return or for the host.
- **Backoff holds against flapping.** Every attempt, successful or not, counts
  a strike; the delay before the next doubles from `base` to `max`; strikes
  clear after `settle` with no attempt. A device that flaps faster than
  `settle` is retried at `max`, not on every change.
- **A failed follow keeps the old spec.** It is the documented state of
  `Session::restart` (the stream stopped, the old spec and rate kept); the
  watch records a strike and tries again after the delay.
- **The session decides, the host executes.** `Device` is `!Send` and the
  host owns it, so `Poll` reports `RestartWanted(request)` and the host calls
  `Session::restart` with it. Every restart, wanted or the host's own, tells
  the watch the outcome and is reported by the next poll
  (`DeviceRestarted` or `RestartFailed`); a restart onto a different selector
  is the host choosing, and the policy then follows that choice. Bevy's
  system queues `restart_device` for the request.
- **The mic is the host's.** The session holds no `MicIn`, so it cannot reopen
  one. After a successful restart it reports `DeviceRestarted` with both
  rates; the host reopens its mic when `rate_changed()`. Mic stream faults
  are the input-faults lane's.
- **Startup retry is a watch with no session.** `DeviceWatch::without_stream`
  runs alone for an app that has no device yet; the adapter builds the engine
  when it says to, and `AudioEngineState::Failed` is no longer terminal when
  `retry_startup` is on. Systems that ran once at startup (`Startup`) have
  not seen that engine; only the engine-gated ones run once it is built.
- **A stream is lost only by a fault.** Absence from the list never ends it:
  ALSA opens every PCM to enumerate, so a device this process holds
  exclusively can drop out of the list while it plays, and `default` may never
  be listed. A list change is `Vanished`/`Appeared` (`DeviceLost` and
  `DeviceAppeared` events: "no longer listed", not "the stream died"). The
  stream's death is `DeviceFault` with `Disconnected`; the input-faults lane
  (#262) is making ALSA report that, and this design needs nothing more from
  it. Ordinals of same-named devices renumber when one goes; that changes
  events, not the stream's tracking.
- **Unknown is not different.** The device a stream runs on is `None` when the
  host cannot name it (a custom driver). `None` is never compared with the
  default, so a healthy stream is not restarted. The id recorded is the one
  opened: the selector's own id, or for `Default` the device's name at
  ordinal 0. cpal names the default and nothing else, so with two same-named
  cards the default is taken to be the first.
- **Bare ALSA never changes default.** The default's name is constant
  (`default`), so `DefaultChanged` cannot fire there and `follow_default`
  moves the stream only after a fault. Probing can also make a restart meet
  `EBUSY`: a counted strike, then the backoff. Opening every PCM each
  interval is a cost a host opts into; the interval is at least 250 ms.
- **A policy restart never makes a choice.** `Session::restart` (the host's)
  records the selector as what the user wants. A restart the policy asked for
  goes through `Session::restart_wanted`, which never changes it and is
  dropped when the host has chosen something else since (a stale request).
  `restart_on` (a custom driver) records no choice and leaves the running
  device unknown.
- **A host can decline.** Ignoring `RestartWanted` declines it (a strike is
  counted); `Session::hold_device_restarts(true)` (Bevy:
  `AudioGraphRes::hold_device_restarts`) stops the asking while a take is
  recorded, since a restart can change the rate mid-take. The session cannot
  see a host's recorder.
- **The adapter copies.** The session hands over its current list
  (`Session::watched_devices`) and the adapter copies it for display; it
  compares nothing and has no clock. The startup retry's decision is
  `DeviceWatch::retry_start`, in the session crate, and its poller moves into
  the session's watch when the engine is built.
- **The poller watches the default host** (`AudioHost::Default`); a JACK app
  polls through its own `SnapshotSource`.
- **What this does not claim.** Nothing here was exercised on real hardware,
  and the cost of enumeration on WASAPI, CoreAudio and JACK is unknown. The
  poll interval is therefore a policy value, 2 s by default.

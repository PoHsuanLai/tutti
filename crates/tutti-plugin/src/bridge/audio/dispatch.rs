//! Per-command IPC choreography for the bridge thread.

use super::channels::Channels;
use super::messages::{AudioResponse, BridgeEvent, Command, ResyncKind};
use super::payload_pool::PayloadPool;
use crate::error::Result;
use crate::protocol::{BridgeMessage, HostMessage, IpcMidiEvent, ProcessAudioFullData};
use crate::transport::control::{self as ipc, ControlStream};
use std::time::Duration;

const PROCESS_TIMEOUT: Duration = Duration::from_millis(500);
const PARAM_TIMEOUT: Duration = Duration::from_secs(5);
const STATE_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn handle(
    cmd: Command,
    stream: &mut ControlStream,
    channels: &Channels,
    payloads: &PayloadPool,
) -> Result<()> {
    match cmd {
        Command::Process(mut payload) => {
            let msg = HostMessage::ProcessAudioFull(Box::new(ProcessAudioFullData {
                buffer_id: payload.buffer_id,
                num_samples: payload.num_samples,
                midi_events: payload.midi_events.iter().map(IpcMidiEvent::from).collect(),
                param_changes: core::mem::take(&mut payload.param_changes),
                note_expression: core::mem::take(&mut payload.note_expression),
                chords: core::mem::take(&mut payload.chords),
                scales: core::mem::take(&mut payload.scales),
                expr_texts: core::mem::take(&mut payload.expr_texts),
                expr_ints: core::mem::take(&mut payload.expr_ints),
                transport: core::mem::take(&mut payload.transport),
            }));

            payloads.recycle(payload);

            ipc::send(stream, &msg)?;

            match recv_reply(stream, channels, PROCESS_TIMEOUT)? {
                BridgeMessage::AudioProcessedFull { .. }
                | BridgeMessage::AudioProcessedMidi { .. }
                | BridgeMessage::AudioProcessed { .. } => {
                    channels.push_audio_response(AudioResponse::AudioProcessed);
                }
                BridgeMessage::Error { .. } => {
                    channels.push_audio_response(AudioResponse::Error);
                }
                _ => {}
            }
        }
        Command::SetParameter { param_id, value } => {
            ipc::send(stream, &HostMessage::SetParameter { param_id, value })?;
        }
        Command::SetAutomationState { state } => {
            ipc::send(stream, &HostMessage::SetAutomationState { state })?;
        }
        Command::SetSampleRate { rate } => {
            ipc::send(stream, &HostMessage::SetSampleRate { rate })?;
        }
        Command::Reset => {
            ipc::send(stream, &HostMessage::Reset)?;
        }
        Command::Shutdown => {
            ipc::send(stream, &HostMessage::Shutdown)?;
        }
        Command::SaveState { reply } => {
            ipc::send(stream, &HostMessage::SaveState)?;
            let value = match recv_reply(stream, channels, STATE_TIMEOUT)? {
                BridgeMessage::StateData { data } => Some(data),
                _ => None,
            };
            reply.send(value);
        }
        Command::LoadState { data, reply } => {
            ipc::send(stream, &HostMessage::LoadState { data })?;
            reply.send(true);
        }
        Command::GetParameterList { reply } => {
            ipc::send(stream, &HostMessage::GetParameterList)?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterList { parameters } => Some(parameters),
                _ => None,
            };
            reply.send(value);
        }
        Command::GetParameter { param_id, reply } => {
            ipc::send(stream, &HostMessage::GetParameter { param_id })?;
            let value = match recv_reply(stream, channels, PARAM_TIMEOUT)? {
                BridgeMessage::ParameterValue { value } => value,
                _ => None,
            };
            reply.send(value);
        }
    }
    Ok(())
}

/// Drain unsolicited events, returning the next reply-type message.
fn recv_reply(
    stream: &mut ControlStream,
    channels: &Channels,
    timeout: Duration,
) -> Result<BridgeMessage> {
    loop {
        match ipc::recv_within(stream, timeout)? {
            BridgeMessage::LatencyChanged { samples } => {
                channels.push_unsolicited(BridgeEvent::LatencyChanged { samples });
            }
            BridgeMessage::ParameterChanged { index, value } => {
                channels.push_unsolicited(BridgeEvent::ParameterChanged { index, value });
            }
            BridgeMessage::PluginParamValuesChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::ParamValues));
            }
            BridgeMessage::PluginParamTitlesChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::ParamTitles));
            }
            BridgeMessage::PluginIoChanged => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::Io));
            }
            BridgeMessage::PluginReloaded => {
                channels.push_unsolicited(BridgeEvent::Resync(ResyncKind::Reloaded));
            }
            msg => return Ok(msg),
        }
    }
}

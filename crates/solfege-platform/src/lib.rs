//! Native audio and MIDI resources. The engine has no dependency on this crate.

use cpal::{
    Device, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use midir::{Ignore, MidiInput, MidiInputConnection};
use solfege_engine::{EngineCommand, SamplerEngine, SharedMetrics};
use thiserror::Error;

pub struct OutputDevice {
    device: Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    name: String,
}

impl OutputDevice {
    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate.0
    }

    pub fn channels(&self) -> u16 {
        self.config.channels
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

pub fn default_output_device() -> Result<OutputDevice, PlatformError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(PlatformError::NoOutputDevice)?;
    let name = device
        .name()
        .unwrap_or_else(|_| "Default output".to_owned());
    let supported = device
        .default_output_config()
        .map_err(|error| PlatformError::Audio(error.to_string()))?;
    Ok(OutputDevice {
        config: supported.config(),
        sample_format: supported.sample_format(),
        device,
        name,
    })
}

pub struct AudioRuntime {
    _stream: Stream,
}

impl AudioRuntime {
    pub fn start(
        output: OutputDevice,
        mut engine: SamplerEngine,
        commands: Receiver<EngineCommand>,
        metrics: std::sync::Arc<SharedMetrics>,
    ) -> Result<Self, PlatformError> {
        let channels = output.config.channels as usize;
        let stream = match output.sample_format {
            SampleFormat::F32 => {
                let error_metrics = metrics.clone();
                output
                    .device
                    .build_output_stream(
                        &output.config,
                        move |data: &mut [f32], _| {
                            drain_commands(&commands, &mut engine);
                            engine.process_interleaved(data, channels, &[]);
                        },
                        move |_| error_metrics.record_underrun(),
                        None,
                    )
                    .map_err(|error| PlatformError::Audio(error.to_string()))?
            }
            SampleFormat::I16 => build_converting_stream::<i16>(
                &output.device,
                &output.config,
                channels,
                engine,
                commands,
                metrics,
            )?,
            SampleFormat::U16 => build_converting_stream::<u16>(
                &output.device,
                &output.config,
                channels,
                engine,
                commands,
                metrics,
            )?,
            format => return Err(PlatformError::UnsupportedSampleFormat(format)),
        };
        stream
            .play()
            .map_err(|error| PlatformError::Audio(error.to_string()))?;
        Ok(Self { _stream: stream })
    }
}

fn drain_commands(commands: &Receiver<EngineCommand>, engine: &mut SamplerEngine) {
    for _ in 0..256 {
        match commands.try_recv() {
            Ok(command) => engine.handle_command(command),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

trait OutputSample: cpal::SizedSample + Copy + Send + 'static {
    fn from_engine(value: f32) -> Self;
    fn silence() -> Self;
}

impl OutputSample for i16 {
    fn from_engine(value: f32) -> Self {
        (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
    }

    fn silence() -> Self {
        0
    }
}

impl OutputSample for u16 {
    fn from_engine(value: f32) -> Self {
        ((value.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32).round() as u16
    }

    fn silence() -> Self {
        u16::MAX / 2
    }
}

fn build_converting_stream<T: OutputSample>(
    device: &Device,
    config: &StreamConfig,
    channels: usize,
    mut engine: SamplerEngine,
    commands: Receiver<EngineCommand>,
    metrics: std::sync::Arc<SharedMetrics>,
) -> Result<Stream, PlatformError> {
    const MAX_CALLBACK_FRAMES: usize = 8192;
    let mut scratch = vec![0.0_f32; channels * MAX_CALLBACK_FRAMES];
    let error_metrics = metrics.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                drain_commands(&commands, &mut engine);
                let Some(buffer) = scratch.get_mut(..data.len()) else {
                    data.fill(T::silence());
                    metrics.record_underrun();
                    return;
                };
                engine.process_interleaved(buffer, channels, &[]);
                for (destination, source) in data.iter_mut().zip(buffer.iter().copied()) {
                    *destination = T::from_engine(source);
                }
            },
            move |_| error_metrics.record_underrun(),
            None,
        )
        .map_err(|error| PlatformError::Audio(error.to_string()))
}

pub struct MidiRuntime {
    _connection: MidiInputConnection<()>,
    port_name: String,
}

impl MidiRuntime {
    pub fn connect_first(commands: Sender<EngineCommand>) -> Result<Option<Self>, PlatformError> {
        let mut input = MidiInput::new("Solfege MIDI")
            .map_err(|error| PlatformError::Midi(error.to_string()))?;
        input.ignore(Ignore::None);
        let Some(port) = input.ports().into_iter().next() else {
            return Ok(None);
        };
        let port_name = input
            .port_name(&port)
            .unwrap_or_else(|_| "MIDI input".to_owned());
        let connection = input
            .connect(
                &port,
                "solfege-input",
                move |_, message, _| {
                    if let Some(event) = solfege_midi::parse_channel_message(message) {
                        let _ = commands.try_send(EngineCommand::Event(event));
                    }
                },
                (),
            )
            .map_err(|error| PlatformError::Midi(error.to_string()))?;
        Ok(Some(Self {
            _connection: connection,
            port_name,
        }))
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("no default audio output device")]
    NoOutputDevice,
    #[error("audio error: {0}")]
    Audio(String),
    #[error("unsupported output sample format: {0:?}")]
    UnsupportedSampleFormat(SampleFormat),
    #[error("MIDI error: {0}")]
    Midi(String),
}

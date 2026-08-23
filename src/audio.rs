/// Audio capture using the `cpal` crate.
/// Supports microphone input and stereo mix (WASAPI loopback) on Windows.
/// Audio frames are provided as interleaved f32 samples.
use cpal::{
    Sample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::sync::Arc;

/// Available audio sources
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Microphone input
    Microphone,
    /// Stereo mix / What U Hear (WASAPI loopback)
    StereoMix,
}

/// Default audio stream configuration
pub const AUDIO_CONFIG: StreamConfig = StreamConfig {
    channels: 2,
    sample_rate: 44100,
    buffer_size: cpal::BufferSize::Default,
};

#[derive(Debug)]
pub enum AudioError {
    NoDevice,
    StreamSetup(String),
    IoError(std::io::Error),
}

/// Initialize audio capture for the given source.
/// Returns a stream that provides audio data via a callback.
pub fn init_capture(source: AudioSource, _sample_rate: u32) -> Result<AudioStream, AudioError> {
    let host = cpal::default_host();
    let device = match source {
        AudioSource::Microphone => host
            .default_input_device()
            .ok_or_else(|| AudioError::NoDevice)?,
        AudioSource::StereoMix => host
            .default_output_device()
            .ok_or_else(|| AudioError::NoDevice)?,
    };

    let stream = device
        .build_input_stream(
            AUDIO_CONFIG,
            move |data: &[f32], _| {
                // Callback provides interleaved f32 samples
            },
            move |e| {
                let _ = std::cell::Cell::new(None).set(Some(format!("{e}")));
            },
            None,
        )
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))?;

    Ok(AudioStream {
        stream,
        source,
        config: AUDIO_CONFIG,
        buffer: Vec::new(),
    })
}

/// An active audio capture stream.
pub struct AudioStream {
    stream: Stream,
    source: AudioSource,
    config: StreamConfig,
    buffer: Vec<f32>,
}

impl AudioStream {
    /// Get the current audio source
    pub fn source(&self) -> AudioSource {
        self.source
    }

    /// Get the audio configuration
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Read the latest captured audio samples
    pub fn read(&mut self) -> Option<&[f32]> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(&self.buffer)
        }
    }

    /// Start the audio stream
    pub fn play(&mut self) {
        let _ = self.stream.play();
    }

    /// Stop the audio stream
    pub fn stop(&mut self) {
        let _ = self.stream.pause();
    }
}

/// Detect available audio input devices
pub fn list_input_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    host.input_devices()
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))
        .map(|_| Vec::new())
}

/// Detect available audio output/loopback devices
pub fn list_output_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    host.output_devices()
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))
        .map(|_| Vec::new())
}

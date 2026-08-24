/// Audio capture using the `cpal` crate.
/// Supports microphone input and stereo mix (WASAPI loopback) on Windows.
/// Audio frames are provided as interleaved f32 samples.
use cpal::{
    Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::sync::{Arc, Mutex};

/// Available audio sources
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    /// Microphone input
    Microphone,
    /// Stereo mix / What U Hear (WASAPI loopback)
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    StreamSetup(String),
    #[allow(dead_code)]
    IoError(std::io::Error),
}

/// Initialize audio capture for the given source.
/// Returns a stream that provides audio data via a callback.
pub fn init_capture(source: AudioSource, _sample_rate: u32) -> Result<AudioStream, AudioError> {
    let host = cpal::default_host();
    let device = match source {
        AudioSource::Microphone => host.default_input_device().ok_or(AudioError::NoDevice)?,
        AudioSource::StereoMix => host.default_output_device().ok_or(AudioError::NoDevice)?,
    };

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let buf_clone = buffer.clone();

    let stream = device
        .build_input_stream(
            AUDIO_CONFIG,
            move |data: &[f32], _| {
                if let Ok(mut guard) = buf_clone.lock() {
                    guard.extend_from_slice(data);
                }
            },
            move |e| {
                eprintln!("[orr] audio stream error: {e}");
            },
            None,
        )
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))?;

    Ok(AudioStream {
        stream,
        source,
        config: AUDIO_CONFIG,
        buffer,
        local_buffer: Vec::new(),
    })
}

/// An active audio capture stream.
pub struct AudioStream {
    stream: Stream,
    #[allow(dead_code)]
    source: AudioSource,
    #[allow(dead_code)]
    config: StreamConfig,
    buffer: Arc<Mutex<Vec<f32>>>,
    local_buffer: Vec<f32>,
}

impl AudioStream {
    /// Get the current audio source
    #[allow(dead_code)]
    pub fn source(&self) -> AudioSource {
        self.source
    }

    /// Get the audio configuration
    #[allow(dead_code)]
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Read the latest captured audio samples
    pub fn read(&mut self) -> Option<&[f32]> {
        if let Ok(mut guard) = self.buffer.lock() {
            if guard.is_empty() {
                None
            } else {
                self.local_buffer = std::mem::take(&mut *guard);
                Some(&self.local_buffer)
            }
        } else {
            None
        }
    }

    /// Start the audio stream
    pub fn play(&mut self) {
        let _ = self.stream.play();
    }

    /// Stop the audio stream
    #[allow(dead_code)]
    pub fn stop(&mut self) {
        let _ = self.stream.pause();
    }
}

/// Detect available audio input devices
#[allow(dead_code)]
pub fn list_input_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    host.input_devices()
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))
        .map(|_| Vec::new())
}

/// Detect available audio output/loopback devices
#[allow(dead_code)]
pub fn list_output_devices() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    host.output_devices()
        .map_err(|e| AudioError::StreamSetup(format!("{e}")))
        .map(|_| Vec::new())
}

//! Pipeline seam for the pure-Rust rewrite (see docs/ARCHITECTURE_PURE_RUST.md).
//!
//! Stage contracts: a [`FrameSource`] yields BGRA [`Frame`]s, a [`VideoEncoder`]
//! turns frames into compressed [`Sample`]s, a [`Muxer`] writes them to a
//! container. No stage may spawn external processes (P4/P5 enforce this).

// P1 scaffold: stages consume these types from P2 (gdi source), P4 (sw+mux)
// and P6 (hw encoders) onward.
#![allow(dead_code)]

use anyhow::Result;
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// BGRA8, tightly packed, `width * height * 4` bytes.
    pub data: Vec<u8>,
    pub pts_ms: u64,
}

impl Frame {
    pub fn new(width: u32, height: u32, pts_ms: u64) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width * height * 4) as usize],
            pts_ms,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_ms: u64,
    pub dur_ms: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrackInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl From<FrameSpec> for TrackInfo {
    fn from(s: FrameSpec) -> Self {
        Self {
            width: s.width,
            height: s.height,
            fps: s.fps,
        }
    }
}

pub trait FrameSource {
    fn spec(&self) -> FrameSpec;
    fn start(&mut self) -> Result<()>;
    /// Next captured frame, or `None` at end of stream.
    fn next_frame(&mut self) -> Option<Frame>;
}

pub trait VideoEncoder {
    fn codec_name(&self) -> &'static str;
    fn init(&mut self, spec: &FrameSpec) -> Result<()>;
    fn feed(&mut self, frame: &Frame) -> Result<Vec<Sample>>;
    /// Drain encoder-internal buffers after the last frame.
    fn finish(&mut self) -> Result<Vec<Sample>>;
}

/// Muxer writes compressed samples to a container file.
pub trait Muxer {
    fn open(&mut self, track: &TrackInfo) -> Result<()>;
    fn write_sample(&mut self, sample: &Sample) -> Result<()>;
    fn finalize(&mut self) -> Result<PathBuf>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PipelineStats {
    pub frames_source: usize,
    pub frames_encoded: usize,
    pub samples_written: usize,
    pub duration_ms: u64,
}

/// Pump: source -> encoder -> muxer, with guaranteed flush order
/// (source drained, then encoder finished, then muxer finalized).
pub fn run(
    source: &mut dyn FrameSource,
    encoder: &mut dyn VideoEncoder,
    muxer: &mut dyn Muxer,
) -> Result<(PipelineStats, PathBuf)> {
    let mut stats = PipelineStats::default();
    let spec = source.spec();
    source.start()?;
    encoder.init(&spec)?;
    muxer.open(&TrackInfo::from(spec))?;

    while let Some(frame) = source.next_frame() {
        stats.frames_source += 1;
        stats.duration_ms = frame.pts_ms;
        for sample in encoder.feed(&frame)? {
            stats.samples_written += 1;
            muxer.write_sample(&sample)?;
        }
    }
    for sample in encoder.finish()? {
        stats.samples_written += 1;
        muxer.write_sample(&sample)?;
    }
    stats.frames_encoded = stats.frames_source;
    let out = muxer.finalize()?;
    Ok((stats, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::sw::SwH264Encoder;

    struct CountingSource {
        spec: FrameSpec,
        remaining: usize,
        emitted: usize,
        started: bool,
    }

    impl FrameSource for CountingSource {
        fn spec(&self) -> FrameSpec {
            self.spec
        }
        fn start(&mut self) -> Result<()> {
            self.started = true;
            Ok(())
        }
        fn next_frame(&mut self) -> Option<Frame> {
            if self.remaining == 0 {
                return None;
            }
            self.remaining -= 1;
            let pts = (self.emitted as u64 * 1000) / self.spec.fps as u64;
            self.emitted += 1;
            let mut f = Frame::new(self.spec.width, self.spec.height, pts);
            f.data[0] = (self.emitted % 256) as u8;
            Some(f)
        }
    }

    struct CountingEncoder {
        fed: usize,
        finished: bool,
        inited_with: Option<FrameSpec>,
    }

    impl VideoEncoder for CountingEncoder {
        fn codec_name(&self) -> &'static str {
            "counting"
        }
        fn init(&mut self, spec: &FrameSpec) -> Result<()> {
            self.inited_with = Some(*spec);
            Ok(())
        }
        fn feed(&mut self, frame: &Frame) -> Result<Vec<Sample>> {
            self.fed += 1;
            Ok(vec![Sample {
                data: vec![self.fed as u8],
                keyframe: self.fed == 1,
                pts_ms: frame.pts_ms,
                dur_ms: 33,
            }])
        }
        fn finish(&mut self) -> Result<Vec<Sample>> {
            self.finished = true;
            Ok(vec![Sample {
                data: vec![0xAA],
                keyframe: false,
                pts_ms: u64::MAX / 2,
                dur_ms: 0,
            }])
        }
    }

    struct MemoryMuxer {
        log: Vec<&'static str>,
        samples: Vec<Sample>,
        opened_with: Option<TrackInfo>,
        out: PathBuf,
    }

    impl Muxer for MemoryMuxer {
        fn open(&mut self, track: &TrackInfo) -> Result<()> {
            self.log.push("open");
            self.opened_with = Some(*track);
            Ok(())
        }
        fn write_sample(&mut self, sample: &Sample) -> Result<()> {
            self.log.push("sample");
            self.samples.push(sample.clone());
            Ok(())
        }
        fn finalize(&mut self) -> Result<PathBuf> {
            self.log.push("finalize");
            Ok(self.out.clone())
        }
    }

    #[test]
    fn pump_flows_frames_in_order_and_flushes() {
        let spec = FrameSpec {
            width: 4,
            height: 2,
            fps: 30,
        };
        let mut src = CountingSource {
            spec,
            remaining: 5,
            emitted: 0,
            started: false,
        };
        let mut enc = CountingEncoder {
            fed: 0,
            finished: false,
            inited_with: None,
        };
        let out_path =
            std::env::temp_dir().join(format!("orr_pipe_test_{}.mp4", std::process::id()));
        let mut mux = MemoryMuxer {
            log: Vec::new(),
            samples: Vec::new(),
            opened_with: None,
            out: out_path.clone(),
        };

        let (stats, written) = run(&mut src, &mut enc, &mut mux).expect("pipeline runs");

        assert!(src.started);
        assert_eq!(enc.inited_with, Some(spec));
        assert_eq!(enc.fed, 5);

        assert_eq!(stats.frames_source, 5);
        assert_eq!(stats.frames_encoded, 5);
        assert_eq!(stats.duration_ms, (4 * 1000) / 30); // last pts

        assert_eq!(mux.opened_with, Some(TrackInfo::from(spec)));
        assert_eq!(mux.samples.len(), 6); // 5 frames + 1 flushed tail
        assert!(mux.samples.first().unwrap().keyframe);
        assert_eq!(*mux.log.last().unwrap(), "finalize");
        assert_eq!(written, out_path);

        // strict ordering: open before all samples before finalize
        assert_eq!(mux.log[0], "open");
        assert_eq!(&mux.log[1..6], &["sample"; 5]);
    }
}

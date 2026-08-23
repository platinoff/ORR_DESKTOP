//! MP4 muxer (P4) built on the pure-Rust `muxide` crate. Fast-start
//! (moov before mdat) is enabled so recordings play immediately while being
//! written. H.264 samples arrive as Annex-B (OpenH264 output); muxide converts
//! to AVCC internally. No external process is spawned.

use crate::pipeline::{Muxer, Sample, TrackInfo};
use anyhow::{Context, Result};
use muxide::api::{MuxerBuilder, MuxerStats, VideoCodec};
use std::fs::File;
use std::path::PathBuf;

/// [`Muxer`] implementation writing an MP4 file.
pub struct Mp4Muxer {
    path: PathBuf,
    inner: Option<muxide::api::Muxer<File>>,
    /// Last PTS handed to muxide; guarantees strictly increasing timestamps
    /// even when a capture source emits two frames within the same
    /// millisecond tick (muxide rejects non-increasing PTS).
    next_pts_ms: u64,
}

impl Mp4Muxer {
    pub fn create(path: PathBuf) -> Self {
        Self {
            path,
            inner: None,
            next_pts_ms: 0,
        }
    }
}

impl Muxer for Mp4Muxer {
    fn open(&mut self, track: &TrackInfo) -> Result<()> {
        let file = File::create(&self.path)
            .with_context(|| format!("cannot create {}", self.path.display()))?;
        let muxer = MuxerBuilder::new(file)
            .video(
                VideoCodec::H264,
                track.width,
                track.height,
                f64::from(track.fps),
            )
            .build()
            .map_err(anyhow::Error::msg)
            .context("muxide mp4 builder failed")?;
        self.inner = Some(muxer);
        self.next_pts_ms = 0;
        Ok(())
    }

    fn write_sample(&mut self, sample: &Sample) -> Result<()> {
        let mux = self.inner.as_mut().context("muxer not opened")?;
        let pts_ms = sample.pts_ms.max(self.next_pts_ms);
        self.next_pts_ms = pts_ms + 1;
        mux.write_video(pts_ms as f64 / 1000.0, &sample.data, sample.keyframe)
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    fn finalize(&mut self) -> Result<PathBuf> {
        let mux = self.inner.take().context("muxer not opened")?;
        let stats: MuxerStats = mux.finish_with_stats().map_err(anyhow::Error::msg)?;
        println!(
            "[orr] muxed {} video frames, {:.3}s, {} bytes -> {}",
            stats.video_frames,
            stats.duration_secs,
            stats.bytes_written,
            self.path.display()
        );
        Ok(self.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::sw::SwH264Encoder;
    use crate::pipeline::{Frame, FrameSpec, VideoEncoder};

    /// Encode `n` synthetic gradient frames and return Annex-B samples.
    pub(crate) fn encode_samples(spec: FrameSpec, n: usize) -> Vec<Sample> {
        let mut enc = SwH264Encoder::new(crate::recorder::Quality::Low);
        enc.init(&spec).expect("encoder init");
        let mut out = Vec::new();
        for i in 0..n {
            let mut f = Frame::new(spec.width, spec.height, (i as u64 * 1000) / spec.fps as u64);
            for (px, chunk) in f.data.chunks_exact_mut(4).enumerate() {
                let x = (px % spec.width as usize) as u8;
                chunk.copy_from_slice(&[x.wrapping_mul(7), i as u8, x ^ 0x5A, 255]);
            }
            out.extend(enc.feed(&f).expect("feed"));
        }
        out.extend(enc.finish().expect("finish"));
        out
    }

    fn top_level_boxes(buf: &[u8]) -> Vec<(u32, [u8; 4], usize)> {
        // (size, type, offset) — walks the ISO-BMFF box chain from offset 0.
        let mut boxes = Vec::new();
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let size = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let ty = [buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]];
            if size == 0 {
                boxes.push((0, ty, off));
                break;
            }
            boxes.push((size, ty, off));
            off += size as usize;
        }
        boxes
    }

    #[test]
    fn mp4_has_ftyp_moov_before_mdat_and_exact_duration() {
        let spec = FrameSpec {
            width: 64,
            height: 48,
            fps: 30,
        };
        let n_frames = 12usize;
        let samples = encode_samples(spec, n_frames);

        let path = std::env::temp_dir().join(format!("orr_mux_test_{}.mp4", std::process::id()));
        let mut mux = Mp4Muxer::create(path.clone());
        mux.open(&TrackInfo::from(spec)).expect("open");
        for s in &samples {
            mux.write_sample(s).expect("write_sample");
        }
        let written = mux.finalize().expect("finalize");
        assert_eq!(written, path);

        let buf = std::fs::read(&path).expect("read back");
        assert!(
            buf.len() > 1024,
            "suspiciously tiny mp4: {} bytes",
            buf.len()
        );

        let boxes = top_level_boxes(&buf);
        let types: Vec<[u8; 4]> = boxes.iter().map(|b| b.1).collect();
        assert_eq!(types.first().copied(), Some(*b"ftyp"), "ftyp must be first");
        assert!(types.contains(b"moov"), "moov box missing");
        assert!(types.contains(b"mdat"), "mdat box missing");
        // fast-start equivalent: moov precedes mdat
        let moov_pos = types.iter().position(|t| t == b"moov").unwrap();
        let mdat_pos = types.iter().position(|t| t == b"mdat").unwrap();
        assert!(moov_pos < mdat_pos, "fast-start violated: moov after mdat");

        // mvhd duration: walk moov children for 'mvhd', read timescale+duration.
        let moov_off = boxes.iter().find(|b| b.1 == *b"moov").unwrap().2;
        let mvhd_type = moov_off
            + buf[moov_off..]
                .windows(4)
                .position(|w| w == b"mvhd")
                .expect("mvhd present");
        // Box header: size(4)+type(4); FullBox: version(1)+flags(3).
        let p = mvhd_type + 4 + 4; // first byte after version+flags -> creation_time
        let (timescale, duration): (u32, u64) = if buf[mvhd_type + 4] == 1 {
            (
                u32::from_be_bytes([buf[p + 16], buf[p + 17], buf[p + 18], buf[p + 19]]),
                u64::from_be_bytes([
                    buf[p + 20],
                    buf[p + 21],
                    buf[p + 22],
                    buf[p + 23],
                    buf[p + 24],
                    buf[p + 25],
                    buf[p + 26],
                    buf[p + 27],
                ]),
            )
        } else {
            (
                u32::from_be_bytes([buf[p + 8], buf[p + 9], buf[p + 10], buf[p + 11]]),
                u32::from_be_bytes([buf[p + 12], buf[p + 13], buf[p + 14], buf[p + 15]]) as u64,
            )
        };
        let dur_secs = duration as f64 / timescale as f64;
        let expected = n_frames as f64 / f64::from(spec.fps);
        assert!(
            (dur_secs - expected).abs() <= 1.0 / f64::from(spec.fps),
            "duration {dur_secs:.3}s != expected {expected:.3}s"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn duplicate_pts_are_bumped_not_rejected() {
        let spec = FrameSpec {
            width: 64,
            height: 48,
            fps: 30,
        };
        let samples = encode_samples(spec, 3);
        let path = std::env::temp_dir().join(format!("orr_mux_dup_{}.mp4", std::process::id()));
        let mut mux = Mp4Muxer::create(path.clone());
        mux.open(&TrackInfo::from(spec)).expect("open");
        // Force all three samples onto the same source PTS; the muxer must
        // internally space them instead of erroring on non-increasing PTS.
        for s in &samples {
            let mut s = s.clone();
            s.pts_ms = 33;
            mux.write_sample(&s).expect("duplicate pts accepted");
        }
        mux.finalize().expect("finalize");
        assert!(path.exists());
        std::fs::remove_file(&path).ok();
    }
}

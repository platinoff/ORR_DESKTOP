//! Software H.264 encoder (P4): OpenH264 compiled into the binary via the
//! `openh264` crate (`source` feature, bundled C). BGRA8 → I420 conversion is
//! done in-house (BT.601 limited range, fixed point); no external process is
//! ever spawned. See docs/ARCHITECTURE_PURE_RUST.md.

use crate::pipeline::{Frame, FrameSpec, Sample, VideoEncoder};
use crate::recorder::Quality;
use anyhow::{Result, bail};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};

/// An I420 (YUV 4:2:0, planar) frame that OpenH264 can encode directly.
pub struct I420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    /// Plane sizes for the given dimensions (width and height must be even).
    pub fn with_capacity(width: usize, height: usize) -> Self {
        debug_assert!(width.is_multiple_of(2) && height.is_multiple_of(2));
        Self {
            width,
            height,
            y: vec![0; width * height],
            u: vec![0; (width / 2) * (height / 2)],
            v: vec![0; (width / 2) * (height / 2)],
        }
    }

    fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        let cw = width / 2;
        let ch = height / 2;
        self.u.resize(cw * ch, 0);
        self.v.resize(cw * ch, 0);
    }
}

impl YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

/// Convert a packed BGRA8 buffer to planar I420 (BT.601, limited/studio range:
/// Y in 16..=235, Cb/Cr in 16..=240 centered on 128).
///
/// `out` is resized to the coded size, which is the input size rounded down to
/// even dimensions (chroma subsampling requires whole 2x2 blocks). Odd trailing
/// columns/rows are dropped.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, out: &mut I420Frame) {
    let w = (width as usize) & !1;
    let h = (height as usize) & !1;
    debug_assert!(bgra.len() >= width as usize * height as usize * 4);

    out.resize(w, h);
    let y = &mut out.y;
    let u = &mut out.u;
    let v = &mut out.v;

    // Per-pixel luma first (row by row, dropping odd tail column).
    // Pre-compute BGRA row pointer for efficiency.
    for row in 0..h {
        let src_row = &bgra[row * width as usize * 4..][..w * 4];
        let dst_row = &mut y[row * w..][..w];
        for (x, dst) in dst_row.iter_mut().enumerate() {
            let b = src_row[x * 4] as i32;
            let g = src_row[x * 4 + 1] as i32;
            let r = src_row[x * 4 + 2] as i32;
            *dst = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }

    // 2x2 chroma blocks: average the four source pixels in RGB space, then
    // convert once (cheaper than converting four times and averaging YUV).
    let cw = w / 2;
    let ch = h / 2;
    for cy in 0..ch {
        for cx in 0..cw {
            let mut rs = 0i32;
            let mut gs = 0i32;
            let mut bs = 0i32;
            for (dy, dx) in [(0usize, 0usize), (0, 1), (1, 0), (1, 1)] {
                let px = ((cy * 2 + dy) * width as usize + cx * 2 + dx) * 4;
                bs += bgra[px] as i32;
                gs += bgra[px + 1] as i32;
                rs += bgra[px + 2] as i32;
            }
            let r = rs / 4;
            let g = gs / 4;
            let b = bs / 4;
            let idx = cy * cw + cx;
            u[idx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            v[idx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// Quality preset → OpenH264 complexity (speed vs. compression effort).
fn complexity_for(q: Quality) -> Complexity {
    match q {
        Quality::Ultra => Complexity::High,
        Quality::High => Complexity::Medium,
        Quality::Medium | Quality::Low => Complexity::Low,
    }
}

/// Software H.264 [`VideoEncoder`] backed by OpenH264.
///
/// Screen-content tuned: bitrate rate-control at the quality preset's target,
/// periodic intra frames every ~10 s for seekable recordings, no B-frames
/// (PTS == DTS, zero encoder latency, nothing to drain in `finish`).
pub struct SwH264Encoder {
    bitrate_bps: u32,
    fps: u32,
    complexity: Complexity,
    enc: Option<Encoder>,
    i420: I420Frame,
    encoded: u64,
}

impl SwH264Encoder {
    pub fn new(quality: Quality) -> Self {
        Self {
            bitrate_bps: quality.bitrate_mbps() * 1_000_000,
            fps: 30,
            complexity: complexity_for(quality),
            enc: None,
            i420: I420Frame::with_capacity(2, 2),
            encoded: 0,
        }
    }

    fn encode_one(&mut self, frame: &Frame) -> Result<Sample> {
        bgra_to_i420(&frame.data, frame.width, frame.height, &mut self.i420);
        let enc = self.enc.as_mut().expect("init() called before feed()");
        let ts = Timestamp::from_millis(frame.pts_ms);
        let bitstream = enc.encode_at(&self.i420, ts)?;
        let keyframe = matches!(
            bitstream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        Ok(Sample {
            data: bitstream.to_vec(),
            keyframe,
            pts_ms: frame.pts_ms,
            dur_ms: u64::from(1000 / self.fps.max(1)),
        })
    }

    /// Pre-initialize encoder state for batch processing (reduces per-frame overhead).
    #[allow(dead_code)]
    fn pre_init(&mut self, spec: &FrameSpec) -> Result<()> {
        // Re-initialize with new spec if dimensions changed
        let w = spec.width as usize;
        let h = spec.height as usize;
        if self.enc.is_none() || self.i420.width != w || self.i420.height != h {
            self.init(spec)?;
        }
        Ok(())
    }
}

impl VideoEncoder for SwH264Encoder {
    fn codec_name(&self) -> &'static str {
        "openh264"
    }

    fn init(&mut self, spec: &FrameSpec) -> Result<()> {
        if !spec.width.is_multiple_of(2) || !spec.height.is_multiple_of(2) {
            bail!(
                "openh264 requires even frame dimensions, got {}x{} \
                 (area selections are rounded down to even by the native session)",
                spec.width,
                spec.height
            );
        }
        if spec.width > 3840 || spec.height > 2160 {
            bail!(
                "openh264 max resolution is 3840x2160, got {}x{}",
                spec.width,
                spec.height
            );
        }
        self.fps = spec.fps.max(1);
        let gop = 10 * self.fps;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(self.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(self.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(self.complexity)
            .intra_frame_period(IntraFramePeriod::from_num_frames(gop))
            .num_threads(if spec.width * spec.height >= 1920 * 1080 {
                0
            } else {
                1
            });
        self.enc = Some(Encoder::with_api_config(
            OpenH264API::from_source(),
            config,
        )?);
        self.i420 = I420Frame::with_capacity(spec.width as usize, spec.height as usize);
        self.encoded = 0;
        Ok(())
    }

    fn feed(&mut self, frame: &Frame) -> Result<Vec<Sample>> {
        let sample = self.encode_one(frame)?;
        self.encoded += 1;
        Ok(vec![sample])
    }

    fn finish(&mut self) -> Result<Vec<Sample>> {
        // No B-frames / reorder buffers: every frame was flushed by feed().
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&[b, g, r, 255]);
        }
        data
    }

    #[test]
    fn i420_gray_is_neutral_chroma() {
        let mut out = I420Frame::with_capacity(2, 2);
        bgra_to_i420(&solid_frame(2, 2, 128, 128, 128), 2, 2, &mut out);
        // R=G=B=v -> Y = 16 + 219*v/255 ; v=128 -> 126
        assert_eq!(out.y(), &[126, 126, 126, 126]);
        assert!(out.u().iter().all(|&c| c == 128));
        assert!(out.v().iter().all(|&c| c == 128));
    }

    #[test]
    fn i420_black_white_luma_extremes() {
        let mut out = I420Frame::with_capacity(2, 2);
        bgra_to_i420(&solid_frame(2, 2, 0, 0, 0), 2, 2, &mut out);
        assert!(out.y().iter().all(|&c| c == 16)); // studio-swing black

        bgra_to_i420(&solid_frame(2, 2, 255, 255, 255), 2, 2, &mut out);
        assert!(out.y().iter().all(|&c| c == 235)); // studio-swing white
    }

    #[test]
    fn i420_primary_colors_match_bt601_fixed_point() {
        let mut out = I420Frame::with_capacity(2, 2);

        // Pure red: Y=((66*255+128)>>8)+16=82, U=((-38*255+128)>>8)+128=90, V=((112*255+128)>>8)+128=240
        bgra_to_i420(&solid_frame(2, 2, 0, 0, 255), 2, 2, &mut out);
        assert_eq!(out.y(), &[82, 82, 82, 82]);
        assert_eq!(out.u(), &[90]);
        assert_eq!(out.v(), &[240]);

        // Pure blue: Y=((25*255+128)>>8)+16=41, U=((112*255+128)>>8)+128=240, V=(((-18)*255+128)>>8)+128=110
        bgra_to_i420(&solid_frame(2, 2, 255, 0, 0), 2, 2, &mut out);
        assert_eq!(out.y(), &[41, 41, 41, 41]);
        assert_eq!(out.u(), &[240]);
        assert_eq!(out.v(), &[110]);
    }

    #[test]
    fn i420_subsamples_two_by_two_average() {
        // Top-left pixel red, rest black -> 2x2 block avg r=64 (rounded down by /4 sums)
        let mut data = solid_frame(2, 2, 0, 0, 0);
        data[2] = 255; // pixel(0,0): B=0 G=0 R=255
        let mut out = I420Frame::with_capacity(2, 2);
        bgra_to_i420(&data, 2, 2, &mut out);
        // avg r = (255+0+0+0)/4 = 63 -> Y from per-pixel path unaffected; V from avg
        let expect_v: u8 = (((112 * 63i32 + 128) >> 8) + 128) as u8;
        assert_eq!(out.v()[0], expect_v);
    }

    #[test]
    fn i420_odd_dimensions_drop_tail_column_row() {
        // 3x3 input codes as 2x2 (odd tails dropped)
        let mut out = I420Frame::with_capacity(2, 2);
        bgra_to_i420(&solid_frame(3, 3, 10, 20, 30), 3, 3, &mut out);
        assert_eq!(out.dimensions(), (2, 2));
        assert_eq!(out.y().len(), 4);
        assert_eq!(out.u().len(), 1);
    }

    fn gradient_frames(spec: FrameSpec, n: u32) -> Vec<Frame> {
        (0..n)
            .map(|i| {
                let mut f =
                    Frame::new(spec.width, spec.height, (i as u64 * 1000) / spec.fps as u64);
                for (px, chunk) in f.data.chunks_exact_mut(4).enumerate() {
                    let x = (px % spec.width as usize) as u8;
                    chunk.copy_from_slice(&[x, i as u8, 255 - x, 255]);
                }
                f
            })
            .collect()
    }

    #[test]
    fn sw_encoder_produces_annexb_keyframe_first() {
        let spec = FrameSpec {
            width: 64,
            height: 48,
            fps: 30,
        };
        let mut enc = SwH264Encoder::new(Quality::Low);
        enc.init(&spec).expect("init");
        let frames = gradient_frames(spec, 5);
        let mut samples = Vec::new();
        for f in &frames {
            samples.extend(enc.feed(f).expect("feed"));
        }
        samples.extend(enc.finish().expect("finish"));

        assert_eq!(samples.len(), 5, "no B-frame drain expected");
        assert!(samples[0].keyframe, "first sample must be a keyframe");
        assert!(!samples[1].keyframe, "later samples should be delta frames");
        // Annex-B start code prefix on the first NAL.
        assert_eq!(&samples[0].data[..4], &[0, 0, 0, 1]);
        // SPS (type 7/0x67) or PPS (type 8/0x68) or IDR (type 5/0x65) header bytes present.
        assert!(samples.iter().all(|s| !s.data.is_empty()));
        assert_eq!(samples[0].pts_ms, 0);
        assert_eq!(samples[4].pts_ms, 133); // 4*1000/30
        assert_eq!(enc.codec_name(), "openh264");
    }

    #[test]
    fn sw_encoder_rejects_odd_dimensions() {
        let mut enc = SwH264Encoder::new(Quality::Low);
        let err = enc
            .init(&FrameSpec {
                width: 321,
                height: 240,
                fps: 30,
            })
            .expect_err("odd width must fail");
        assert!(err.to_string().contains("even"));
    }

    #[cfg(test)]
    mod bitrate_map {
        use super::*;

        #[test]
        fn presets_map_monotonically_to_bitrate_and_complexity() {
            let b = |q| SwH264Encoder::new(q).bitrate_bps;
            assert!(b(Quality::Ultra) > b(Quality::High));
            assert!(b(Quality::High) > b(Quality::Medium));
            assert!(b(Quality::Medium) > b(Quality::Low));
            assert_eq!(b(Quality::High), 50_000_000);
            assert!(matches!(
                SwH264Encoder::new(Quality::Ultra).complexity,
                Complexity::High
            ));
            assert!(matches!(
                SwH264Encoder::new(Quality::Low).complexity,
                Complexity::Low
            ));
        }
    }
}

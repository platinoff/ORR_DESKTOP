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
///
/// Large frames are converted in horizontal bands on the scoped thread pool
/// (`std::thread::scope`, no persistent workers, no dependencies); small frames
/// convert inline on the caller thread. Both paths share [`convert_band`] and
/// produce bit-identical output.
pub fn bgra_to_i420(bgra: &[u8], width: u32, height: u32, out: &mut I420Frame) {
    let stride = width as usize * 4;
    let w = (width as usize) & !1;
    let h = (height as usize) & !1;
    debug_assert!(bgra.len() >= stride * height as usize);

    out.resize(w, h);

    let threads = band_threads(w, h);
    if threads <= 1 {
        convert_band(bgra, stride, w, 0, h, &mut out.y, &mut out.u, &mut out.v);
        return;
    }

    // Even band height so every band owns whole 2x2 chroma blocks. Each band
    // gets a disjoint row range of every plane (`chunks_mut` guarantees the
    // splits line up with the ranges below).
    let band = (h.div_ceil(threads) + 1) & !1;
    let band = band.max(2);
    let cw = w / 2;

    let y_bands: Vec<&mut [u8]> = out.y.chunks_mut(w * band).collect();
    let u_bands: Vec<&mut [u8]> = out.u.chunks_mut(cw * (band / 2)).collect();
    let v_bands: Vec<&mut [u8]> = out.v.chunks_mut(cw * (band / 2)).collect();

    std::thread::scope(|scope| {
        for (i, (yb, (ub, vb))) in y_bands
            .into_iter()
            .zip(u_bands.into_iter().zip(v_bands))
            .enumerate()
        {
            let start = i * band;
            let end = (start + band).min(h);
            if start >= end {
                break;
            }
            scope.spawn(move || convert_band(bgra, stride, w, start, end, yb, ub, vb));
        }
    });
}

/// Number of bands to split a `w` x `h` conversion into (1 = inline, serial).
fn band_threads(w: usize, h: usize) -> usize {
    const PARALLEL_MIN_PIXELS: usize = 1280 * 720;
    if w * h < PARALLEL_MIN_PIXELS {
        return 1;
    }
    let hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // At least 2 rows per band; keep the fan-out modest for a per-frame fork.
    hw.min(8).min(h / 2).max(1)
}

/// Convert rows `[row_start..row_end)` (even-aligned) of a BGRA frame into the
/// given disjoint plane slices. `y` holds `(row_end - row_start)` rows of `w`
/// bytes; `u`/`v` hold `(row_end - row_start) / 2` rows of `w / 2` bytes.
#[allow(clippy::too_many_arguments)]
fn convert_band(
    bgra: &[u8],
    stride: usize,
    w: usize,
    row_start: usize,
    row_end: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    // Per-pixel luma (dropping odd tail column), bounds-check-free via
    // chunks_exact/zip so LLVM can vectorize the fixed-point math.
    for (row, dst_row) in y.chunks_exact_mut(w).enumerate() {
        let src_row = &bgra[(row_start + row) * stride..][..w * 4];
        for (px, dst) in src_row.chunks_exact(4).zip(dst_row.iter_mut()) {
            let b = px[0] as i32;
            let g = px[1] as i32;
            let r = px[2] as i32;
            *dst = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
    }

    // 2x2 chroma blocks processed as whole pixel pairs from two adjacent rows:
    // average the four source pixels in RGB space, then convert once (cheaper
    // than converting four times and averaging YUV).
    let cw = w / 2;
    let chroma_start = row_start / 2;
    let chroma_end = row_end / 2;
    for cy in chroma_start..chroma_end {
        let top = &bgra[(cy * 2) * stride..][..w * 4];
        let bottom = &bgra[(cy * 2 + 1) * stride..][..w * 4];
        let u_row = &mut u[(cy - chroma_start) * cw..][..cw];
        let v_row = &mut v[(cy - chroma_start) * cw..][..cw];
        for (pair, (du, dv)) in top
            .chunks_exact(8)
            .zip(bottom.chunks_exact(8))
            .zip(u_row.iter_mut().zip(v_row.iter_mut()))
        {
            let rs = pair.0[2] as i32 + pair.0[6] as i32 + pair.1[2] as i32 + pair.1[6] as i32;
            let gs = pair.0[1] as i32 + pair.0[5] as i32 + pair.1[1] as i32 + pair.1[5] as i32;
            let bs = pair.0[0] as i32 + pair.0[4] as i32 + pair.1[0] as i32 + pair.1[4] as i32;
            let r = rs / 4;
            let g = gs / 4;
            let b = bs / 4;
            *du = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            *dv = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
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

        // Slice-based multithreading: openh264-0.9.x pins SM_SINGLE_SLICE by
        // default, which blocks its internal thread pool. Setting a NAL size
        // constraint switches the codec to SM_SIZELIMITED_SLICE whose
        // THREAD_FULLY_FIRE_MODE partitions each frame across worker threads
        // (verified against the bundled source, encoder_ext.cpp:3751). Small
        // frames stay single-threaded — fan-out costs more than it saves.
        let pixels = spec.width * spec.height;
        let threads = if pixels >= 1280 * 720 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(2, 4)
        } else {
            1
        };

        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(self.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(self.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(self.complexity)
            .intra_frame_period(IntraFramePeriod::from_num_frames(gop))
            .num_threads(threads as u16);
        if threads > 1 {
            // ~64 KiB per slice NAL keeps several partitions in flight per
            // 1080p frame without fragmenting small regions excessively.
            config = config.max_slice_len(64 * 1024);
        }
        self.enc = Some(Encoder::with_api_config(
            OpenH264API::from_source(),
            config,
        )?);
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

    #[test]
    fn i420_parallel_bands_match_serial_bit_exact() {
        // Gradient with enough pixels to cross the parallel threshold when
        // forced; run the band path against a serial conversion of the same
        // input and require identical planes.
        let (w, h) = (1930usize, 1082usize);
        let mut bgra = vec![0u8; w * h * 4];
        for (px, chunk) in bgra.chunks_exact_mut(4).enumerate() {
            let x = px % w;
            let y = px / w;
            chunk.copy_from_slice(&[(x * 7) as u8, (y * 13) as u8, (x + y) as u8, 255]);
        }
        let cw = w / 2;
        let ch = h / 2;
        let bgra: &[u8] = &bgra;

        let convert = |threads: usize| -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let mut out = I420Frame::with_capacity(w, h);
            let stride = w * 4;
            let band = (h.div_ceil(threads) + 1) & !1;
            if threads > 1 {
                let yb: Vec<&mut [u8]> = out.y.chunks_mut(w * band.max(2)).collect();
                let ub: Vec<&mut [u8]> = out.u.chunks_mut(cw * (band.max(2) / 2)).collect();
                let vb: Vec<&mut [u8]> = out.v.chunks_mut(cw * (band.max(2) / 2)).collect();
                std::thread::scope(|scope| {
                    for (i, (yy, (uu, vv))) in
                        yb.into_iter().zip(ub.into_iter().zip(vb)).enumerate()
                    {
                        let start = i * band;
                        let end = (start + band).min(h);
                        scope.spawn(move || convert_band(bgra, stride, w, start, end, yy, uu, vv));
                    }
                });
            } else {
                convert_band(bgra, stride, w, 0, h, &mut out.y, &mut out.u, &mut out.v);
            }
            (out.y, out.u, out.v)
        };

        // 1 thread vs a many-band split that exercises ragged final bands.
        let serial = convert(1);
        let split = convert(7);
        assert_eq!(serial.0.len(), w * h);
        assert_eq!(serial.1.len(), cw * ch);
        assert_eq!(serial.0, split.0, "Y planes diverge");
        assert_eq!(serial.1, split.1, "U planes diverge");
        assert_eq!(serial.2, split.2, "V planes diverge");
    }

    /// Manual perf probe: `cargo test --release perf_1080p -- --ignored --nocapture`.
    #[test]
    #[ignore = "perf probe, not an assertion"]
    fn perf_1080p_conversion_throughput() {
        let (w, h) = (1920usize, 1080usize);
        let frame = solid_frame(w as u32, h as u32, 12, 34, 200);
        let mut out = I420Frame::with_capacity(w, h);

        // Warm up allocator/branch predictors.
        for _ in 0..5 {
            bgra_to_i420(&frame, w as u32, h as u32, &mut out);
        }
        const N: usize = 120;
        let t0 = std::time::Instant::now();
        for _ in 0..N {
            bgra_to_i420(&frame, w as u32, h as u32, &mut out);
        }
        let dt = t0.elapsed();
        println!(
            "1080p BGRA->I420: {:.3} ms/frame, ~{:.1} fps conversion headroom",
            dt.as_secs_f64() * 1000.0 / N as f64,
            N as f64 / dt.as_secs_f64()
        );
    }

    /// Manual perf probe for the encoder stage alone (no capture, no muxing):
    /// `cargo test --release perf_encode -- --ignored --nocapture`.
    #[test]
    #[ignore = "perf probe, not an assertion"]
    fn perf_encode_1080p_stage() {
        let spec = FrameSpec {
            width: 1920,
            height: 1080,
            fps: 30,
        };
        for (label, quality) in [("Low", Quality::Low), ("High", Quality::High)] {
            let mut enc = SwH264Encoder::new(quality);
            enc.init(&spec).expect("init");
            // Static-ish frames first (typical desktop), then full gradient
            // (worst-case motion) — both at exactly 30 fps worth of frames.
            for kind in 0..2 {
                let n = 60;
                let t0 = std::time::Instant::now();
                for i in 0..n {
                    let mut f = Frame::new(spec.width, spec.height, (i * 33) as u64);
                    match kind {
                        0 => {
                            // Mostly-static desktop: one moving block.
                            f.data[0..1000].copy_from_slice(&[255u8; 1000]);
                        }
                        _ => {
                            for (px, chunk) in f.data.chunks_exact_mut(4).enumerate() {
                                let x = (px % spec.width as usize) as u8;
                                chunk.copy_from_slice(&[x, i as u8, 255 - x, 255]);
                            }
                        }
                    }
                    let t1 = std::time::Instant::now();
                    enc.feed(&f).expect("feed");
                    if i == 0 && kind == 0 {
                        continue; // first frame includes SPS/PPS + IDR setup
                    }
                    let _ = t1;
                }
                let dt = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
                println!(
                    "encode[{label}] kind{kind}: {dt:.2} ms/frame (~{:.1} fps ceiling)",
                    1000.0 / dt
                );
            }
        }
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

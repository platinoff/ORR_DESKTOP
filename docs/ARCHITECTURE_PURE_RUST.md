# Architecture — 100% self-contained Rust pipeline (no side applications)

**Status:** shipped — the native pipeline (P1–P4) is live and is the default
record path (`src/pipeline.rs` + `native.rs`); the ffmpeg spawn remains only
behind `ORR_LEGACY=1` in `recorder.rs`.

**Hard requirement.** The application must not launch external processes at
runtime. The Phase 0 design spawns `ffmpeg.exe`; that is a scaffold, not the
product. Target state: capture → encode → mux as in-crate modules, one
binary, zero side applications.

## Current (Phase 0) vs target

```
Phase 0 (today)                       Target

tray/menu ─┐                          tray/menu ─┐
selector ──┤                          selector ──┤
settings ──┘                          settings ──┘
     │                                     │
recorder.rs                          pipeline.rs
  builds argv                          ├─ capture::wgcap / gdi   (BGRA frames + pts)
  std::process::Command                ├─ encode::Encoder trait
    "ffmpeg.exe" ──► EXTERNAL          │    ├─ hw: vendor API in-process
    (screen grab + h264 + mp4          │    └─ sw: in-process encoder crate
     all inside ffmpeg)                └─ mux::mp4 (in-crate muxer)
                                             │
                                          file.mp4
```

Everything ffmpeg does today (gdigrab screen grab, scale/crop filters, video
encoding, MP4 muxing) gets an owning module in this crate.

## Module layout

```
src/
  main.rs        tray + event loop (unchanged role)
  settings.rs    persisted settings (unchanged role)
  selector.rs    region overlay (unchanged role)
  recorder.rs    orchestrator: session start/stop; no Command spawn
  pipeline.rs    frame pump: Source -> Encoder -> Muxer, backpressure, stop flush
  capture/
    mod.rs       trait FrameSource
    wgcap.rs     Windows.Graphics.Capture (primary)
    gdi.rs       BitBlt fallback
  recorder.rs    legacy ffmpeg path (ORR_LEGACY=1): detect/probe/argv/stop
  encode/
    mod.rs       trait VideoEncoder + registry/probe (priority NVENC>QSV>AMF>SW)
    hw_*.rs      vendor paths via D3D11 interop (per family)
    sw.rs        software encoder (crate-backed, see table)
  mux/
    mod.rs       trait Muxer (mp4 first)
```

## Trait boundaries

```rust
trait FrameSource {
    fn resolution(&self) -> (u32, u32);
    fn fps(&self) -> u32;
    fn recv(&mut self) -> Option<Frame>; // BGRA + timestamp
}

trait VideoEncoder {
    fn codec(&self) -> CodecId;
    fn feed(&mut self, frame: &Frame) -> Result<(), EncodeError>;
    fn finish(&mut self) -> Result<Vec<u8>, EncodeError>; // flushed bitstream tail
}

trait Muxer {
    fn open(&mut self, track: TrackInfo) -> Result<(), MuxError>;
    fn write(&mut self, sample: Sample) -> Result<(), MuxError>;
    fn finalize(self) -> Result<std::path::PathBuf, MuxError>;
}
```

The existing runtime-probe philosophy survives intact: encoders are still
discovered and **validated with a real test encode** before selection; only
the probe target changes from "can ffmpeg argv run" to "does the in-process
encoder init+encode a test frame".

## Encoder strategy

| Option | Purity | Pros | Cons |
|--------|--------|------|------|
| `rav1e` (pure Rust AV1) | 100% Rust | No C at all, real pure-Rust story | AV1 SW encode is slow on low-power iGPUs; file size per CPU-second |
| `openh264` via bundled -sys | In-process C, no side app | Fast, tiny output, Cisco-quality baseline | Not "pure Rust" in the strict sense; still compiled into our binary |
| x264 / ffmpeg libs via -sys | In-process C | Best x264 maturity | Heaviest C surface; closest to "embedding ffmpeg" |
| Vendor HW APIs (NVENC/QSV/AMF) in-process | Vendor SDKs linked in | GPU speed, keeps current UX | Per-vendor FFI work; D3D11 frame plumbing |

Decision rule: **the ban is on launching other programs**, not on linking —
but the pure-Rust path (`rav1e`) is preferred where performance allows.
Recommended landing order: GDI/WGC capture + `openh264`-sys software path +
`mp4-muxer` (ship "no external apps" early), then add hardware paths, then
evaluate swapping SW to `rav1e` if AV1 perf is acceptable on target machines.

## Migration steps (maps to ROADMAP phases)

1. Introduce traits + `pipeline.rs`; keep ffmpeg spawn behind
   `capture/ffspawn.rs` + reuse its encode/mux as one opaque "legacy" node so
   behavior is byte-comparable during transition.
2. Land native capture (`gdi.rs` first — deterministic to verify against
   gdigrab crops; then `wgcap.rs`).
3. Land `sw.rs` + `mux/mp4.rs`; switch default pipeline off the legacy node;
   delete `ffspawn.rs` after clean-machine acceptance.
4. Add hardware encoder modules reusing the existing probe/priority logic.

## Risks

- WGC: DPI virtualization, multi-monitor negative origins, protected-content
  windows — mitigate with the same crop math tests used for `cli-area`.
- SW encode throughput at 1080p30 on weak CPUs — keep FPS/CPU presets honest;
  hardware paths are the primary answer, SW is the floor.
- MP4 finalization bugs (moov placement/duration) — golden-file tests from
  Phase 0 outputs serve as fixtures.

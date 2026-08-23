mod audio;
mod capture;
mod encode;
mod mux;
mod native;
mod pipeline;
mod recorder;
mod selector;
mod settings;

use anyhow::Result;
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use recorder::{Encoder, Mode, Quality, Rect};
use selector::Outcome;
use settings::Settings;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};

const FPS_CHOICES: [u32; 3] = [24, 30, 60];

#[derive(Debug)]
enum UserEvent {
    Selector(Outcome),
    Tick,
    Finished(Result<PathBuf, String>),
    Menu(String),
}

/// One active recording through the native in-process pipeline
/// (WGC/GDI capture -> OpenH264 -> MP4 muxing, zero child processes).
struct RunningSession {
    handle: native::NativeSessionHandle,
    started: Instant,
    path: PathBuf,
}

struct Menus {
    _menu: Menu,
    record_full: MenuItem,
    record_area: MenuItem,
    stop: MenuItem,
    pause: MenuItem,
    resume: MenuItem,
    quality: Vec<CheckMenuItem>,
    fps: Vec<CheckMenuItem>,
    encoder: Vec<CheckMenuItem>,
    perf: CheckMenuItem,
    mouse: CheckMenuItem,
    choose_dir: MenuItem,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    cfg_path: PathBuf,
    settings: Settings,
    tray: Option<TrayIcon>,
    menus: Option<Menus>,
    session: Option<RunningSession>,
    selecting: bool,
    exiting: bool,
    exit_pending: bool,
    paused: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("orr-desktop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h") | Some("--help") => {
            println!(
                "ORR Desktop Recorder - system-tray screen recorder (native in-process pipeline)"
            );
            println!();
            println!("Usage:");
            println!("  orr_desktop                     run the tray application");
            println!(
                "  orr_desktop probe               show detected ffmpeg encoders (legacy info)"
            );
            println!("  orr_desktop cli-rec [s] [out]   record fullscreen for s seconds");
            println!("  orr_desktop cli-area X Y W H [s] [out]");
            println!("                                  record a fixed region");
            println!("  orr_desktop --version           print version");
            println!();
            println!("Recording uses the native in-process pipeline (WGC/GDI capture + OpenH264 +");
            println!("MP4 muxing) - no ffmpeg required. `probe` still reports legacy ffmpeg");
            println!("encoders when ORR_FFMPEG is set (diagnostics only).");
            Ok(())
        }
        Some("probe") => {
            let ffmpeg = std::env::var("ORR_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
            let caps = recorder::detect(&ffmpeg);
            println!("ffmpeg: {ffmpeg}");
            println!("ddagrab filter: {}", caps.ddagrab);
            println!("encoders:");
            for e in &caps.encoders {
                println!("  {}", e.label());
            }
            Ok(())
        }
        Some("cli-rec") => cli_record(&args),
        Some("cli-area") => cli_area(&args),
        _ => gui(),
    }
}

fn gui() -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
        let _ = menu_proxy.send_event(UserEvent::Menu(e.id.0));
    }));

    let cfg_path = Settings::config_path();
    let app_settings = Settings::load(&cfg_path).sanitized();

    let mut app = App {
        proxy,
        cfg_path,
        settings: app_settings,
        tray: None,
        menus: None,
        session: None,
        selecting: false,
        exiting: false,
        exit_pending: false,
        paused: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl App {
    fn busy(&self) -> bool {
        self.session.is_some() || self.selecting
    }

    fn fatal(&mut self, msg: &str) {
        eprintln!("[orr] {msg}");
        self.set_tooltip(&format!("ORR error: {msg}"));
    }

    fn set_tooltip(&self, text: &str) {
        if let Some(t) = &self.tray {
            let _ = t.set_tooltip(Some(text));
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.cfg_path) {
            self.fatal(&format!("cannot save settings: {e}"));
        }
    }

    fn sync_menu(&self) {
        if let Some(m) = &self.menus {
            let recording = self.session.is_some();
            let idle = !self.busy();
            m.record_full.set_enabled(idle);
            m.record_area.set_enabled(idle);
            m.stop.set_enabled(recording);
            for (i, q) in m.quality.iter().enumerate() {
                q.set_checked(self.settings.quality == Quality::ALL[i]);
            }
            for (i, f) in m.fps.iter().enumerate() {
                f.set_checked(self.settings.fps == FPS_CHOICES[i]);
            }
            for (i, e) in m.encoder.iter().enumerate() {
                e.set_checked(self.settings.encoder == Encoder::ALL[i]);
            }
            m.perf.set_checked(self.settings.performance_mode);
            m.mouse.set_checked(self.settings.capture_mouse);
        }
    }

    fn start_full(&mut self) {
        if self.busy() {
            return;
        }
        self.begin(Mode::FullScreen);
    }

    fn start_area(&mut self) {
        if self.busy() {
            return;
        }
        self.selecting = true;
        self.sync_menu();
        self.set_tooltip("Drag a rectangle to select area (Esc cancels)");
        let proxy = self.proxy.clone();
        selector::select_region(move |out| {
            let _ = proxy.send_event(UserEvent::Selector(out));
        });
    }

    fn begin(&mut self, mode: Mode) {
        if self.session.is_some() {
            return;
        }
        if let Err(e) = recorder::validate_output_dir(&self.settings.output_dir) {
            self.fatal(&format!("{e}"));
            return;
        }
        let path = recorder::output_file(&self.settings.output_dir);
        // Native in-process pipeline (P5: zero external executables).
        self.begin_native(mode, path);
    }

    fn begin_native(&mut self, mode: Mode, path: PathBuf) {
        let rect = match mode {
            Mode::FullScreen => match native::full_desktop_rect() {
                Ok(r) => r,
                Err(e) => {
                    self.fatal(&format!("cannot size desktop region: {e:#}"));
                    return;
                }
            },
            Mode::Area(r) => r,
        };
        let params = native::SessionParams {
            rect,
            fps: self.settings.fps,
            cursor: self.settings.capture_mouse,
            quality: self.settings.quality,
        };
        let proxy = self.proxy.clone();
        let audio_source = audio::AudioSource::Microphone;
        match native::spawn_session(
            params,
            path.clone(),
            move |res| {
                let _ = proxy.send_event(UserEvent::Finished(res));
            },
            audio_source,
        ) {
            Ok(handle) => {
                self.set_tooltip("ORR starting...");
                self.session = Some(RunningSession {
                    handle,
                    started: Instant::now(),
                    path,
                });
                self.sync_menu();
            }
            Err(e) => self.fatal(&format!("cannot start native recording: {e:#}")),
        }
    }

    fn stop(&mut self) {
        if let Some(RunningSession { handle, .. }) = &mut self.session {
            handle.stop();
            self.set_tooltip("ORR finalizing mp4...");
        }
    }

    fn open_folder(&self) {
        let dir = &self.settings.output_dir;
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }

    fn elapsed_string(&self) -> Option<String> {
        self.session.as_ref().map(|s| {
            let el = s.started.elapsed().as_secs();
            format!(
                "ORR REC {:02}:{:02}:{:02}",
                el / 3600,
                (el % 3600) / 60,
                el % 60
            )
        })
    }

    fn handle_menu_id(&mut self, id: &str) {
        match id {
            "rec_full" => self.start_full(),
            "rec_area" => self.start_area(),
            "rec_stop" => self.stop(),
            "pause" => {
                if let Some(s) = &self.session {
                    s.handle.pause();
                    self.paused = true;
                    self.sync_menu();
                }
            }
            "resume" => {
                if let Some(s) = &self.session {
                    s.handle.resume();
                    self.paused = false;
                    self.sync_menu();
                }
            }
            "open_dir" => self.open_folder(),
            "quit" => {
                self.exiting = true;
                if self.session.is_some() {
                    self.stop();
                } else {
                    self.exit_pending = true;
                }
            }
            "perf" => {
                self.settings.performance_mode = !self.settings.performance_mode;
                self.save_settings();
            }
            "reset" => {
                self.settings = Settings::default();
                self.save_settings();
                self.sync_menu();
            }
            "mouse" => {
                self.settings.capture_mouse = !self.settings.capture_mouse;
                self.save_settings();
                self.sync_menu();
            }
            "choose_dir" => {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.settings.output_dir = folder;
                    self.save_settings();
                    self.set_tooltip(&format!("Output folder: {}", self.settings.output_dir.display()));
                }
            }
            other => {
                let mut changed = false;
                if let Some(i) = index_of(other, "q_") {
                    if let Some(q) = Quality::ALL.get(i) {
                        self.settings.quality = *q;
                        changed = true;
                    }
                } else if let Some(i) = index_of(other, "f_") {
                    if let Some(f) = FPS_CHOICES.get(i) {
                        self.settings.fps = *f;
                        changed = true;
                    }
                } else if let Some(i) = index_of(other, "e_")
                    && let Some(e) = Encoder::ALL.get(i)
                {
                    self.settings.encoder = *e;
                    changed = true;
                }
                if changed {
                    self.save_settings();
                    self.sync_menu();
                }
            }
        }
    }
}

fn index_of(id: &str, prefix: &str) -> Option<usize> {
    id.strip_prefix(prefix)?.parse::<usize>().ok()
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_none() {
            match build_tray(&self.settings) {
                Ok((tray, menus)) => {
                    self.tray = Some(tray);
                    self.menus = Some(menus);
                    self.sync_menu();
                    self.set_tooltip("ORR Desktop Recorder");
                }
                Err(e) => self.fatal(&format!("tray init failed: {e}")),
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(id) => self.handle_menu_id(id.as_str()),
            UserEvent::Selector(out) => {
                self.selecting = false;
                match out {
                    Outcome::Selected(rect) => {
                        self.begin(Mode::Area(rect));
                        return;
                    }
                    Outcome::Cancelled(reason) => {
                        eprintln!("[orr] selection cancelled: {reason}");
                    }
                }
                self.sync_menu();
                if !self.busy() {
                    self.set_tooltip("ORR Desktop Recorder");
                }
            }
            UserEvent::Tick => {
                if let Some(t) = self.elapsed_string() {
                    self.set_tooltip(&t);
                }
            }
            UserEvent::Finished(res) => {
                let was_exiting = self.exiting;
                let err = res.err();
                if let Some(s) = self.session.take() {
                    match &err {
                        None => println!("[orr] saved {}", s.path.display()),
                        Some(e) => eprintln!("[orr] {e}"),
                    }
                }
                self.sync_menu();
                match (&err, was_exiting) {
                    (None, true) => self.exit_pending = true,
                    (Some(e), _) => self.fatal(e),
                    (None, false) => self.set_tooltip("ORR Desktop Recorder"),
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit_pending {
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        _: &ActiveEventLoop,
        _: winit::window::WindowId,
        _: winit::event::WindowEvent,
    ) {
    }
}

fn build_tray(settings: &Settings) -> Result<(TrayIcon, Menus)> {
    let menu = Menu::new();

    let record_full = MenuItem::with_id(
        "rec_full",
        "Record Full Screen",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let record_area = MenuItem::with_id(
        "rec_area",
        "Select Area && Record...",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let stop = MenuItem::with_id(
        "rec_stop",
        "Stop Recording",
        false,
        None::<muda::accelerator::Accelerator>,
    );
    let pause = MenuItem::with_id(
        "pause",
        "Pause Recording",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let resume = MenuItem::with_id(
        "resume",
        "Resume Recording",
        false,
        None::<muda::accelerator::Accelerator>,
    );
    let open_dir = MenuItem::with_id(
        "open_dir",
        "Open Recordings Folder",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let quit = MenuItem::with_id("quit", "Quit", true, None::<muda::accelerator::Accelerator>);

    let quality_sub = Submenu::with_id("quality_sub", "Quality", true);
    let quality: Vec<CheckMenuItem> = Quality::ALL
        .iter()
        .map(|q| {
            CheckMenuItem::with_id(
                format!("q_{}", *q as usize),
                q.label(),
                true,
                false,
                None::<muda::accelerator::Accelerator>,
            )
        })
        .collect();
    for q in &quality {
        quality_sub.append(q)?;
    }

    let fps_sub = Submenu::with_id("fps_sub", "Frame Rate", true);
    let fps_items: Vec<CheckMenuItem> = FPS_CHOICES
        .iter()
        .enumerate()
        .map(|(i, f)| {
            CheckMenuItem::with_id(
                format!("f_{i}"),
                format!("{f} FPS"),
                true,
                false,
                None::<muda::accelerator::Accelerator>,
            )
        })
        .collect();
    for f in &fps_items {
        fps_sub.append(f)?;
    }

    let enc_sub = Submenu::with_id("enc_sub", "Encoder", true);
    let encoders: Vec<CheckMenuItem> = Encoder::ALL
        .iter()
        .map(|e| {
            CheckMenuItem::with_id(
                format!("e_{}", *e as usize),
                e.label(),
                true,
                false,
                None::<muda::accelerator::Accelerator>,
            )
        })
        .collect();
    for e in &encoders {
        enc_sub.append(e)?;
    }

    let perf = CheckMenuItem::with_id(
        "perf",
        "Performance Mode (max GPU)",
        true,
        settings.performance_mode,
        None::<muda::accelerator::Accelerator>,
    );
    let mouse = CheckMenuItem::with_id(
        "mouse",
        "Capture Mouse Cursor",
        true,
        settings.capture_mouse,
        None::<muda::accelerator::Accelerator>,
    );
    let choose_dir = MenuItem::with_id(
        "choose_dir",
        "Choose Output Folder...",
        true,
        None::<muda::accelerator::Accelerator>,
    );
    let reset = MenuItem::with_id(
        "reset",
        "Reset Settings",
        true,
        None::<muda::accelerator::Accelerator>,
    );

    let settings_sub = Submenu::with_id("settings_sub", "Settings", true);
    settings_sub.append_items(&[&quality_sub, &fps_sub, &enc_sub, &perf, &mouse, &choose_dir, &reset])?;

    menu.append_items(&[
        &record_full,
        &record_area,
        &stop,
        &pause,
        &resume,
        &PredefinedMenuItem::separator(),
        &settings_sub,
        &open_dir,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;

    let menus = Menus {
        _menu: menu.clone(),
        record_full,
        record_area,
        stop,
        pause,
        resume,
        quality,
        fps: fps_items,
        encoder: encoders,
        perf,
        mouse,
        choose_dir,
    };

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ORR Desktop Recorder")
        .with_icon(app_icon()?)
        .build()?;

    Ok((tray, menus))
}

fn app_icon() -> Result<Icon> {
    const S: u32 = 64;
    let mut rgba = vec![0u8; (S * S * 4) as usize];
    let c = S as f32 / 2.0;
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - c + 0.5;
            let dy = y as f32 - c + 0.5;
            let d2 = dx * dx + dy * dy;
            let i = ((y * S + x) * 4) as usize;
            if d2 <= 625.0 {
                rgba[i] = 232;
                rgba[i + 1] = 64;
                rgba[i + 2] = 56;
                rgba[i + 3] = 255;
            } else if d2 <= 784.0 {
                rgba[i] = 255;
                rgba[i + 1] = 255;
                rgba[i + 2] = 255;
                rgba[i + 3] = 255;
            }
        }
    }
    Ok(Icon::from_rgba(rgba, S, S)?)
}

fn cli_native(seconds: u64, rect: Rect, out: PathBuf) -> Result<()> {
    let st = Settings::load(&Settings::config_path()).sanitized();
    recorder::validate_output_dir(out.parent().unwrap_or(std::path::Path::new(".")))?;
    let params = native::SessionParams {
        rect,
        fps: st.fps,
        cursor: st.capture_mouse,
        quality: st.quality,
    };
    // Watchdog ends the stream after `seconds`; the pipeline still finalizes.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(seconds));
        stop2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let (stats, path) = native::run_blocking(
        &params,
        out,
        Arc::clone(&stop),
        Arc::new(AtomicBool::new(false)),
        None,
        audio::AudioSource::Microphone,
    )?;
    println!(
        "[cli] done: {} frames / {} encoded, {} bytes -> {}",
        stats.frames_source,
        stats.frames_encoded,
        std::fs::metadata(&path)?.len(),
        path.display()
    );
    Ok(())
}

fn cli_area(args: &[String]) -> Result<()> {
    let nums: Vec<i64> = args[2..].iter().filter_map(|s| s.parse().ok()).collect();
    if nums.len() < 5 {
        anyhow::bail!("usage: orr_desktop cli-area X Y W H [seconds] [out.mp4]");
    }
    let rect = Rect {
        x: nums[0] as i32,
        y: nums[1] as i32,
        w: nums[2] as u32,
        h: nums[3] as u32,
    };
    let seconds = nums.get(4).copied().unwrap_or(3) as u64;
    let out = match args.get(7) {
        Some(p) => PathBuf::from(p),
        None => recorder::output_file(&std::env::current_dir()?),
    };

    println!("[cli] area {rect:?} for {seconds}s via native in-process pipeline");
    cli_native(seconds, rect, out)
}

fn cli_record(args: &[String]) -> Result<()> {
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = match args.get(3) {
        Some(p) => PathBuf::from(p),
        None => recorder::output_file(&std::env::current_dir()?),
    };

    println!("[cli] recording {seconds}s fullscreen via native in-process pipeline");
    let desktop = native::full_desktop_rect()?;
    cli_native(seconds, desktop, out)
}

mod recorder;
mod selector;
mod settings;

use anyhow::Result;
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use recorder::{Capabilities, Encoder, Mode, Quality, Rect};
use selector::Outcome;
use settings::Settings;
use std::path::PathBuf;
use std::process::ChildStdin;
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

struct Session {
    stdin: ChildStdin,
    started: Instant,
    path: PathBuf,
}

struct Menus {
    _menu: Menu,
    record_full: MenuItem,
    record_area: MenuItem,
    stop: MenuItem,
    quality: Vec<CheckMenuItem>,
    fps: Vec<CheckMenuItem>,
    encoder: Vec<CheckMenuItem>,
    perf: CheckMenuItem,
    mouse: CheckMenuItem,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    ffmpeg: String,
    caps: Capabilities,
    cfg_path: PathBuf,
    settings: Settings,
    tray: Option<TrayIcon>,
    menus: Option<Menus>,
    session: Option<Session>,
    selecting: bool,
    exiting: bool,
    exit_pending: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("orr-desktop {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h") | Some("--help") => {
            println!("ORR Desktop Recorder - system-tray screen recorder (ffmpeg)");
            println!();
            println!("Usage:");
            println!("  orr_desktop                     run the tray application");
            println!("  orr_desktop probe               show detected ffmpeg encoders");
            println!("  orr_desktop cli-rec [s] [out]   record fullscreen for s seconds");
            println!("  orr_desktop cli-area X Y W H [s] [out]");
            println!("                                  record a fixed region");
            println!("  orr_desktop --version           print version");
            println!();
            println!("Environment: ORR_FFMPEG overrides the ffmpeg path.");
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

    let ffmpeg = std::env::var("ORR_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let caps = recorder::detect(&ffmpeg);
    let cfg_path = Settings::config_path();
    let app_settings = Settings::load(&cfg_path).sanitized();

    let mut app = App {
        proxy,
        ffmpeg,
        caps,
        cfg_path,
        settings: app_settings,
        tray: None,
        menus: None,
        session: None,
        selecting: false,
        exiting: false,
        exit_pending: false,
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
        if self.caps.encoders.is_empty() {
            self.fatal("no usable h264 encoder found - is ffmpeg installed and on PATH?");
            return;
        }
        let encoder = match recorder::resolve_encoder(self.settings.encoder, &self.caps) {
            Some(e) => e,
            None => match recorder::resolve_encoder(Encoder::Auto, &self.caps) {
                Some(e) => e,
                None => {
                    self.fatal("selected encoder unavailable");
                    return;
                }
            },
        };
        if let Err(e) = recorder::validate_output_dir(&self.settings.output_dir) {
            self.fatal(&format!("{e}"));
            return;
        }
        let path = recorder::output_file(&self.settings.output_dir);
        let cmd = match recorder::build_command(
            &self.ffmpeg,
            &self.settings,
            &self.caps,
            mode,
            encoder,
            &path,
        ) {
            Ok(c) => c,
            Err(e) => {
                self.fatal(&format!("cannot build ffmpeg command: {e}"));
                return;
            }
        };
        match recorder::start(cmd) {
            Ok(spawned) => {
                self.set_tooltip("ORR starting...");
                let monitor_path = path.clone();
                self.spawn_monitor(spawned.child, monitor_path);
                self.session = Some(Session {
                    stdin: spawned.stdin,
                    started: Instant::now(),
                    path,
                });
                self.sync_menu();
            }
            Err(e) => self.fatal(&format!("cannot start ffmpeg: {e:#}")),
        }
    }

    fn spawn_monitor(&self, mut child: std::process::Child, out_path: PathBuf) {
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let stderr = child.stderr.take();
            let tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let tail2 = tail.clone();
            let reader = std::thread::spawn(move || {
                use std::io::Read;
                if let Some(mut e) = stderr {
                    let mut buf = [0u8; 4096];
                    loop {
                        match e.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let mut t = tail2.lock().unwrap();
                                t.push_str(&String::from_utf8_lossy(&buf[..n]));
                                let len = t.len();
                                if len > 4000 {
                                    t.drain(..len - 4000);
                                }
                            }
                        }
                    }
                }
            });
            let status = loop {
                std::thread::sleep(Duration::from_secs(1));
                match child.try_wait() {
                    Ok(None) => {
                        let _ = proxy.send_event(UserEvent::Tick);
                    }
                    Ok(Some(st)) => break st,
                    Err(e) => {
                        let _ = proxy.send_event(UserEvent::Finished(Err(format!(
                            "ffmpeg wait failed: {e}"
                        ))));
                        let _ = reader.join();
                        return;
                    }
                }
            };
            let _ = reader.join();
            let tail_msg = tail.lock().unwrap().clone();
            let result = if status.success() {
                Ok(out_path)
            } else {
                Err(format!("ffmpeg exited with {status}\n{tail_msg}"))
            };
            let _ = proxy.send_event(UserEvent::Finished(result));
        });
    }

    fn stop(&mut self) {
        if let Some(s) = &mut self.session {
            recorder::graceful_stop(&mut s.stdin);
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
                self.sync_menu();
            }
            "mouse" => {
                self.settings.capture_mouse = !self.settings.capture_mouse;
                self.save_settings();
                self.sync_menu();
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

    let settings_sub = Submenu::with_id("settings_sub", "Settings", true);
    settings_sub.append_items(&[&quality_sub, &fps_sub, &enc_sub, &perf, &mouse])?;

    menu.append_items(&[
        &record_full,
        &record_area,
        &stop,
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
        quality,
        fps: fps_items,
        encoder: encoders,
        perf,
        mouse,
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

    let ffmpeg = std::env::var("ORR_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let caps = recorder::detect(&ffmpeg);
    let st = Settings::load(&Settings::config_path()).sanitized();
    let encoder = recorder::resolve_encoder(st.encoder, &caps)
        .or_else(|| recorder::resolve_encoder(Encoder::Auto, &caps))
        .ok_or_else(|| anyhow::anyhow!("no h264 encoder available"))?;

    let cmd = recorder::build_command(&ffmpeg, &st, &caps, Mode::Area(rect), encoder, &out)?;
    let spawned = recorder::start(cmd)?;
    println!("[cli] area {rect:?} for {seconds}s via {}", encoder.label());
    std::thread::sleep(Duration::from_secs(seconds));
    let mut stdin = spawned.stdin;
    let mut child = spawned.child;
    recorder::graceful_stop(&mut stdin);
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed with {status}");
    }
    println!("[cli] done, size={} bytes", std::fs::metadata(&out)?.len());
    Ok(())
}

fn cli_record(args: &[String]) -> Result<()> {
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = match args.get(3) {
        Some(p) => PathBuf::from(p),
        None => recorder::output_file(&std::env::current_dir()?),
    };

    let ffmpeg = std::env::var("ORR_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string());
    let caps = recorder::detect(&ffmpeg);
    let st = Settings::load(&Settings::config_path()).sanitized();

    let encoder = recorder::resolve_encoder(st.encoder, &caps)
        .or_else(|| recorder::resolve_encoder(Encoder::Auto, &caps))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no h264 encoder available (install ffmpeg with nvenc/qsv/amf or libx264)"
            )
        })?;
    recorder::validate_output_dir(out.parent().unwrap_or(std::path::Path::new(".")))?;

    let cmd = recorder::build_command(&ffmpeg, &st, &caps, Mode::FullScreen, encoder, &out)?;
    let spawned = recorder::start(cmd)?;
    println!(
        "[cli] recording {seconds}s fullscreen via {} -> {}",
        encoder.label(),
        out.display()
    );
    std::thread::sleep(Duration::from_secs(seconds));
    let mut stdin = spawned.stdin;
    let mut child = spawned.child;
    recorder::graceful_stop(&mut stdin);
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("ffmpeg failed with {status}");
    }
    println!("[cli] done, size={} bytes", std::fs::metadata(&out)?.len());
    Ok(())
}

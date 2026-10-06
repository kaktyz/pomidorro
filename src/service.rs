use crate::{
    ipc::{self, Command, Reply},
    model::{self, Phase, Session, Settings, Snapshot, Timer},
    storage::Storage,
};
use anyhow::{Context, Result};
use chrono::Local;
use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSScreen, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Child, Command as Process},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime},
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

struct Request {
    command: Command,
    response: mpsc::Sender<Reply>,
}

struct App {
    storage: Storage,
    settings: Settings,
    sessions: Vec<Session>,
    timer: Timer,
    message: String,
    overlays: Vec<Retained<NSWindow>>,
    dimmed: bool,
    screens: Vec<(f64, f64, f64, f64)>,
    audio: Option<Child>,
    last_tick_sound: Instant,
    tray: Option<TrayIcon>,
    toggle: MenuItem,
    status: MenuItem,
    dim: MenuItem,
    sound: MenuItem,
    skip: MenuItem,
    reset: MenuItem,
    quit: MenuItem,
}

impl App {
    fn new() -> Result<Self> {
        let storage = Storage::new()?;
        let settings = storage.settings()?;
        let sessions = storage.sessions()?;
        Ok(Self {
            timer: Timer::new(&settings),
            storage,
            settings,
            sessions,
            message: "Готов к работе — Space для старта".into(),
            overlays: vec![],
            dimmed: false,
            screens: vec![],
            audio: None,
            last_tick_sound: Instant::now(),
            tray: None,
            toggle: MenuItem::new("Старт / Пауза", true, None),
            status: MenuItem::new("Готов", false, None),
            dim: MenuItem::new("Затемнение", true, None),
            sound: MenuItem::new("Звук", true, None),
            skip: MenuItem::new("Пропустить этап", true, None),
            reset: MenuItem::new("Сбросить цикл", true, None),
            quit: MenuItem::new("Завершить Pomidorro", true, None),
        })
    }
    fn create_tray(&mut self) -> Result<()> {
        let menu = Menu::new();
        menu.append_items(&[
            &self.status,
            &PredefinedMenuItem::separator(),
            &self.toggle,
            &self.skip,
            &self.reset,
            &PredefinedMenuItem::separator(),
            &self.dim,
            &self.sound,
            &PredefinedMenuItem::separator(),
            &self.quit,
        ])?;
        self.tray = Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_title("◉ 25:00")
                .with_tooltip("Pomidorro")
                .build()?,
        );
        Ok(())
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            phase: self.timer.phase,
            running: self.timer.running,
            remaining_secs: self.timer.remaining.as_secs()
                + u64::from(self.timer.remaining.subsec_nanos() > 0),
            total_secs: self.timer.total.as_secs(),
            cycle_completed: self.timer.cycle_completed,
            settings: self.settings.clone(),
            stats: model::statistics(
                &self.sessions,
                Local::now().date_naive(),
                self.settings.daily_goal,
            ),
            message: self.message.clone(),
        }
    }
    fn command(&mut self, command: Command) -> Result<bool> {
        match command {
            Command::Status => {}
            Command::TestNotification => {
                notify("Проверка уведомлений. Пора сделать небольшой перерыв!");
                if self.settings.sound {
                    self.play("Glass");
                }
                self.message =
                    "Проверка отправлена. Разреши уведомления Script Editor в настройках macOS"
                        .into();
            }
            Command::Toggle => {
                self.timer.toggle();
                self.message = if self.timer.running {
                    "Таймер идёт"
                } else {
                    "На паузе"
                }
                .into();
            }
            Command::Skip => {
                self.timer.next(&self.settings);
                self.message = "Этап пропущен — помидор не засчитан".into();
            }
            Command::Reset => {
                self.timer = Timer::new(&self.settings);
                self.message = "Цикл сброшен".into();
            }
            Command::ToggleDim => {
                let mut settings = self.settings.clone();
                settings.dim_enabled = !settings.dim_enabled;
                self.configure(settings)?;
            }
            Command::ToggleSound => {
                let mut settings = self.settings.clone();
                settings.sound = !settings.sound;
                self.configure(settings)?;
            }
            Command::Settings(settings) => self.configure(settings)?,
            Command::Shutdown => return Ok(true),
        }
        Ok(false)
    }
    fn configure(&mut self, settings: Settings) -> Result<()> {
        self.storage.save_settings(&settings)?;
        self.timer.reconfigure(&settings);
        self.settings = settings;
        self.message = "Настройки сохранены".into();
        for window in self.overlays.drain(..) {
            window.orderOut(None);
        }
        self.dimmed = false; // Rebuild windows when opacity changes.
        Ok(())
    }
    fn tick(&mut self, elapsed: Duration) {
        if elapsed > Duration::from_secs(5) && self.timer.running {
            self.timer.running = false;
            self.message = "После сна или задержки таймер на паузе — Space для продолжения".into();
        }
        let (changed, completed) = self.timer.advance(elapsed, &self.settings);
        if let Some(seconds) = completed {
            self.sessions.push(Session {
                completed_at: Local::now(),
                seconds,
            });
            if let Err(error) = self.storage.save_sessions(&self.sessions) {
                self.message = format!("Ошибка сохранения истории: {error}");
            }
        }
        if changed {
            let message = if self.timer.phase == Phase::Focus {
                "Перерыв завершён. Время сфокусироваться."
            } else {
                "Помидор завершён. Отдохни и разомнись."
            };
            notify(message);
            if self.settings.sound {
                self.play("Glass");
            }
        }
        if self.timer.running
            && self.timer.phase == Phase::Focus
            && self.settings.ticking
            && self.last_tick_sound.elapsed() >= Duration::from_secs(1)
        {
            self.last_tick_sound = Instant::now();
            self.play("Pop");
        }
        if self
            .audio
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.audio = None;
        }
    }
    fn play(&mut self, name: &str) {
        if self.audio.is_some() {
            return;
        }
        self.audio = Process::new("/usr/bin/afplay")
            .args([
                "-v",
                if name == "Pop" { "0.08" } else { "0.5" },
                &format!("/System/Library/Sounds/{name}.aiff"),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
    }
    fn refresh(&mut self, mtm: MainThreadMarker) {
        let state = self.snapshot();
        if let Some(tray) = &self.tray {
            tray.set_title(Some(format!(
                "{} {:02}:{:02}",
                if !state.running {
                    "Ⅱ"
                } else if state.phase == Phase::Focus {
                    "◉"
                } else {
                    "☕"
                },
                state.remaining_secs / 60,
                state.remaining_secs % 60
            )));
        }
        self.status.set_text(format!(
            "{} · сегодня {}/{}",
            state.phase.label(),
            state.stats.today_count,
            self.settings.daily_goal
        ));
        self.toggle.set_text(if self.timer.running {
            "Пауза"
        } else {
            "Старт / Продолжить"
        });
        self.dim.set_text(format!(
            "Затемнение: {}",
            if self.settings.dim_enabled {
                "вкл"
            } else {
                "выкл"
            }
        ));
        self.sound.set_text(format!(
            "Звук: {}",
            if self.settings.sound {
                "вкл"
            } else {
                "выкл"
            }
        ));
        let dim =
            self.timer.running && self.timer.phase != Phase::Focus && self.settings.dim_enabled;
        let screens = NSScreen::screens(mtm);
        let frames: Vec<_> = screens
            .iter()
            .map(|s| {
                let f = s.frame();
                (f.origin.x, f.origin.y, f.size.width, f.size.height)
            })
            .collect();
        if dim != self.dimmed || (dim && frames != self.screens) {
            for window in self.overlays.drain(..) {
                window.orderOut(None);
            }
            if dim {
                for screen in screens.iter() {
                    // All AppKit access is on the event-loop main thread. Retained windows
                    // are ordered out before release and never become key or capture input.
                    let window = unsafe {
                        NSWindow::initWithContentRect_styleMask_backing_defer(
                            NSWindow::alloc(mtm),
                            screen.frame(),
                            NSWindowStyleMask::Borderless,
                            NSBackingStoreType::Buffered,
                            false,
                        )
                    };
                    unsafe {
                        window.setReleasedWhenClosed(false);
                    }
                    window.setBackgroundColor(Some(&NSColor::blackColor()));
                    window.setOpaque(false);
                    window.setAlphaValue(self.settings.dim_opacity);
                    window.setHasShadow(false);
                    window.setIgnoresMouseEvents(true);
                    window.setLevel(3); // Floating, below the menu bar and system dialogs.
                    window.setCollectionBehavior(
                        NSWindowCollectionBehavior::CanJoinAllSpaces
                            | NSWindowCollectionBehavior::FullScreenAuxiliary
                            | NSWindowCollectionBehavior::Stationary,
                    );
                    window.orderFrontRegardless();
                    self.overlays.push(window);
                }
            }
            self.screens = frames;
            self.dimmed = dim;
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for window in &self.overlays {
            window.orderOut(None);
        }
        if let Some(child) = &mut self.audio {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn notify(message: &str) {
    let message = message.to_owned();
    thread::spawn(move || {
        let result = Process::new("/usr/bin/osascript")
            .args([
                "-e",
                &format!("display notification {message:?} with title \"Pomidorro\""),
            ])
            .output();
        if let Ok(output) = result
            && !output.status.success()
        {
            eprintln!("Уведомление: {}", String::from_utf8_lossy(&output.stderr));
        }
    });
}

struct SocketGuard(std::path::PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn run() -> Result<()> {
    let socket = ipc::socket_path();
    let parent = socket.parent().context("Нет каталога сокета")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    // Bind first: a second service never overwrites a running instance's socket.
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return Ok(());
            }
            if error.kind() != std::io::ErrorKind::AddrInUse {
                return Err(error.into());
            }
            fs::remove_file(&socket)?;
            UnixListener::bind(&socket)?
        }
    };
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let _guard = SocketGuard(socket);
    let mut app = App::new()?;
    let (sender, receiver) = mpsc::channel::<Request>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            if BufReader::new(&mut stream)
                .take(65536)
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let Ok(command) = serde_json::from_str(&line) else {
                continue;
            };
            let (response, reply) = mpsc::channel();
            if sender.send(Request { command, response }).is_err() {
                break;
            }
            if let Ok(reply) = reply.recv_timeout(Duration::from_secs(2)) {
                let _ = serde_json::to_writer(&mut stream, &reply);
                let _ = stream.write_all(b"\n");
            }
        }
    });
    use tao::platform::macos::EventLoopExtMacOS;
    use tao::platform::run_return::EventLoopExtRunReturn;
    let mut event_loop = EventLoopBuilder::new().build();
    event_loop.set_activation_policy(tao::platform::macos::ActivationPolicy::Accessory);
    let mtm = MainThreadMarker::new().context("macOS UI требует главный поток")?;
    let mut last = Instant::now();
    let mut last_wall = SystemTime::now();
    let exit_code = event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(100));
        if matches!(event, Event::NewEvents(StartCause::Init))
            && let Err(error) = app.create_tray()
        {
            eprintln!("Ошибка меню-бара: {error}");
            *control_flow = ControlFlow::Exit;
            return;
        }
        if matches!(event, Event::MainEventsCleared) {
            let now = Instant::now();
            let wall = SystemTime::now();
            let wall_elapsed = wall.duration_since(last_wall).unwrap_or_default();
            // Instant on macOS doesn't count sleep. Wall time is used only to
            // detect suspension; normal countdowns use the monotonic clock.
            app.tick(if wall_elapsed > Duration::from_secs(5) {
                wall_elapsed
            } else {
                now.duration_since(last)
            });
            last = now;
            last_wall = wall;
            while let Ok(request) = receiver.try_recv() {
                let result = app.command(request.command);
                let exit = matches!(result, Ok(true));
                let error = result.err().map(|e| e.to_string());
                let _ = request.response.send(Reply {
                    snapshot: app.snapshot(),
                    error,
                });
                if exit {
                    *control_flow = ControlFlow::Exit;
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let command = if event.id == app.toggle.id() {
                    Some(Command::Toggle)
                } else if event.id == app.skip.id() {
                    Some(Command::Skip)
                } else if event.id == app.reset.id() {
                    Some(Command::Reset)
                } else if event.id == app.dim.id() {
                    Some(Command::ToggleDim)
                } else if event.id == app.sound.id() {
                    Some(Command::ToggleSound)
                } else if event.id == app.quit.id() {
                    Some(Command::Shutdown)
                } else {
                    None
                };
                if let Some(command) = command {
                    match app.command(command) {
                        Ok(true) => *control_flow = ControlFlow::Exit,
                        Err(error) => app.message = error.to_string(),
                        _ => {}
                    }
                }
            }
            app.refresh(mtm);
        }
    });
    // tao::run exits the process directly; run_return lets us release overlays,
    // reap audio subprocesses and remove the socket before normal termination.
    drop(app);
    anyhow::ensure!(exit_code == 0, "Цикл macOS завершился с кодом {exit_code}");
    Ok(())
}

use crate::{
    ipc::{self, Command},
    model::{Settings, Snapshot},
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};
use std::{
    fs::OpenOptions,
    io::{self, IsTerminal},
    process::{Command as Process, Stdio},
    thread,
    time::{Duration, Instant},
};

enum Page {
    Main,
    Stats,
    Settings,
    Help,
    Exit,
}
struct Ui {
    state: Snapshot,
    page: Page,
    draft: Settings,
    selected: usize,
    history_offset: usize,
    error: String,
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

pub fn run() -> Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "Запусти pomidorro в интерактивном терминале"
    );
    let state = connect()?;
    terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut ui = Ui {
        draft: state.settings.clone(),
        state,
        page: Page::Main,
        selected: 0,
        history_offset: 0,
        error: String::new(),
    };
    let mut refreshed = Instant::now();
    loop {
        terminal.draw(|frame| draw(frame, &ui))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                ui.page = Page::Exit;
                continue;
            }
            if matches!(ui.page, Page::Exit) {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('й') | KeyCode::Enter | KeyCode::F(10) => {
                        send(&mut ui, Command::Shutdown);
                        if ui.error.is_empty() {
                            break;
                        }
                    }
                    KeyCode::Char('b') | KeyCode::Char('и') => break,
                    KeyCode::Esc => ui.page = Page::Main,
                    _ => {}
                }
                continue;
            }
            if matches!(ui.page, Page::Settings) {
                match key.code {
                    KeyCode::Esc => ui.page = Page::Main,
                    KeyCode::Tab | KeyCode::Down => ui.selected = (ui.selected + 1) % 10,
                    KeyCode::BackTab | KeyCode::Up => ui.selected = (ui.selected + 9) % 10,
                    KeyCode::Left | KeyCode::Char('-') => adjust(&mut ui, -1),
                    KeyCode::Right | KeyCode::Char('+') | KeyCode::Char(' ') => adjust(&mut ui, 1),
                    KeyCode::Enter => {
                        let settings = ui.draft.clone();
                        send(&mut ui, Command::Settings(settings));
                        if ui.error.is_empty() {
                            ui.page = Page::Main;
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::F(1) | KeyCode::Char('h') => ui.page = Page::Help,
                KeyCode::F(2) | KeyCode::Char(' ') => send(&mut ui, Command::Toggle),
                KeyCode::F(3) | KeyCode::Char('n') => send(&mut ui, Command::Skip),
                KeyCode::F(4) | KeyCode::Char('s') => ui.page = Page::Stats,
                KeyCode::F(5) | KeyCode::Char('o') => {
                    ui.draft = ui.state.settings.clone();
                    ui.page = Page::Settings;
                }
                KeyCode::F(6) | KeyCode::Char('d') => send(&mut ui, Command::ToggleDim),
                KeyCode::F(7) | KeyCode::Char('m') => send(&mut ui, Command::ToggleSound),
                KeyCode::F(8) | KeyCode::Char('t') => send(&mut ui, Command::TestNotification),
                KeyCode::Down if matches!(ui.page, Page::Stats) => {
                    ui.history_offset =
                        (ui.history_offset + 1).min(ui.state.stats.recent.len().saturating_sub(1));
                }
                KeyCode::Up if matches!(ui.page, Page::Stats) => {
                    ui.history_offset = ui.history_offset.saturating_sub(1);
                }
                KeyCode::Char('r') => send(&mut ui, Command::Reset),
                KeyCode::F(10) | KeyCode::Char('q') => ui.page = Page::Exit,
                KeyCode::Esc => ui.page = Page::Main,
                _ => {}
            }
        }
        if refreshed.elapsed() >= Duration::from_millis(500) {
            match ipc::request(&Command::Status) {
                Ok(state) => ui.state = state,
                Err(error) => {
                    return Err(error.context("Сервис завершён. Перезапусти pomidorro"));
                }
            }
            refreshed = Instant::now();
        }
    }
    Ok(())
}

fn connect() -> Result<Snapshot> {
    if let Ok(state) = ipc::request(&Command::Status) {
        return Ok(state);
    }
    let storage = crate::storage::Storage::new()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(storage.root.join("service.log"))?;
    let mut command = Process::new(std::env::current_exe()?);
    command
        .arg("--service")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    use std::os::unix::process::CommandExt;
    // setsid is async-signal-safe; no allocation or locks are used in pre_exec.
    // The service survives closing the terminal after choosing background mode.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    for _ in 0..60 {
        if let Ok(state) = ipc::request(&Command::Status) {
            thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(state);
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "Сервис завершился ({status}). Лог: {}",
                storage.root.join("service.log").display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    anyhow::bail!(
        "Сервис не запустился за 6 секунд. Лог: {}",
        storage.root.join("service.log").display()
    )
}

fn send(ui: &mut Ui, command: Command) {
    match ipc::request(&command) {
        Ok(state) => {
            ui.state = state;
            ui.error.clear();
        }
        Err(error) => ui.error = error.to_string(),
    }
}

fn adjust(ui: &mut Ui, step: i32) {
    let s = &mut ui.draft;
    fn shift(value: &mut u64, step: i32, max: u64) {
        *value = (*value as i64 + step as i64).clamp(1, max as i64) as u64;
    }
    match ui.selected {
        0 => shift(&mut s.focus_minutes, step, 240),
        1 => shift(&mut s.break_minutes, step, 120),
        2 => shift(&mut s.long_break_minutes, step, 120),
        3 => s.sessions_per_cycle = (s.sessions_per_cycle as i32 + step).clamp(1, 12) as u32,
        4 => s.daily_goal = (s.daily_goal as i32 + step).clamp(1, 100) as u32,
        5 => s.dim_enabled = !s.dim_enabled,
        6 => {
            s.dim_opacity =
                ((s.dim_opacity * 100.0).round() + step as f64 * 5.0).clamp(10.0, 85.0) / 100.0
        }
        7 => s.sound = !s.sound,
        8 => s.ticking = !s.ticking,
        9 => ui.draft = Settings::default(),
        _ => {}
    }
}

const ACCENT: Color = Color::Cyan;
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
}
fn on(value: bool) -> &'static str {
    if value { "Вкл" } else { "Выкл" }
}

fn draw(frame: &mut Frame, ui: &Ui) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(15, 23, 42)).fg(Color::White)),
        area,
    );
    if area.width < 54 || area.height < 19 {
        let message = if matches!(ui.page, Page::Exit) {
            "Выход: Enter/q — остановить, b — оставить в фоне, Esc — отмена".to_owned()
        } else {
            format!(
                "Увеличь терминал до 54×19 или больше.\n{} {:02}:{:02}\nSpace: старт/пауза · q: выход",
                ui.state.phase.label(),
                ui.state.remaining_secs / 60,
                ui.state.remaining_secs % 60
            )
        };
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), area);
        return;
    }
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(9),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .split(area);
    let s = &ui.state;
    frame.render_widget(
        Paragraph::new(format!(
            "POMIDORRO   │   {}   │   {}   │   Цикл {}/{}",
            s.phase.label(),
            if s.running { "Идёт" } else { "Пауза" },
            s.cycle_completed,
            s.settings.sessions_per_cycle
        ))
        .block(panel(" 🍅 Rust / macOS ")),
        layout[0],
    );
    match ui.page {
        Page::Main | Page::Exit => main_page(frame, layout[1], s),
        Page::Stats => stats_page(frame, layout[1], s, ui.history_offset),
        Page::Settings => settings_page(frame, layout[1], ui),
        Page::Help => {
            frame.render_widget(Paragraph::new("Space / F2 — старт, пауза, продолжение\nn / F3 — пропустить этап (без зачёта помидора)\ns / F4 — статистика и история; ↑↓ — прокрутка\no / F5 — настройки; ↑↓ / Tab — выбор, ←→ — изменить\nd / F6 — затемнение на перерывах\nm / F7 — сигнал смены этапов\nt / F8 — проверить уведомление и звук\nr — сбросить цикл\nq / F10 — выход; Esc — назад\n\nТаймер продолжает работать при скрытом терминале.\nПосле сна Mac таймер ставится на паузу.\nСерия растёт при достижении дневной цели.").block(panel(" Помощь ")).wrap(Wrap { trim: true }), layout[1]);
        }
    }
    frame.render_widget(
        Paragraph::new(if ui.error.is_empty() {
            s.message.as_str()
        } else {
            ui.error.as_str()
        })
        .style(Style::default().fg(if ui.error.is_empty() {
            Color::Gray
        } else {
            Color::Red
        }))
        .block(panel(" Статус ")),
        layout[2],
    );
    frame.render_widget(Paragraph::new("1 Помощь  2 Пауза  3 Пропуск  4 Стат.  5 Настр.\n6 Затемн.  7 Звук  8 Проверка  10 Выход  │  Space").style(Style::default().fg(Color::Black).bg(ACCENT)), layout[3]);
    if matches!(ui.page, Page::Exit) {
        let popup = centered(area, 54, 9);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new("Enter / q — остановить таймер и выйти\n\nb — оставить таймер работать в меню-баре\n\nEsc — вернуться").block(panel(" Выход ")).style(Style::default().bg(Color::Rgb(30, 41, 59))).alignment(Alignment::Center), popup);
    }
}

fn main_page(frame: &mut Frame, area: Rect, s: &Snapshot) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(1),
    ])
    .split(area);
    let time = format!("{:02}:{:02}", s.remaining_secs / 60, s.remaining_secs % 60);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(time, Style::default().fg(ACCENT).bold())),
            Line::from(s.phase.label()),
        ])
        .alignment(Alignment::Center)
        .block(panel(" Таймер ")),
        rows[0],
    );
    let progress = 1.0 - (s.remaining_secs as f64 / s.total_secs.max(1) as f64).clamp(0.0, 1.0);
    frame.render_widget(
        Gauge::default()
            .ratio(progress)
            .gauge_style(Style::default().fg(ACCENT))
            .block(panel(" Прогресс ")),
        rows[1],
    );
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
    frame.render_widget(
        Paragraph::new(format!(
            "Помидоры: {} / {}\nФокус: {} мин\nСерия: {} дн.\nЗа 7 дней: {} пом.",
            s.stats.today_count,
            s.settings.daily_goal,
            s.stats.today_seconds / 60,
            s.stats.streak,
            s.stats.week_count
        ))
        .block(panel(" Сегодня ")),
        cols[0],
    );
    frame.render_widget(Paragraph::new(format!("Работа: {} мин\nПерерыв: {} мин\nДлинный: {} мин\nЗатемнение: {} · {:.0}%\nЗвук: {} · Тик: {}", s.settings.focus_minutes, s.settings.break_minutes, s.settings.long_break_minutes, on(s.settings.dim_enabled), s.settings.dim_opacity * 100.0, on(s.settings.sound), on(s.settings.ticking))).block(panel(" Цикл ")), cols[1]);
}

fn stats_page(frame: &mut Frame, area: Rect, s: &Snapshot, offset: usize) {
    let rows = Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).split(area);
    frame.render_widget(Paragraph::new(format!("Сегодня: {} пом. / {} мин   │   Серия: {} дн.\nЗа 7 дней: {} пом. / {} мин   │   Цель: {} пом./день", s.stats.today_count, s.stats.today_seconds / 60, s.stats.streak, s.stats.week_count, s.stats.week_seconds / 60, s.settings.daily_goal)).block(panel(" Статистика · Esc назад ")), rows[0]);
    let items: Vec<_> = if s.stats.recent.is_empty() {
        vec![ListItem::new("История пока пуста. Заверши первый помидор!")]
    } else {
        s.stats
            .recent
            .iter()
            .skip(offset)
            .map(|session| {
                ListItem::new(format!(
                    "  {}    {:3} мин    ✓ Завершён",
                    session.completed_at.format("%d.%m.%Y %H:%M"),
                    session.seconds / 60
                ))
            })
            .collect()
    };
    frame.render_widget(
        List::new(items).block(panel(" Последние сессии · ↑↓ прокрутка ")),
        rows[1],
    );
}

fn settings_page(frame: &mut Frame, area: Rect, ui: &Ui) {
    let s = &ui.draft;
    let labels = [
        format!("Фокус                      {} мин", s.focus_minutes),
        format!("Короткий перерыв            {} мин", s.break_minutes),
        format!("Длинный перерыв             {} мин", s.long_break_minutes),
        format!("Помидоров в цикле           {}", s.sessions_per_cycle),
        format!("Дневная цель                {}", s.daily_goal),
        format!("Затемнение на перерывах     {}", on(s.dim_enabled)),
        format!("Интенсивность затемнения    {:.0}%", s.dim_opacity * 100.0),
        format!("Сигнал смены этапа          {}", on(s.sound)),
        format!("Тихое тикание в фокусе      {}", on(s.ticking)),
        "Вернуть значения по умолчанию".into(),
    ];
    let height = area.height.saturating_sub(3) as usize;
    let offset = ui.selected.saturating_sub(height.saturating_sub(1));
    let mut lines: Vec<_> = labels
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, text)| {
            Line::from(Span::styled(
                format!("{} {text}", if i == ui.selected { "▸" } else { " " }),
                if i == ui.selected {
                    Style::default().fg(Color::Black).bg(ACCENT).bold()
                } else {
                    Style::default()
                },
            ))
        })
        .collect();
    lines.push(Line::from(
        "↑↓ Tab: выбор · ←→: изменить · Enter: сохранить · Esc: отмена",
    ));
    frame.render_widget(Paragraph::new(lines).block(panel(" Настройки ")), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

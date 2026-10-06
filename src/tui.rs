use crate::{
    ipc::{self, Command},
    model::{Phase, Settings, Snapshot},
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Gauge, List, ListItem, Padding, Paragraph, Wrap},
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
                    KeyCode::Char('q')
                    | KeyCode::Char('й')
                    | KeyCode::Enter
                    | KeyCode::Char('0') => {
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
                    KeyCode::Char('0') => ui.page = Page::Exit,
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
                KeyCode::Char('1') | KeyCode::Char('h') => ui.page = Page::Help,
                KeyCode::Char('2') | KeyCode::Char(' ') => send(&mut ui, Command::Toggle),
                KeyCode::Char('3') | KeyCode::Char('n') => send(&mut ui, Command::Skip),
                KeyCode::Char('4') | KeyCode::Char('s') => ui.page = Page::Stats,
                KeyCode::Char('5') | KeyCode::Char('o') => {
                    ui.draft = ui.state.settings.clone();
                    ui.page = Page::Settings;
                }
                KeyCode::Char('6') | KeyCode::Char('d') => send(&mut ui, Command::ToggleDim),
                KeyCode::Char('7') | KeyCode::Char('m') => send(&mut ui, Command::ToggleSound),
                KeyCode::Char('8') | KeyCode::Char('t') => send(&mut ui, Command::TestNotification),
                KeyCode::Down if matches!(ui.page, Page::Stats) => {
                    ui.history_offset =
                        (ui.history_offset + 1).min(ui.state.stats.recent.len().saturating_sub(1));
                }
                KeyCode::Up if matches!(ui.page, Page::Stats) => {
                    ui.history_offset = ui.history_offset.saturating_sub(1);
                }
                KeyCode::Char('r') => send(&mut ui, Command::Reset),
                KeyCode::Char('0') | KeyCode::Char('q') => ui.page = Page::Exit,
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

const BACKGROUND: Color = Color::Rgb(17, 19, 30);
const SURFACE: Color = Color::Rgb(26, 29, 44);
const BORDER: Color = Color::Rgb(55, 60, 81);
const TEXT: Color = Color::Rgb(226, 230, 243);
const MUTED: Color = Color::Rgb(146, 153, 177);
const ACCENT: Color = Color::Rgb(183, 166, 250);
const FOCUS: Color = Color::Rgb(250, 179, 153);
const REST: Color = Color::Rgb(143, 218, 187);

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .title_style(Style::default().fg(TEXT).bold())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE).fg(TEXT))
        .padding(Padding::horizontal(1))
}
fn on(value: bool) -> &'static str {
    if value { "Вкл" } else { "Выкл" }
}

fn draw(frame: &mut Frame, ui: &Ui) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BACKGROUND).fg(TEXT)),
        area,
    );
    if area.width < 54 || area.height < 19 {
        let message = if matches!(ui.page, Page::Exit) {
            "Выход: Enter/0 — остановить, b — оставить в фоне, Esc — отмена".to_owned()
        } else {
            format!(
                "Увеличь терминал до 54×19 или больше.\n{} {:02}:{:02}\n2/Space: старт/пауза · 0: выход",
                ui.state.phase.label(),
                ui.state.remaining_secs / 60,
                ui.state.remaining_secs % 60
            )
        };
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: true }), area);
        return;
    }
    let layout = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(9),
        Constraint::Length(2),
        Constraint::Length(3),
    ])
    .split(area.inner(Margin::new(1, 1)));
    let s = &ui.state;
    let phase_color = if s.phase == Phase::Focus { FOCUS } else { REST };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("● POMIDORRO", Style::default().fg(FOCUS).bold()),
            Span::styled("   /   ", Style::default().fg(BORDER)),
            Span::styled(s.phase.label(), Style::default().fg(phase_color)),
            Span::styled(
                format!(
                    "   {}   ·   Цикл {}/{}",
                    if s.running {
                        "● Идёт"
                    } else {
                        "Ⅱ Пауза"
                    },
                    s.cycle_completed,
                    s.settings.sessions_per_cycle
                ),
                Style::default().fg(MUTED),
            ),
        ])),
        layout[0],
    );
    match ui.page {
        Page::Main | Page::Exit => main_page(frame, layout[1], s),
        Page::Stats => stats_page(frame, layout[1], s, ui.history_offset),
        Page::Settings => settings_page(frame, layout[1], ui),
        Page::Help => {
            frame.render_widget(Paragraph::new("2 / Space — старт, пауза, продолжение\n3 / n — пропустить этап (без зачёта помидора)\n4 / s — статистика и история; ↑↓ — прокрутка\n5 / o — настройки; ↑↓ / Tab — выбор, ←→ — изменить\n6 / d — затемнение на перерывах\n7 / m — сигнал смены этапов\n8 / t — проверить уведомление и звук\nr — сбросить цикл\n0 / q — выход; Esc — назад\n\nТаймер продолжает работать при скрытом терминале.\nПосле сна Mac таймер ставится на паузу.\nСерия растёт при достижении дневной цели.").block(panel(" Помощь ")).wrap(Wrap { trim: true }), layout[1]);
        }
    }
    frame.render_widget(
        Paragraph::new(if ui.error.is_empty() {
            s.message.as_str()
        } else {
            ui.error.as_str()
        })
        .style(Style::default().fg(if ui.error.is_empty() {
            MUTED
        } else {
            Color::Red
        }))
        .wrap(Wrap { trim: true }),
        layout[2],
    );
    draw_shortcuts(frame, layout[3], ui);
    if matches!(ui.page, Page::Exit) {
        let popup = centered(area, 54, 9);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new("Enter / 0 — остановить таймер и выйти\n\nb — оставить таймер работать в меню-баре\n\nEsc — вернуться").block(panel(" Завершить сессию? ").border_style(Style::default().fg(ACCENT))).alignment(Alignment::Center), popup);
    }
}

fn main_page(frame: &mut Frame, area: Rect, s: &Snapshot) {
    let rows = Layout::vertical([
        Constraint::Length(if area.height >= 17 { 9 } else { 5 }),
        Constraint::Length(if area.height >= 17 { 3 } else { 2 }),
        Constraint::Min(1),
    ])
    .split(area);
    let time = format!("{:02}:{:02}", s.remaining_secs / 60, s.remaining_secs % 60);
    let color = if s.phase == Phase::Focus { FOCUS } else { REST };
    let mut clock = vec![Line::from(Span::styled(
        s.phase.label(),
        Style::default().fg(MUTED),
    ))];
    if rows[0].height >= 9 {
        clock.extend(
            clock_digits(&time)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(color)))),
        );
    } else {
        clock.push(Line::from(Span::styled(
            time,
            Style::default().fg(color).bold(),
        )));
    }
    clock.push(Line::from(Span::styled(
        if s.running {
            "2 / Space — поставить на паузу"
        } else {
            "2 / Space — начать или продолжить"
        },
        Style::default().fg(MUTED),
    )));
    frame.render_widget(
        Paragraph::new(clock)
            .alignment(Alignment::Center)
            .block(panel(" Таймер ").border_style(Style::default().fg(color))),
        rows[0],
    );
    let progress = 1.0 - (s.remaining_secs as f64 / s.total_secs.max(1) as f64).clamp(0.0, 1.0);
    frame.render_widget(
        Gauge::default()
            .ratio(progress)
            .gauge_style(Style::default().fg(color).bg(BORDER))
            .label(format!("{:.0}% завершено", progress * 100.0))
            .block(if rows[1].height >= 3 {
                panel(" Прогресс ")
            } else {
                Block::default().padding(Padding::horizontal(2))
            }),
        rows[1],
    );
    let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .spacing(1)
        .split(rows[2]);
    frame.render_widget(
        Paragraph::new(format!(
            "Помидоры     {} / {}\nФокус        {} мин\nСерия        {} дн.\nЗа 7 дней    {} пом.",
            s.stats.today_count,
            s.settings.daily_goal,
            s.stats.today_seconds / 60,
            s.stats.streak,
            s.stats.week_count
        ))
        .block(panel(" Сегодня ")),
        cols[0],
    );
    frame.render_widget(Paragraph::new(format!("Работа / отдых  {} / {} мин\nДлинный отдых   {} мин\nЭкран  {} · {:.0}%\nЗвук   {} · Тик {}", s.settings.focus_minutes, s.settings.break_minutes, s.settings.long_break_minutes, on(s.settings.dim_enabled), s.settings.dim_opacity * 100.0, on(s.settings.sound), on(s.settings.ticking))).block(panel(" Цикл ")), cols[1]);
}

fn clock_digits(time: &str) -> Vec<String> {
    const DIGITS: [[&str; 5]; 10] = [
        ["███", "█ █", "█ █", "█ █", "███"],
        [" █ ", "██ ", " █ ", " █ ", "███"],
        ["███", "  █", "███", "█  ", "███"],
        ["███", "  █", "███", "  █", "███"],
        ["█ █", "█ █", "███", "  █", "  █"],
        ["███", "█  ", "███", "  █", "███"],
        ["███", "█  ", "███", "█ █", "███"],
        ["███", "  █", "  █", "  █", "  █"],
        ["███", "█ █", "███", "█ █", "███"],
        ["███", "█ █", "███", "  █", "███"],
    ];
    (0..5)
        .map(|row| {
            time.chars()
                .map(|ch| {
                    if let Some(digit) = ch.to_digit(10) {
                        DIGITS[digit as usize][row]
                    } else if row == 1 || row == 3 {
                        " ▪ "
                    } else {
                        "   "
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

fn draw_shortcuts(frame: &mut Frame, area: Rect, ui: &Ui) {
    let lines = if matches!(ui.page, Page::Settings) {
        vec![
            Line::from(
                vec![
                    shortcut("↑↓", "Выбор"),
                    shortcut("←→", "Изменить"),
                    shortcut("Enter", "Сохранить"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            ),
            Line::from(
                vec![shortcut("Esc", "Отмена"), shortcut("0", "Выход")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
            ),
        ]
    } else {
        vec![
            Line::from(
                vec![
                    shortcut("1", "Помощь"),
                    shortcut(
                        "2",
                        if ui.state.running {
                            "Пауза"
                        } else {
                            "Старт"
                        },
                    ),
                    shortcut("3", "Пропуск"),
                    shortcut("4", "Стат."),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            ),
            Line::from(
                vec![
                    shortcut("5", "Настр."),
                    shortcut("6", "Экран"),
                    shortcut("7", "Звук"),
                    shortcut("8", "Тест"),
                    shortcut("0", "Выход"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>(),
            ),
        ]
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn shortcut<'a>(key: &str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default().fg(ACCENT).bg(SURFACE).bold(),
        ),
        Span::styled(format!(" {label}  "), Style::default().fg(MUTED)),
    ]
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
                    Style::default().fg(ACCENT).bg(BORDER).bold()
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

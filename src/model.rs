use chrono::{DateTime, Duration as CalendarDuration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub focus_minutes: u64,
    pub break_minutes: u64,
    pub long_break_minutes: u64,
    pub sessions_per_cycle: u32,
    pub daily_goal: u32,
    pub dim_enabled: bool,
    pub dim_opacity: f64,
    pub sound: bool,
    pub ticking: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            focus_minutes: 25,
            break_minutes: 5,
            long_break_minutes: 15,
            sessions_per_cycle: 4,
            daily_goal: 8,
            dim_enabled: true,
            dim_opacity: 0.45,
            sound: true,
            ticking: false,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=240).contains(&self.focus_minutes),
            "Фокус: от 1 до 240 минут"
        );
        anyhow::ensure!(
            (1..=120).contains(&self.break_minutes),
            "Перерыв: от 1 до 120 минут"
        );
        anyhow::ensure!(
            (1..=120).contains(&self.long_break_minutes),
            "Длинный перерыв: от 1 до 120 минут"
        );
        anyhow::ensure!(
            (1..=12).contains(&self.sessions_per_cycle),
            "Цикл: от 1 до 12 сессий"
        );
        anyhow::ensure!(
            (1..=100).contains(&self.daily_goal),
            "Цель: от 1 до 100 помидоров"
        );
        anyhow::ensure!(
            self.dim_opacity.is_finite() && (0.1..=0.85).contains(&self.dim_opacity),
            "Затемнение: от 10 до 85%"
        );
        Ok(())
    }

    pub fn duration(&self, phase: Phase) -> Duration {
        Duration::from_secs(
            60 * match phase {
                Phase::Focus => self.focus_minutes,
                Phase::Break => self.break_minutes,
                Phase::LongBreak => self.long_break_minutes,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Focus,
    Break,
    LongBreak,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Focus => "Фокус",
            Self::Break => "Перерыв",
            Self::LongBreak => "Длинный перерыв",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub completed_at: DateTime<Local>,
    pub seconds: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub today_count: u32,
    pub today_seconds: u64,
    pub week_count: u32,
    pub week_seconds: u64,
    pub streak: u32,
    pub recent: Vec<Session>,
}

pub fn statistics(sessions: &[Session], today: NaiveDate, goal: u32) -> Stats {
    let mut stats = Stats::default();
    let week_start = today - CalendarDuration::days(6);
    let mut days = std::collections::BTreeMap::<NaiveDate, u32>::new();
    for session in sessions {
        let day = session.completed_at.date_naive();
        *days.entry(day).or_default() += 1;
        if day == today {
            stats.today_count += 1;
            stats.today_seconds += session.seconds;
        }
        if day >= week_start && day <= today {
            stats.week_count += 1;
            stats.week_seconds += session.seconds;
        }
    }
    let mut day = today;
    // An unfinished today doesn't erase the streak earned through yesterday.
    if days.get(&day).copied().unwrap_or(0) < goal {
        day -= CalendarDuration::days(1);
    }
    while days.get(&day).copied().unwrap_or(0) >= goal {
        stats.streak += 1;
        day -= CalendarDuration::days(1);
    }
    stats.recent = sessions.iter().rev().take(30).cloned().collect();
    stats
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub phase: Phase,
    pub running: bool,
    pub remaining_secs: u64,
    pub total_secs: u64,
    pub cycle_completed: u32,
    pub settings: Settings,
    pub stats: Stats,
    pub message: String,
}

pub struct Timer {
    pub phase: Phase,
    pub running: bool,
    pub remaining: Duration,
    pub total: Duration,
    elapsed: Duration,
    pub cycle_completed: u32,
}

impl Timer {
    pub fn new(settings: &Settings) -> Self {
        let total = settings.duration(Phase::Focus);
        Self {
            phase: Phase::Focus,
            running: false,
            remaining: total,
            total,
            elapsed: Duration::ZERO,
            cycle_completed: 0,
        }
    }

    pub fn toggle(&mut self) {
        self.running = !self.running;
    }

    /// Returns a completed focus duration exactly once at a natural boundary.
    pub fn advance(&mut self, elapsed: Duration, settings: &Settings) -> (bool, Option<u64>) {
        if !self.running {
            return (false, None);
        }
        self.elapsed += elapsed.min(self.remaining);
        if elapsed < self.remaining {
            self.remaining -= elapsed;
            return (false, None);
        }
        let completed = if self.phase == Phase::Focus {
            self.cycle_completed += 1;
            Some(self.elapsed.as_secs())
        } else {
            None
        };
        self.next(settings);
        // Discard overshoot rather than inventing sessions after a delayed event loop.
        (true, completed)
    }

    pub fn next(&mut self, settings: &Settings) {
        self.phase = match self.phase {
            Phase::Focus if self.cycle_completed >= settings.sessions_per_cycle => Phase::LongBreak,
            Phase::Focus => Phase::Break,
            Phase::Break => Phase::Focus,
            Phase::LongBreak => {
                self.cycle_completed = 0;
                Phase::Focus
            }
        };
        self.total = settings.duration(self.phase);
        self.remaining = self.total;
        self.elapsed = Duration::ZERO;
    }

    pub fn reconfigure(&mut self, settings: &Settings) {
        self.total = settings.duration(self.phase);
        self.remaining = self
            .total
            .saturating_sub(self.elapsed)
            .max(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_cycle_and_skips() {
        let settings = Settings::default();
        let mut timer = Timer::new(&settings);
        timer.toggle();
        for i in 1..=4 {
            assert_eq!(
                timer.advance(Duration::from_secs(1500), &settings),
                (true, Some(1500))
            );
            assert_eq!(timer.cycle_completed, i);
            assert_eq!(
                timer.phase,
                if i == 4 {
                    Phase::LongBreak
                } else {
                    Phase::Break
                }
            );
            let rest = timer.remaining;
            assert_eq!(timer.advance(rest, &settings), (true, None));
        }
        assert_eq!(timer.cycle_completed, 0);
        timer.next(&settings);
        assert_eq!(timer.cycle_completed, 0);
    }
    #[test]
    fn pause_preserves_remaining_and_boundary_is_once() {
        let settings = Settings::default();
        let mut timer = Timer::new(&settings);
        timer.advance(Duration::from_secs(600), &settings);
        assert_eq!(timer.remaining.as_secs(), 1500);
        timer.toggle();
        timer.advance(Duration::from_secs(600), &settings);
        timer.toggle();
        timer.advance(Duration::from_secs(500), &settings);
        assert_eq!(timer.remaining.as_secs(), 900);
        timer.toggle();
        assert_eq!(
            timer.advance(Duration::from_secs(900), &settings),
            (true, Some(1500))
        );
        assert_eq!(timer.advance(Duration::ZERO, &settings), (false, None));
    }
    #[test]
    fn shorter_settings_preserve_elapsed_without_underflow() {
        let mut settings = Settings::default();
        let mut timer = Timer::new(&settings);
        timer.toggle();
        timer.advance(Duration::from_secs(600), &settings);
        settings.focus_minutes = 20;
        timer.reconfigure(&settings);
        assert_eq!(timer.remaining.as_secs(), 600);
        settings.focus_minutes = 5;
        timer.reconfigure(&settings);
        assert_eq!(timer.remaining.as_secs(), 1);
        settings.focus_minutes = 25;
        timer.reconfigure(&settings);
        assert_eq!(timer.remaining.as_secs(), 900);
        settings.focus_minutes = 5;
        timer.reconfigure(&settings);
        assert_eq!(
            timer.advance(Duration::from_secs(1), &settings),
            (true, Some(601))
        );
    }

    #[test]
    fn streak_uses_goals_and_calendar_days() {
        use chrono::TimeZone;
        let today = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let make = |day| Session {
            completed_at: Local.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap(),
            seconds: 1500,
        };
        let sessions = vec![make(4), make(4), make(5), make(5), make(6)];
        let stats = statistics(&sessions, today, 2);
        assert_eq!(stats.streak, 2);
        assert_eq!(stats.today_count, 1);
        assert_eq!(stats.week_seconds, 7500);
    }
}

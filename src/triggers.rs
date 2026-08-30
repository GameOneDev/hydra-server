//! What makes a task run.
//!
//! A timer is only the most common answer. A task carries a list of triggers
//! and any one of them can start it, so "every night at three" and "whenever
//! the abandoned uploads pile up" are the same kind of thing to the scheduler
//! and to the operator editing them.
//!
//! Triggers are stored as JSON on the task's row: they are always read and
//! written whole, and each kind carries different fields. Unknown kinds — a
//! schedule written by a newer build — are dropped on load rather than
//! failing it, so a downgrade costs a trigger and not the whole screen.

use crate::state::AppState;
use chrono::{DateTime, Datelike, Duration, Months, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MINUTES_PER_DAY: i64 = 24 * 60;

/// The shortest gap a timer may be set to. Below this the tick that looks for
/// due work becomes the thing that decides the cadence.
pub const MIN_GAP_MINUTES: i64 = 5;

/// The most triggers one task may carry. Not a technical limit — a task whose
/// rules don't fit on a screen is one nobody can reason about.
pub const MAX_TRIGGERS: usize = 8;

/// The unit an interval is counted in. Months and weeks are calendar steps,
/// not fixed multiples of a day, so "every month on the 1st" stays on the 1st.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Unit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl Unit {
    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Minute => "minute",
            Unit::Hour => "hour",
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
        }
    }

    /// Whether a run of this cadence lands on a time of day. A cadence shorter
    /// than a day just runs every interval.
    pub fn has_time_of_day(self) -> bool {
        matches!(self, Unit::Day | Unit::Week | Unit::Month)
    }

    /// Roughly how long one is, for sorting and for the "too often" guard.
    /// Approximate for months on purpose: it is a floor, not a calendar.
    fn minutes(self) -> i64 {
        match self {
            Unit::Minute => 1,
            Unit::Hour => 60,
            Unit::Day => MINUTES_PER_DAY,
            Unit::Week => 7 * MINUTES_PER_DAY,
            Unit::Month => 28 * MINUTES_PER_DAY,
        }
    }

    fn plural(self, count: i64) -> String {
        match count {
            1 => format!("every {}", self.as_str()),
            n => format!("every {n} {}s", self.as_str()),
        }
    }
}

/// A number the server can measure about itself, cheaply, on every tick.
///
/// Each one is a reason some job exists: uploads that were abandoned, games
/// nothing could name, history past its window, a database that has grown, a
/// volume running out. Turning them into triggers is what lets a task run
/// because the server needs it rather than because a clock came round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Metric {
    PendingUploads,
    UnresolvedGames,
    ExpiredEvents,
    DatabaseBytes,
    FreeDiskBytes,
}

impl Metric {
    pub const ALL: &'static [Metric] = &[
        Metric::PendingUploads,
        Metric::UnresolvedGames,
        Metric::ExpiredEvents,
        Metric::DatabaseBytes,
        Metric::FreeDiskBytes,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Metric::PendingUploads => "pendingUploads",
            Metric::UnresolvedGames => "unresolvedGames",
            Metric::ExpiredEvents => "expiredEvents",
            Metric::DatabaseBytes => "databaseBytes",
            Metric::FreeDiskBytes => "freeDiskBytes",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Metric::PendingUploads => "abandoned uploads",
            Metric::UnresolvedGames => "games with no name",
            Metric::ExpiredEvents => "events past the retention window",
            Metric::DatabaseBytes => "database size",
            Metric::FreeDiskBytes => "free disk space",
        }
    }

    /// Bytes are shown and entered as sizes; the rest are plain counts.
    pub fn is_bytes(self) -> bool {
        matches!(self, Metric::DatabaseBytes | Metric::FreeDiskBytes)
    }

    /// The comparison that makes sense for it, so the editor opens on the one
    /// an operator meant: disk space is a floor, everything else a ceiling.
    pub fn natural_comparison(self) -> Comparison {
        match self {
            Metric::FreeDiskBytes => Comparison::Below,
            _ => Comparison::Above,
        }
    }

    /// What this number is right now.
    pub async fn measure(self, state: &AppState) -> i64 {
        match self {
            Metric::PendingUploads => {
                let cutoff =
                    (Utc::now() - Duration::hours(crate::jobs::PENDING_TTL_HOURS)).to_rfc3339();
                let snapshots: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM cloud_save_snapshots
                      WHERE status = 'pending' AND created_at < ?",
                )
                .bind(&cutoff)
                .fetch_one(&state.pool)
                .await
                .unwrap_or(0);
                let souvenirs: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM souvenirs WHERE status = 'pending' AND created_at < ?",
                )
                .bind(&cutoff)
                .fetch_one(&state.pool)
                .await
                .unwrap_or(0);
                snapshots + souvenirs
            }
            Metric::UnresolvedGames => sqlx::query_scalar(
                "SELECT COUNT(*) FROM (
                     SELECT DISTINCT t.shop, t.object_id FROM (
                         SELECT shop, object_id FROM cloud_save_snapshots
                         UNION SELECT shop, object_id FROM artifacts
                         UNION SELECT shop, object_id FROM playtime_daily
                         UNION SELECT shop, object_id FROM game_artwork
                     ) t
                     LEFT JOIN game_metadata g
                            ON g.shop = t.shop AND g.object_id = t.object_id
                     WHERE g.name IS NULL
                 )",
            )
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0),
            Metric::ExpiredEvents => {
                let cutoff = (Utc::now()
                    - Duration::days(state.config.event_retention_days.max(1)))
                .to_rfc3339();
                sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE at < ?")
                    .bind(&cutoff)
                    .fetch_one(&state.pool)
                    .await
                    .unwrap_or(0)
            }
            Metric::DatabaseBytes => crate::jobs::database_bytes(state).await as i64,
            Metric::FreeDiskBytes => {
                crate::backup::free_disk_bytes(&state.config.data_dir).unwrap_or(0) as i64
            }
        }
    }

    pub fn json(self) -> Value {
        json!({
            "metric": self.as_str(),
            "label": self.label(),
            "bytes": self.is_bytes(),
            "comparison": self.natural_comparison().as_str(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Comparison {
    Above,
    Below,
}

impl Comparison {
    pub fn as_str(self) -> &'static str {
        match self {
            Comparison::Above => "above",
            Comparison::Below => "below",
        }
    }

    fn holds(self, measured: i64, threshold: i64) -> bool {
        match self {
            Comparison::Above => measured > threshold,
            Comparison::Below => measured < threshold,
        }
    }
}

/// One reason a task may run. Any trigger firing runs the task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
/* `rename_all_fields` as well as `rename_all`: the variant names are the
   `type` tag the panel switches on, and the fields inside them are the ones it
   sends back. Without it a time of day would arrive as `atMinute`, match no
   field, and be silently dropped.
   Unknown fields are ignored rather than refused, because the panel edits a
   trigger by sending back the object it was given — `label` and all. */
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Trigger {
    /// A timer: every `count` `unit`s, landing on `at_minute` (UTC) for
    /// cadences of a day or more, on `weekday` for weeks and on `day` of the
    /// month for months.
    Every {
        count: i64,
        unit: Unit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_minute: Option<i64>,
        /// 0 = Monday.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weekday: Option<i64>,
        /// 1–31, clamped to the length of the month.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        day: Option<i64>,
    },
    /// Once, shortly after the server starts.
    Startup {
        #[serde(default)]
        delay_minutes: i64,
    },
    /// When another task finishes successfully.
    AfterTask {
        task: String,
        #[serde(default)]
        delay_minutes: i64,
    },
    /// When an event of one of these kinds is recorded. Prefixes, so
    /// `cloud_save.` keeps matching a kind added later.
    OnEvent {
        kinds: Vec<String>,
        #[serde(default)]
        min_gap_minutes: i64,
    },
    /// While a measured number is over (or under) a line.
    Condition {
        metric: Metric,
        comparison: Comparison,
        value: i64,
        #[serde(default)]
        min_gap_minutes: i64,
    },
}

impl Trigger {
    /// The phrase the panel prints for this trigger.
    pub fn label(&self) -> String {
        match self {
            Trigger::Every {
                count,
                unit,
                at_minute,
                weekday,
                day,
            } => {
                let mut label = unit.plural(*count);
                if *unit == Unit::Week {
                    label.push_str(&format!(" on {}", weekday_name(weekday.unwrap_or(0))));
                }
                if *unit == Unit::Month {
                    label.push_str(&format!(" on the {}", ordinal(day.unwrap_or(1))));
                }
                if unit.has_time_of_day() {
                    label.push_str(&format!(" at {} UTC", clock(at_minute.unwrap_or(0))));
                }
                label
            }
            Trigger::Startup { delay_minutes } => match delay_minutes {
                0 => "when the server starts".to_string(),
                n => format!("{n} min after the server starts"),
            },
            Trigger::AfterTask {
                task,
                delay_minutes,
            } => {
                let title = crate::jobs::find(task).map_or(task.as_str(), |job| job.title);
                match delay_minutes {
                    0 => format!("after {title}"),
                    n => format!("{n} min after {title}"),
                }
            }
            Trigger::OnEvent { kinds, .. } => match kinds.len() {
                0 => "when anything is recorded".to_string(),
                1 => format!("when {} happens", kinds[0]),
                n => format!("when any of {n} event kinds happen"),
            },
            Trigger::Condition {
                metric,
                comparison,
                value,
                ..
            } => format!(
                "when {} is {} {}",
                metric.label(),
                comparison.as_str(),
                if metric.is_bytes() {
                    bytes_label(*value)
                } else {
                    value.to_string()
                }
            ),
        }
    }

    /// Rejects a trigger that couldn't do what it says: a cadence faster than
    /// the tick, a weekday that isn't one, a task that doesn't exist.
    pub fn validate(&self, owner: &str) -> Result<(), String> {
        match self {
            Trigger::Every {
                count,
                unit,
                at_minute,
                weekday,
                day,
            } => {
                if *count < 1 {
                    return Err("an interval has to be at least 1".to_string());
                }
                if count.saturating_mul(unit.minutes()) < MIN_GAP_MINUTES {
                    return Err(format!(
                        "the shortest interval this server runs is {MIN_GAP_MINUTES} minutes"
                    ));
                }
                if count.saturating_mul(unit.minutes()) > 366 * MINUTES_PER_DAY {
                    return Err("that interval is longer than a year".to_string());
                }
                if at_minute.is_some_and(|minute| !(0..MINUTES_PER_DAY).contains(&minute)) {
                    return Err("the run time has to be a minute of the day".to_string());
                }
                if weekday.is_some_and(|weekday| !(0..7).contains(&weekday)) {
                    return Err("that isn't a day of the week".to_string());
                }
                if day.is_some_and(|day| !(1..=31).contains(&day)) {
                    return Err("that isn't a day of the month".to_string());
                }
                Ok(())
            }
            Trigger::Startup { delay_minutes } => bounded_delay(*delay_minutes),
            Trigger::AfterTask {
                task,
                delay_minutes,
            } => {
                if task == owner {
                    return Err("a task can't wait for itself".to_string());
                }
                if !crate::jobs::find(task).is_some_and(|job| job.schedulable) {
                    return Err(format!("there is no task called {task}"));
                }
                bounded_delay(*delay_minutes)
            }
            Trigger::OnEvent {
                kinds,
                min_gap_minutes,
            } => {
                if kinds.len() > 12 {
                    return Err("that is more event kinds than one trigger should carry".to_string());
                }
                if kinds.iter().any(|kind| kind.trim().is_empty()) {
                    return Err("an event kind can't be blank".to_string());
                }
                bounded_delay(*min_gap_minutes)
            }
            Trigger::Condition {
                value,
                min_gap_minutes,
                ..
            } => {
                if *value < 0 {
                    return Err("a threshold can't be negative".to_string());
                }
                bounded_delay(*min_gap_minutes)
            }
        }
    }

    /// When this trigger next fires on its own clock. `None` for the ones that
    /// wait for something to happen instead.
    pub fn next_run(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let Trigger::Every {
            count,
            unit,
            at_minute,
            weekday,
            day,
        } = self
        else {
            return None;
        };

        let count = (*count).max(1);
        Some(match unit {
            Unit::Minute => from + Duration::minutes(count),
            Unit::Hour => from + Duration::hours(count),
            Unit::Day => step_days(from, count, at_minute.unwrap_or(0)),
            Unit::Week => step_weeks(from, count, weekday.unwrap_or(0), at_minute.unwrap_or(0)),
            Unit::Month => step_months(from, count, day.unwrap_or(1), at_minute.unwrap_or(0)),
        })
    }

    /// The trigger as the panel reads it: its own fields, tagged with the
    /// kind (`type`), plus the phrase to print for it.
    pub fn json(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        value["label"] = json!(self.label());
        value
    }
}

fn bounded_delay(minutes: i64) -> Result<(), String> {
    if !(0..=7 * MINUTES_PER_DAY).contains(&minutes) {
        return Err("that delay is longer than a week".to_string());
    }
    Ok(())
}

/// The next `at_minute` that is a whole number of `count` days away, counting
/// from the day `from` falls in.
fn step_days(from: DateTime<Utc>, count: i64, at_minute: i64) -> DateTime<Utc> {
    let mut candidate = midnight(from) + Duration::minutes(at_minute.rem_euclid(MINUTES_PER_DAY));
    while candidate <= from {
        candidate += Duration::days(count);
    }
    candidate
}

/// The next `weekday` at `at_minute`, stepping whole `count`-week blocks.
fn step_weeks(from: DateTime<Utc>, count: i64, weekday: i64, at_minute: i64) -> DateTime<Utc> {
    let target = weekday.rem_euclid(7);
    let today = from.weekday().num_days_from_monday() as i64;
    let ahead = (target - today).rem_euclid(7);

    let mut candidate = midnight(from)
        + Duration::days(ahead)
        + Duration::minutes(at_minute.rem_euclid(MINUTES_PER_DAY));
    while candidate <= from {
        candidate += Duration::weeks(count);
    }
    candidate
}

/// The next `day` of a month at `at_minute`, stepping whole calendar months.
/// A day past the end of a short month lands on its last day rather than
/// skipping it — "the 31st" in February is the 28th.
fn step_months(from: DateTime<Utc>, count: i64, day: i64, at_minute: i64) -> DateTime<Utc> {
    let months = Months::new(count.clamp(1, 120) as u32);
    let mut anchor = midnight(from);

    for _ in 0..=13 {
        let candidate = on_day(anchor, day) + Duration::minutes(at_minute.rem_euclid(MINUTES_PER_DAY));
        if candidate > from {
            return candidate;
        }
        anchor = anchor.checked_add_months(months).unwrap_or(anchor + Duration::days(30));
    }

    from + Duration::days(30)
}

/// `anchor`'s month, on `day` — clamped to the month's length.
fn on_day(anchor: DateTime<Utc>, day: i64) -> DateTime<Utc> {
    let first = Utc
        .with_ymd_and_hms(anchor.year(), anchor.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(anchor);
    let length = days_in_month(anchor.year(), anchor.month());
    first + Duration::days(day.clamp(1, length) - 1)
}

fn days_in_month(year: i32, month: u32) -> i64 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single();
    let next = Utc.with_ymd_and_hms(next_year, next_month, 1, 0, 0, 0).single();
    match (first, next) {
        (Some(first), Some(next)) => (next - first).num_days(),
        _ => 30,
    }
}

fn midnight(at: DateTime<Utc>) -> DateTime<Utc> {
    at.with_hour(0)
        .and_then(|at| at.with_minute(0))
        .and_then(|at| at.with_second(0))
        .and_then(|at| at.with_nanosecond(0))
        .unwrap_or(at)
}

pub fn weekday_name(weekday: i64) -> &'static str {
    match weekday.rem_euclid(7) {
        0 => "Monday",
        1 => "Tuesday",
        2 => "Wednesday",
        3 => "Thursday",
        4 => "Friday",
        5 => "Saturday",
        _ => "Sunday",
    }
}

fn ordinal(day: i64) -> String {
    let suffix = match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

/// A minute of the day as `HH:MM`.
pub fn clock(at_minute: i64) -> String {
    let at = at_minute.rem_euclid(MINUTES_PER_DAY);
    format!("{:02}:{:02}", at / 60, at % 60)
}

/// Sizes the way the panel prints them, for a threshold in a sentence.
fn bytes_label(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value.fract() == 0.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Whether `comparison` of `metric` against `value` holds right now.
pub async fn condition_holds(
    state: &AppState,
    metric: Metric,
    comparison: Comparison,
    value: i64,
) -> Option<i64> {
    let measured = metric.measure(state).await;
    comparison.holds(measured, value).then_some(measured)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(time: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(time)
            .expect("a timestamp")
            .with_timezone(&Utc)
    }

    fn every(count: i64, unit: Unit, at_minute: Option<i64>) -> Trigger {
        Trigger::Every {
            count,
            unit,
            at_minute,
            weekday: None,
            day: None,
        }
    }

    /// "Every day at 03:00" has to mean 03:00 — not "24 hours after whenever
    /// the last run finished", which drifts an hour further into the morning
    /// every time a run takes a minute.
    #[test]
    fn a_daily_timer_lands_on_its_time_of_day() {
        let daily = every(1, Unit::Day, Some(180));

        assert_eq!(
            daily.next_run(at("2026-08-30T14:20:00Z")),
            Some(at("2026-08-31T03:00:00Z"))
        );
        /* Before today's slot: today, not tomorrow. */
        assert_eq!(
            daily.next_run(at("2026-08-30T02:59:00Z")),
            Some(at("2026-08-30T03:00:00Z"))
        );
        /* Exactly on it: the next one, so a run can never re-trigger itself. */
        assert_eq!(
            daily.next_run(at("2026-08-30T03:00:00Z")),
            Some(at("2026-08-31T03:00:00Z"))
        );
        /* Every two days steps two, from the day it is asked on. */
        assert_eq!(
            every(2, Unit::Day, Some(180)).next_run(at("2026-08-30T14:00:00Z")),
            Some(at("2026-09-01T03:00:00Z"))
        );
    }

    /// A cadence shorter than a day has no time of day to land on, so it runs
    /// an interval from now.
    #[test]
    fn a_short_timer_runs_an_interval_from_now() {
        assert_eq!(
            every(90, Unit::Minute, None).next_run(at("2026-08-30T14:20:00Z")),
            Some(at("2026-08-30T15:50:00Z"))
        );
        assert_eq!(
            every(6, Unit::Hour, Some(180)).next_run(at("2026-08-30T14:20:00Z")),
            Some(at("2026-08-30T20:20:00Z")),
            "a time of day is ignored for a cadence that isn't whole days"
        );
    }

    /// A weekly timer lands on the weekday it names, and steps whole weeks
    /// from there.
    #[test]
    fn a_weekly_timer_lands_on_its_weekday() {
        /* 2026-08-30 is a Sunday; 6 is Sunday counting from Monday. */
        let sunday = Trigger::Every {
            count: 1,
            unit: Unit::Week,
            at_minute: Some(4 * 60 + 30),
            weekday: Some(6),
            day: None,
        };

        assert_eq!(
            sunday.next_run(at("2026-08-30T02:00:00Z")),
            Some(at("2026-08-30T04:30:00Z")),
            "earlier the same Sunday"
        );
        assert_eq!(
            sunday.next_run(at("2026-08-30T06:00:00Z")),
            Some(at("2026-09-06T04:30:00Z")),
            "past it, so the next Sunday"
        );
        assert_eq!(sunday.label(), "every week on Sunday at 04:30 UTC");
    }

    /// A monthly timer steps calendar months, and a day past the end of a
    /// short one lands on its last day rather than skipping the month.
    #[test]
    fn a_monthly_timer_clamps_to_the_month() {
        let thirty_first = Trigger::Every {
            count: 1,
            unit: Unit::Month,
            at_minute: Some(120),
            weekday: None,
            day: Some(31),
        };

        assert_eq!(
            thirty_first.next_run(at("2026-01-31T05:00:00Z")),
            Some(at("2026-02-28T02:00:00Z")),
            "February has no 31st"
        );
        assert_eq!(
            thirty_first.next_run(at("2026-03-01T00:00:00Z")),
            Some(at("2026-03-31T02:00:00Z"))
        );

        let quarterly = Trigger::Every {
            count: 3,
            unit: Unit::Month,
            at_minute: Some(0),
            weekday: None,
            day: Some(1),
        };
        assert_eq!(
            quarterly.next_run(at("2026-08-30T12:00:00Z")),
            Some(at("2026-11-01T00:00:00Z"))
        );
        assert_eq!(quarterly.label(), "every 3 months on the 1st at 00:00 UTC");
    }

    /// The triggers that wait for something have no clock of their own; the
    /// scheduler asks them each tick instead.
    #[test]
    fn only_timers_have_a_next_run() {
        let now = at("2026-08-30T14:20:00Z");

        assert!(Trigger::Startup { delay_minutes: 5 }.next_run(now).is_none());
        assert!(Trigger::AfterTask {
            task: "backup".to_string(),
            delay_minutes: 0
        }
        .next_run(now)
        .is_none());
        assert!(Trigger::OnEvent {
            kinds: vec!["cloud_save.".to_string()],
            min_gap_minutes: 60
        }
        .next_run(now)
        .is_none());
    }

    /// Every trigger has to say what it does in one line — it is the only
    /// thing the list screen shows.
    #[test]
    fn every_trigger_reads_as_a_sentence() {
        assert_eq!(every(1, Unit::Day, Some(180)).label(), "every day at 03:00 UTC");
        assert_eq!(every(6, Unit::Hour, None).label(), "every 6 hours");
        assert_eq!(every(30, Unit::Minute, None).label(), "every 30 minutes");
        assert_eq!(
            Trigger::Startup { delay_minutes: 5 }.label(),
            "5 min after the server starts"
        );
        assert_eq!(
            Trigger::AfterTask {
                task: "backup".to_string(),
                delay_minutes: 0
            }
            .label(),
            "after Back up the database"
        );
        assert_eq!(
            Trigger::OnEvent {
                kinds: vec!["cloud_save.".to_string()],
                min_gap_minutes: 60
            }
            .label(),
            "when cloud_save. happens"
        );
        assert_eq!(
            Trigger::Condition {
                metric: Metric::FreeDiskBytes,
                comparison: Comparison::Below,
                value: 5 * 1024 * 1024 * 1024,
                min_gap_minutes: 60,
            }
            .label(),
            "when free disk space is below 5 GB"
        );
    }

    /// A trigger that couldn't do what it says is refused when it is saved,
    /// not discovered when it fires.
    #[test]
    fn a_trigger_that_cannot_work_is_refused() {
        assert!(every(1, Unit::Minute, None).validate("vacuum").is_err(), "faster than the tick");
        assert!(every(0, Unit::Day, None).validate("vacuum").is_err());
        assert!(every(400, Unit::Day, None).validate("vacuum").is_err());
        assert!(every(1, Unit::Day, Some(1500)).validate("vacuum").is_err());
        assert!(every(15, Unit::Minute, None).validate("vacuum").is_ok());

        assert!(Trigger::AfterTask {
            task: "vacuum".to_string(),
            delay_minutes: 0
        }
        .validate("vacuum")
        .is_err(), "a task can't wait for itself");
        assert!(Trigger::AfterTask {
            task: "no-such-task".to_string(),
            delay_minutes: 0
        }
        .validate("vacuum")
        .is_err());
        assert!(Trigger::AfterTask {
            task: "delete-orphan-files".to_string(),
            delay_minutes: 0
        }
        .validate("vacuum")
        .is_err(), "and can't wait for one that never runs unattended");

        assert!(Trigger::Condition {
            metric: Metric::ExpiredEvents,
            comparison: Comparison::Above,
            value: -1,
            min_gap_minutes: 60,
        }
        .validate("prune-events")
        .is_err());
    }

    /// The stored shape is the wire shape: a trigger written by the panel has
    /// to come back as the same trigger.
    #[test]
    fn triggers_round_trip_through_json() {
        let triggers = vec![
            every(2, Unit::Day, Some(180)),
            Trigger::Startup { delay_minutes: 5 },
            Trigger::Condition {
                metric: Metric::PendingUploads,
                comparison: Comparison::Above,
                value: 20,
                min_gap_minutes: 120,
            },
        ];

        let encoded = serde_json::to_string(&triggers).expect("serialisable");
        assert!(encoded.contains("\"type\":\"every\""));
        assert!(encoded.contains("\"unit\":\"day\""));
        assert!(encoded.contains("\"metric\":\"pendingUploads\""));

        let decoded: Vec<Trigger> = serde_json::from_str(&encoded).expect("readable");
        assert_eq!(decoded, triggers);

        /* The panel edits a trigger by sending back what it was given, which
           carries the rendered label and camelCase fields. Both have to
           survive the trip, or a saved run time would quietly become 00:00. */
        let from_panel: Trigger = serde_json::from_value(json!({
            "type": "every",
            "count": 2,
            "unit": "day",
            "atMinute": 180,
            "label": "every 2 days at 03:00 UTC",
        }))
        .expect("what the panel sends");
        assert_eq!(from_panel, every(2, Unit::Day, Some(180)));

        let condition: Trigger = serde_json::from_value(json!({
            "type": "condition",
            "metric": "freeDiskBytes",
            "comparison": "below",
            "value": 5_368_709_120i64,
            "minGapMinutes": 120,
            "label": "when free disk space is below 5 GB",
        }))
        .expect("what the panel sends");
        assert!(matches!(
            condition,
            Trigger::Condition {
                metric: Metric::FreeDiskBytes,
                comparison: Comparison::Below,
                min_gap_minutes: 120,
                ..
            }
        ));
    }
}

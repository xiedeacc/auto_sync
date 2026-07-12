use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
    Weekday,
};

use crate::core::config::{ScheduleConfig, ScheduleMode, parse_schedule_time};

pub fn cycle_is_due(starts_at: DateTime<Utc>, now: DateTime<Utc>, cfg: &ScheduleConfig) -> bool {
    match next_boundary_after(starts_at, cfg) {
        Ok(boundary) => now >= boundary,
        Err(_) => now.signed_duration_since(starts_at) >= Duration::days(1),
    }
}

pub fn next_boundary_after(
    starts_at: DateTime<Utc>,
    cfg: &ScheduleConfig,
) -> Result<DateTime<Utc>> {
    let start_local = starts_at.with_timezone(&Local);

    match cfg.mode {
        ScheduleMode::Realtime => Ok(starts_at),
        ScheduleMode::Daily => {
            let time = schedule_time(cfg)?;
            let mut date = start_local.date_naive();
            loop {
                let candidate = local_datetime(date, time)?;
                if candidate > start_local {
                    return Ok(candidate.with_timezone(&Utc));
                }
                date = date
                    .succ_opt()
                    .context("failed to calculate next schedule date")?;
            }
        }
        ScheduleMode::Weekly => {
            let time = schedule_time(cfg)?;
            let wanted = parse_weekday(cfg.weekday.as_deref().unwrap_or("monday"))?;
            let anchor = schedule_anchor(cfg)?;
            let every = cfg.every_weeks.max(1) as i64;
            let mut date = start_local.date_naive();
            for _ in 0..(every * 7 + 8) {
                if date.weekday() == wanted && week_on_cadence(anchor, date, every) {
                    let candidate = local_datetime(date, time)?;
                    if candidate > start_local {
                        return Ok(candidate.with_timezone(&Utc));
                    }
                }
                date = date
                    .succ_opt()
                    .context("failed to calculate next weekly schedule date")?;
            }
            bail!("failed to calculate weekly schedule boundary")
        }
    }
}

fn schedule_time(cfg: &ScheduleConfig) -> Result<NaiveTime> {
    let (hour, minute, second) = parse_schedule_time(&cfg.time)?;
    NaiveTime::from_hms_opt(hour, minute, second)
        .with_context(|| format!("invalid schedule time {}", cfg.time))
}

fn schedule_anchor(cfg: &ScheduleConfig) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(&cfg.anchor_date, "%Y-%m-%d")
        .with_context(|| format!("invalid schedule anchor_date {}", cfg.anchor_date))
}

fn week_on_cadence(anchor: NaiveDate, date: NaiveDate, every: i64) -> bool {
    if every <= 1 {
        return true;
    }
    let days = (date - anchor).num_days();
    let week = days.div_euclid(7);
    week.rem_euclid(every) == 0
}

fn local_datetime(date: NaiveDate, time: NaiveTime) -> Result<DateTime<Local>> {
    let naive = NaiveDateTime::new(date, time);
    Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .context("failed to resolve local schedule time")
}

fn parse_weekday(value: &str) -> Result<Weekday> {
    match value.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => bail!("invalid weekday: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(mode: ScheduleMode, time: &str, weekday: Option<&str>) -> ScheduleConfig {
        ScheduleConfig {
            mode,
            time: time.to_string(),
            weekday: weekday.map(str::to_string),
            ..ScheduleConfig::default()
        }
    }

    /// Boundaries are computed in the machine's LOCAL timezone; building the
    /// expectations through the same `Local` conversion keeps these tests
    /// correct in any timezone the suite runs in.
    fn local_utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("unambiguous local time")
            .with_timezone(&Utc)
    }

    #[test]
    fn daily_boundary_is_the_next_occurrence_after_start() {
        let cfg = schedule(ScheduleMode::Daily, "19:00", None);
        // Start before today's slot → today's slot.
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 15, 10, 0), &cfg).unwrap(),
            local_utc(2026, 6, 15, 19, 0)
        );
        // Start after today's slot → tomorrow's slot.
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 15, 20, 0), &cfg).unwrap(),
            local_utc(2026, 6, 16, 19, 0)
        );
        // Start exactly AT the slot → strictly after, so the next day.
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 15, 19, 0), &cfg).unwrap(),
            local_utc(2026, 6, 16, 19, 0)
        );
    }

    #[test]
    fn weekly_boundary_lands_on_the_configured_weekday() {
        let cfg = schedule(ScheduleMode::Weekly, "19:00", Some("sat"));
        // 2026-06-15 is a Monday; the next Saturday is 2026-06-20.
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 15, 10, 0), &cfg).unwrap(),
            local_utc(2026, 6, 20, 19, 0)
        );
        // Start ON Saturday past the slot → the following Saturday.
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 20, 19, 30), &cfg).unwrap(),
            local_utc(2026, 6, 27, 19, 0)
        );
        // Full weekday names accepted too.
        let cfg = schedule(ScheduleMode::Weekly, "07:30", Some("Sunday"));
        assert_eq!(
            next_boundary_after(local_utc(2026, 6, 15, 10, 0), &cfg).unwrap(),
            local_utc(2026, 6, 21, 7, 30)
        );
    }

    #[test]
    fn weekly_boundary_honors_multi_week_cadence() {
        let mut cfg = schedule(ScheduleMode::Weekly, "19:00", Some("sat"));
        cfg.every_weeks = 4;
        cfg.anchor_date = "2026-07-04".to_string();

        // 2026-07-04 is the anchor Saturday; 2026-07-11 is the same weekday but
        // not a cadence week. The next allowed boundary is 2026-08-01.
        assert_eq!(
            next_boundary_after(local_utc(2026, 7, 11, 9, 0), &cfg).unwrap(),
            local_utc(2026, 8, 1, 19, 0)
        );

        assert!(!cycle_is_due(
            local_utc(2026, 7, 11, 9, 0),
            local_utc(2026, 7, 11, 19, 0),
            &cfg
        ));
        assert!(cycle_is_due(
            local_utc(2026, 7, 11, 9, 0),
            local_utc(2026, 8, 1, 19, 0),
            &cfg
        ));
    }

    #[test]
    fn realtime_boundary_is_the_start_itself() {
        let cfg = schedule(ScheduleMode::Realtime, "", None);
        let start = local_utc(2026, 6, 15, 10, 0);
        assert_eq!(next_boundary_after(start, &cfg).unwrap(), start);
        assert!(cycle_is_due(start, start, &cfg));
    }

    #[test]
    fn cycle_is_due_only_at_or_after_the_boundary() {
        let cfg = schedule(ScheduleMode::Daily, "19:00", None);
        let start = local_utc(2026, 6, 15, 10, 0);
        let boundary = local_utc(2026, 6, 15, 19, 0);
        assert!(!cycle_is_due(start, boundary - Duration::minutes(1), &cfg));
        assert!(cycle_is_due(start, boundary, &cfg));
        assert!(cycle_is_due(start, boundary + Duration::minutes(1), &cfg));
    }

    #[test]
    fn invalid_schedule_falls_back_to_one_day() {
        // An unparseable time must not wedge the cycle open forever: the
        // fallback closes it after 24h.
        let cfg = schedule(ScheduleMode::Daily, "not-a-time", None);
        let start = local_utc(2026, 6, 15, 10, 0);
        assert!(!cycle_is_due(start, start + Duration::hours(23), &cfg));
        assert!(cycle_is_due(start, start + Duration::days(1), &cfg));
        // Same for an invalid weekday on a weekly schedule.
        let cfg = schedule(ScheduleMode::Weekly, "19:00", Some("caturday"));
        assert!(!cycle_is_due(start, start + Duration::hours(23), &cfg));
        assert!(cycle_is_due(start, start + Duration::days(1), &cfg));
    }

    #[test]
    fn weekday_aliases_parse_and_junk_is_rejected() {
        for (value, expected) in [
            ("mon", Weekday::Mon),
            ("Tuesday", Weekday::Tue),
            ("WED", Weekday::Wed),
            ("thu", Weekday::Thu),
            ("friday", Weekday::Fri),
            ("sat", Weekday::Sat),
            ("sunday", Weekday::Sun),
        ] {
            assert_eq!(parse_weekday(value).unwrap(), expected, "{value}");
        }
        assert!(parse_weekday("caturday").is_err());
    }
}

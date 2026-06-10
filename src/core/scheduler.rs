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
            let mut date = start_local.date_naive();
            for _ in 0..14 {
                if date.weekday() == wanted {
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

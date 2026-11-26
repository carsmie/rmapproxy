//! Standing admin jobs from seed.yaml's `x-schedule:` (inert for mapproxy-seed, like the other
//! `x-` keys): `<name>: {title?, weekday?: mon..sun, at: 'HH:MM' (UTC), job: <POST /_admin/jobs
//! body>}`. No weekday = daily. Only the due-time arithmetic lives here; admin.rs runs them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde_json::Value as Json;
use serde_yaml::Value as Yaml;

const DAY: u64 = 86_400;
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Debug, Clone)]
pub struct Schedule {
    pub name: String,
    pub title: String,
    /// 0 = Monday; None = every day.
    pub weekday: Option<u64>,
    /// Seconds after midnight UTC.
    pub at: u64,
    /// The job request, as the GUI would POST it.
    pub job: Json,
}

impl Schedule {
    /// Entries of `x-schedule`, in file order.
    pub fn load(conf: &Yaml) -> Result<Vec<Schedule>> {
        let mut out = vec![];
        for (k, v) in conf.get("x-schedule").and_then(|s| s.as_mapping()).into_iter().flatten() {
            let name = k.as_str().ok_or_else(|| anyhow!("x-schedule: name must be a string"))?.to_string();
            if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                bail!("x-schedule: bad name {name:?}");
            }
            let weekday = match v.get("weekday").and_then(|w| w.as_str()) {
                None => None,
                Some(w) => Some(
                    WEEKDAYS
                        .iter()
                        .position(|d| w.to_ascii_lowercase().starts_with(d))
                        .ok_or_else(|| anyhow!("x-schedule.{name}: unknown weekday {w:?}"))? as u64,
                ),
            };
            let at = v.get("at").and_then(|a| a.as_str()).ok_or_else(|| anyhow!("x-schedule.{name}: at: 'HH:MM' missing"))?;
            let at = parse_hhmm(at).ok_or_else(|| anyhow!("x-schedule.{name}: bad at {at:?} (HH:MM, UTC)"))?;
            let job = serde_json::to_value(v.get("job").ok_or_else(|| anyhow!("x-schedule.{name}: job missing"))?)?;
            let title = v.get("title").and_then(|t| t.as_str()).unwrap_or(&name).to_string();
            out.push(Schedule { name, title, weekday, at, job });
        }
        Ok(out)
    }

    /// The latest due time at or before `now`.
    pub fn last_due(&self, now: SystemTime) -> SystemTime {
        let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let mut day = secs / DAY;
        if secs % DAY < self.at {
            day -= 1;
        }
        if let Some(w) = self.weekday {
            // 1970-01-01 was a Thursday (3 with Monday = 0).
            day -= (day + 3 + 7 - w) % 7;
        }
        UNIX_EPOCH + Duration::from_secs(day * DAY + self.at)
    }

    pub fn next_due(&self, now: SystemTime) -> SystemTime {
        self.last_due(now) + Duration::from_secs(if self.weekday.is_some() { 7 * DAY } else { DAY })
    }

    /// "søndag 01:00 UTC" / "hver dag 03:30 UTC" for the GUI.
    pub fn describe(&self) -> String {
        const NO: [&str; 7] = ["mandag", "tirsdag", "onsdag", "torsdag", "fredag", "lørdag", "søndag"];
        let hm = format!("{:02}:{:02} UTC", self.at / 3600, self.at % 3600 / 60);
        match self.weekday {
            Some(w) => format!("hver {} {hm}", NO[w as usize]),
            None => format!("hver dag {hm}"),
        }
    }
}

fn parse_hhmm(s: &str) -> Option<u64> {
    let (h, m) = s.split_once(':')?;
    let (h, m) = (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?);
    (h < 24 && m < 60).then_some(h * 3600 + m * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(s)
    }

    fn sched(yaml: &str) -> Schedule {
        Schedule::load(&serde_yaml::from_str(yaml).unwrap()).unwrap().remove(0)
    }

    #[test]
    fn weekly_and_daily_due_times() {
        // 2026-11-26T10:00:00Z is a Thursday.
        let now = at(1_795_687_200);
        let w = sched("x-schedule: {s: {weekday: sun, at: '01:00', job: {mode: cleanup}}}");
        assert_eq!(crate::admin::iso(w.last_due(now)), "2026-11-22T01:00:00Z");
        assert_eq!(crate::admin::iso(w.next_due(now)), "2026-11-29T01:00:00Z");
        let thu = sched("x-schedule: {s: {weekday: Thursday, at: '10:00', job: {}}}");
        assert_eq!(crate::admin::iso(thu.last_due(now)), "2026-11-26T10:00:00Z");
        let thu_later = sched("x-schedule: {s: {weekday: thu, at: '10:01', job: {}}}");
        assert_eq!(crate::admin::iso(thu_later.last_due(now)), "2026-11-19T10:01:00Z");
        let d = sched("x-schedule: {s: {at: '03:30', job: {}}}");
        assert_eq!(crate::admin::iso(d.last_due(now)), "2026-11-26T03:30:00Z");
        assert_eq!(crate::admin::iso(d.next_due(now)), "2026-11-27T03:30:00Z");
        assert_eq!(w.describe(), "hver søndag 01:00 UTC");
    }

    #[test]
    fn rejects_bad_entries() {
        let bad = |y: &str| Schedule::load(&serde_yaml::from_str(y).unwrap()).is_err();
        assert!(bad("x-schedule: {s: {at: '25:00', job: {}}}"));
        assert!(bad("x-schedule: {s: {weekday: funday, at: '01:00', job: {}}}"));
        assert!(bad("x-schedule: {s: {at: '01:00'}}"));
        assert!(bad("x-schedule: {'a b': {at: '01:00', job: {}}}"));
    }
}

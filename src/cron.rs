//! Scheduled HTTP requests (`[cron]` in `config.toml`). Off unless `enabled = true`.
//!
//! The server calls a URL on a schedule, so a job that has to run from this machine (a VPS in the
//! country the target service wants to see) does not need a crontab and `curl` next to it. A job is
//! a request (method, URL, headers, body) and a five-field cron schedule.
//!
//! There is no list of "next run" times: once a minute the scheduler asks every job whether the
//! schedule matches that minute. A job that is still running when its next time comes is skipped,
//! so a slow target does not pile requests up.

use crate::config::{CronConfig, JobConfig};
use crate::utils::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// How much of an answer is written to the log.
const LOGGED_BODY_CHARS: usize = 200;

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// The fields of a minute that a schedule is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minute {
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    pub month: u32,
    /// 0 is Sunday.
    pub weekday: u32,
}

impl Minute {
    /// The minute that contains `unix_seconds`, on a clock that is `offset_seconds` ahead of UTC.
    pub fn from_unix(unix_seconds: i64, offset_seconds: i64) -> Self {
        let local = unix_seconds + offset_seconds;
        let days = local.div_euclid(86_400);
        let seconds = local.rem_euclid(86_400);
        let (month, day) = month_and_day(days);

        Self {
            minute: (seconds / 60 % 60) as u32,
            hour: (seconds / 3600) as u32,
            day,
            month,
            // 1970-01-01 was a Thursday
            weekday: (days + 4).rem_euclid(7) as u32,
        }
    }
}

/// Month and day of the month of a day count from 1970-01-01 (the civil-from-days algorithm).
fn month_and_day(days: i64) -> (u32, u32) {
    let z = days + 719_468;
    // only the month and the day are wanted, so the era (and with it the year) is not needed
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };

    (month as u32, day as u32)
}

/// `+09:00`, `-05:30`, `+9`, `UTC` or `Z`, in seconds.
pub fn parse_utc_offset(text: &str) -> Result<i64> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("z") {
        return Ok(0);
    }

    let error = || format!("utc_offset must look like +09:00, -05:30 or UTC, got {text:?}");
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (1, &text[1..]),
        Some(b'-') => (-1, &text[1..]),
        _ => return Err(error().into()),
    };
    let (hours, minutes) = rest.split_once(':').unwrap_or((rest, "0"));
    let hours: i64 = hours.parse().map_err(|_| error())?;
    let minutes: i64 = minutes.parse().map_err(|_| error())?;
    if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
        return Err(error().into());
    }

    Ok(sign * (hours * 3600 + minutes * 60))
}

// ---------------------------------------------------------------------------------------------
// Schedule
// ---------------------------------------------------------------------------------------------

/// A five-field cron schedule: `minute hour day-of-month month day-of-week`.
///
/// Each field takes `*`, a number, a range (`1-5`), a step (`*/15`, `10-30/5`) and a comma
/// separated list of those. Months and weekdays can be written as names (`jan`, `mon`); the
/// weekday 7 is Sunday like 0. As in cron, when both the day of the month and the weekday are
/// restricted, a day that matches either of them runs the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Schedule {
    pub fn parse(text: &str) -> Result<Self> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(format!(
                "a schedule has five fields (minute hour day-of-month month day-of-week), got {text:?}"
            )
            .into());
        };

        let weekdays = parse_field(weekday, 0, 7, &WEEKDAY_NAMES, 0, "day-of-week")?;

        Ok(Self {
            minutes: parse_field(minute, 0, 59, &[], 0, "minute")?,
            hours: parse_field(hour, 0, 23, &[], 0, "hour")?,
            days: parse_field(day, 1, 31, &[], 0, "day-of-month")?,
            months: parse_field(month, 1, 12, &MONTH_NAMES, 1, "month")?,
            // 7 is Sunday
            weekdays: (weekdays | weekdays >> 7 & 1) & 0x7f,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    pub fn matches(&self, at: &Minute) -> bool {
        let day = self.days >> at.day & 1 == 1;
        let weekday = self.weekdays >> at.weekday & 1 == 1;
        let day_matches = match (self.any_day, self.any_weekday) {
            (false, false) => day || weekday,
            _ => day && weekday,
        };

        self.minutes >> at.minute & 1 == 1
            && self.hours >> at.hour & 1 == 1
            && self.months >> at.month & 1 == 1
            && day_matches
    }
}

/// One field as a bit set (bit `n` is the value `n`).
fn parse_field(text: &str, min: u32, max: u32, names: &[&str], names_from: u32, field: &str) -> Result<u64> {
    let error = |detail: &str| format!("cron {field} {text:?}: {detail}");
    let value = |part: &str| -> Result<u32> {
        if let Some(index) = names.iter().position(|name| name.eq_ignore_ascii_case(part)) {
            return Ok(index as u32 + names_from);
        }
        let number: u32 = part.parse().map_err(|_| error(&format!("{part:?} is not a number")))?;
        if !(min..=max).contains(&number) {
            return Err(error(&format!("{number} is not in {min}..={max}")).into());
        }
        Ok(number)
    };

    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().map_err(|_| error(&format!("the step {step:?} is not a number")))?;
                if step == 0 {
                    return Err(error("the step is 0").into());
                }
                (range, Some(step))
            }
            None => (item, None),
        };

        let (from, to) = if range == "*" {
            (min, max)
        } else if let Some((from, to)) = range.split_once('-') {
            (value(from)?, value(to)?)
        } else {
            let from = value(range)?;
            // `5/10` runs from 5 to the end, as in Vixie cron
            (from, if step.is_some() { max } else { from })
        };
        if from > to {
            return Err(error(&format!("the range {from}-{to} runs backwards")).into());
        }

        let mut n = from;
        while n <= to {
            bits |= 1 << n;
            n += step.unwrap_or(1);
        }
    }

    Ok(bits)
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

/// `${NAME}` in a config value is replaced by the environment variable NAME, so that a secret does
/// not have to be written in `config.toml`. A variable that is not set is an error.
pub fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| format!("{text:?} has a ${{ that is never closed"))?;
        let name = &after[..end];
        let value = lookup(name).ok_or_else(|| format!("the environment variable {name} (used in config.toml) is not set"))?;

        result.push_str(&value);
        rest = &after[end + 1..];
    }
    result.push_str(rest);

    Ok(result)
}

#[derive(Debug)]
struct Job {
    name: String,
    schedule: Schedule,
    method: ureq::http::Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout: Duration,
    /// True while a request of this job is in flight.
    running: Arc<AtomicBool>,
}

impl Job {
    fn new(config: &JobConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let named = |e: Box<dyn std::error::Error + Send + Sync>| format!("[[cron.jobs]] {:?}: {e}", config.name);

        let method: ureq::http::Method = config
            .method
            .to_ascii_uppercase()
            .parse()
            .map_err(|_| named(format!("{:?} is not an HTTP method", config.method).into()))?;
        let url = expand(&config.url, lookup).map_err(named)?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(named("url has to start with http:// or https://".into()).into());
        }
        if config.timeout_secs == 0 {
            return Err(named("timeout_secs is 0".into()).into());
        }

        let mut headers = Vec::new();
        for (key, value) in &config.headers {
            headers.push((key.clone(), expand(value, lookup).map_err(named)?));
        }

        Ok(Self {
            name: config.name.clone(),
            schedule: Schedule::parse(&config.schedule).map_err(named)?,
            method,
            url,
            headers,
            body: config.body.as_deref().map(|body| expand(body, lookup)).transpose().map_err(named)?,
            timeout: Duration::from_secs(config.timeout_secs),
            running: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// What came back from a job's request.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    status: u16,
    /// The start of the body.
    body: String,
}

/// Sends the request of a job. A status that is not 2xx is an answer, not an error.
fn send(job: &Job) -> Result<Answer> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(job.timeout))
        .http_status_as_error(false)
        .build()
        .into();

    let mut request = ureq::http::Request::builder().method(job.method.clone()).uri(&job.url);
    for (key, value) in &job.headers {
        request = request.header(key, value);
    }

    let response = match &job.body {
        Some(body) => agent.run(request.body(body.as_str())?)?,
        None => agent.run(request.body(())?)?,
    };
    let status = response.status().as_u16();
    // the body is read only for the log, and a body that is not text is not worth an error
    let body = response.into_body().read_to_string().unwrap_or_default();

    Ok(Answer { status, body: body.chars().take(LOGGED_BODY_CHARS).collect() })
}

async fn run_job(job: Arc<Job>) {
    let started = Instant::now();
    let sent = job.clone();
    let result = tokio::task::spawn_blocking(move || send(&sent)).await;

    match result {
        Ok(Ok(answer)) if (200..300).contains(&answer.status) => {
            log::info!("cron {}: {} in {:?} {}", job.name, answer.status, started.elapsed(), answer.body.trim());
        }
        Ok(Ok(answer)) => {
            log::warn!("cron {}: {} in {:?} {}", job.name, answer.status, started.elapsed(), answer.body.trim());
        }
        Ok(Err(e)) => log::warn!("cron {}: failed after {:?}: {}", job.name, started.elapsed(), e),
        Err(e) => log::error!("cron {}: the request task died: {}", job.name, e),
    }
    job.running.store(false, Ordering::SeqCst);
}

pub struct Scheduler {
    jobs: Vec<Arc<Job>>,
    offset_seconds: i64,
}

impl Scheduler {
    /// Checks the whole `[cron]` section. `None` when it is off or has no job that is on.
    pub fn new(config: &CronConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }

        let offset_seconds = parse_utc_offset(&config.utc_offset).map_err(|e| format!("[cron] {e}"))?;
        let mut jobs = Vec::new();
        for job in config.jobs.iter().filter(|job| job.enabled) {
            if jobs.iter().any(|other: &Arc<Job>| other.name == job.name) {
                return Err(format!("[[cron.jobs]] {:?}: the name is used twice", job.name).into());
            }
            jobs.push(Arc::new(Job::new(job, lookup)?));
        }

        if jobs.is_empty() {
            log::warn!("[cron] is enabled, but there is no job that is on");
            return Ok(None);
        }

        Ok(Some(Self { jobs, offset_seconds }))
    }

    /// Starts the jobs whose schedule matches `minute`. A job that is still running is skipped.
    fn fire(&self, minute: &Minute) -> usize {
        let mut started = 0;

        for job in self.jobs.iter().filter(|job| job.schedule.matches(minute)) {
            if job.running.swap(true, Ordering::SeqCst) {
                log::warn!("cron {}: the last request is still running, skipped", job.name);
                continue;
            }
            tokio::spawn(run_job(job.clone()));
            started += 1;
        }

        started
    }

    /// Runs until the task is aborted.
    pub fn spawn(self) -> JoinHandle<()> {
        for job in &self.jobs {
            log::info!("cron {}: {} {}", job.name, job.method, job.url);
        }

        tokio::spawn(async move {
            // the last minute that was handled, so that waking up early does not run it twice
            let mut last = now_seconds().div_euclid(60);

            loop {
                tokio::time::sleep(until_next_minute()).await;

                let now = now_seconds();
                if now.div_euclid(60) <= last {
                    continue;
                }
                // after a long sleep of the machine, only the minute it woke up in runs
                last = now.div_euclid(60);
                self.fire(&Minute::from_unix(last * 60, self.offset_seconds));
            }
        })
    }
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn until_next_minute() -> Duration {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let into_minute = Duration::from_millis((now.as_millis() % 60_000) as u64);

    // a little past the boundary, so that the clock has surely moved on to the new minute
    Duration::from_secs(60) - into_minute + Duration::from_millis(50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::any;
    use axum::Router;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    // 2026-10-28 (a Wednesday) 00:00 UTC
    const DAY: i64 = 1_793_145_600;

    fn at(unix: i64) -> Minute {
        Minute::from_unix(unix, 0)
    }

    fn matches(schedule: &str, minute: &Minute) -> bool {
        Schedule::parse(schedule).unwrap().matches(minute)
    }

    #[test]
    fn the_fields_of_a_minute() {
        assert_eq!(at(0), Minute { minute: 0, hour: 0, day: 1, month: 1, weekday: 4 });
        assert_eq!(at(DAY + 13 * 3600 + 45 * 60), Minute { minute: 45, hour: 13, day: 28, month: 10, weekday: 3 });
        // a leap day, and the day before the new year
        assert_eq!(at(1_709_208_000), Minute { minute: 0, hour: 12, day: 29, month: 2, weekday: 4 });
        assert_eq!(at(1_798_675_200 - 60), Minute { minute: 59, hour: 23, day: 30, month: 12, weekday: 3 });
        // before 1970
        assert_eq!(at(-60), Minute { minute: 59, hour: 23, day: 31, month: 12, weekday: 3 });
    }

    #[test]
    fn the_clock_can_be_ahead_of_utc() {
        // 15:00 UTC on the 27th is midnight on the 28th in Japan
        let jst = Minute::from_unix(DAY - 9 * 3600, 9 * 3600);
        assert_eq!((jst.hour, jst.minute, jst.day), (0, 0, 28));
        assert_eq!(parse_utc_offset("+09:00").unwrap(), 9 * 3600);
        assert_eq!(parse_utc_offset("+9").unwrap(), 9 * 3600);
        assert_eq!(parse_utc_offset("-05:30").unwrap(), -(5 * 3600 + 30 * 60));
        assert_eq!(parse_utc_offset("UTC").unwrap(), 0);
        assert!(parse_utc_offset("9").is_err());
        assert!(parse_utc_offset("+25:00").is_err());
        assert!(parse_utc_offset("+09:60").is_err());
    }

    #[test]
    fn stars_lists_ranges_and_steps() {
        let minute = at(DAY + 13 * 3600 + 45 * 60);
        assert!(matches("* * * * *", &minute));
        assert!(matches("45 13 * * *", &minute));
        assert!(!matches("44 13 * * *", &minute));
        assert!(matches("*/15 * * * *", &minute));
        assert!(!matches("*/10 * * * *", &minute));
        assert!(matches("0,45 12-14 * * *", &minute));
        assert!(matches("40-50/5 * * * *", &minute));
        assert!(!matches("40-50/10 * * * *", &minute));
        assert!(matches("5/20 * * * *", &minute), "5, 25, 45");
        assert!(matches("45 13 28 10 3", &minute));
    }

    #[test]
    fn names_and_the_two_ways_to_write_sunday() {
        let wednesday = at(DAY);
        assert!(matches("0 0 * oct wed", &wednesday));
        assert!(matches("0 0 * OCT WED", &wednesday));
        assert!(matches("0 0 * * mon-fri", &wednesday));
        assert!(!matches("0 0 * * sat,sun", &wednesday));

        let sunday = at(DAY + 4 * 86_400);
        assert_eq!(sunday.weekday, 0);
        assert!(matches("0 0 * * 0", &sunday));
        assert!(matches("0 0 * * 7", &sunday));
    }

    #[test]
    fn a_day_of_the_month_and_a_weekday_together_are_either() {
        // the 28th, or a Friday: a Wednesday the 28th is the first, a Wednesday the 21st is neither
        assert!(matches("0 0 28 * fri", &at(DAY)));
        assert!(!matches("0 0 21 * fri", &at(DAY)));
        assert!(matches("0 0 21 * wed", &at(DAY)));
        // only one of them restricted: that one has to match
        assert!(!matches("0 0 21 * *", &at(DAY)));
    }

    #[test]
    fn bad_schedules_are_errors() {
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "* * * foo *",
            "*/x * * * *",
        ] {
            assert!(Schedule::parse(bad).is_err(), "{bad:?} should be an error");
        }
        let message = Schedule::parse("* 24 * * *").unwrap_err().to_string();
        assert!(message.contains("hour"), "{message}");
    }

    #[test]
    fn environment_variables_in_values() {
        let lookup = |name: &str| (name == "TOKEN").then(|| "s3cret".to_string());
        assert_eq!(expand("Bearer ${TOKEN}", &lookup).unwrap(), "Bearer s3cret");
        assert_eq!(expand("${TOKEN}${TOKEN}", &lookup).unwrap(), "s3crets3cret");
        assert_eq!(expand("no variables, a $ is fine", &lookup).unwrap(), "no variables, a $ is fine");
        assert!(expand("${OTHER}", &lookup).unwrap_err().to_string().contains("OTHER"));
        assert!(expand("${TOKEN", &lookup).is_err());
    }

    /// A job named "j" that runs every minute. `extra` is more TOML: its keys replace the defaults,
    /// and a table (`[headers]`) goes last.
    fn job(extra: &str) -> JobConfig {
        let defaults = [("name", "\"j\""), ("schedule", "\"* * * * *\""), ("url", "\"http://127.0.0.1:1/\"")];
        let (keys, tables) = match extra.lines().position(|line| line.starts_with('[')) {
            Some(index) => {
                let at: usize = extra.lines().take(index).map(|line| line.len() + 1).sum();
                extra.split_at(at)
            }
            None => (extra, ""),
        };

        let mut text = String::new();
        for (key, value) in defaults {
            if !keys.lines().any(|line| line.trim_start().starts_with(&format!("{key} "))) {
                text.push_str(&format!("{key} = {value}\n"));
            }
        }

        toml::from_str(&format!("{text}{keys}\n{tables}")).unwrap()
    }

    fn cron(jobs: Vec<JobConfig>, enabled: bool) -> CronConfig {
        CronConfig { enabled, utc_offset: "+00:00".into(), jobs }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn it_is_off_by_default_and_ignores_the_jobs_then() {
        // not even a broken job matters while it is off
        let broken = job("method = \"not a method\"");
        assert!(Scheduler::new(&cron(vec![broken], false), &no_env).unwrap().is_none());
        assert!(Scheduler::new(&CronConfig::default(), &no_env).unwrap().is_none());
    }

    #[test]
    fn a_broken_job_is_an_error_that_names_it() {
        for extra in [
            "method = \"not a method\"",
            "url = \"ftp://example.com\"",
            "schedule = \"61 * * * *\"",
            "timeout_secs = 0",
            "body = \"${NOT_SET}\"",
        ] {
            let config = cron(vec![job(extra)], true);
            let error = Scheduler::new(&config, &no_env).err().unwrap_or_else(|| panic!("{extra} should fail")).to_string();
            assert!(error.contains("\"j\""), "{error}");
        }

        let twice = cron(vec![job(""), job("")], true);
        assert!(Scheduler::new(&twice, &no_env).err().unwrap().to_string().contains("twice"));
    }

    #[test]
    fn a_job_that_is_off_is_left_out() {
        let config = cron(vec![job("enabled = false")], true);
        assert!(Scheduler::new(&config, &no_env).unwrap().is_none());
    }

    #[test]
    fn the_toml_of_a_job() {
        let config: CronConfig = toml::from_str(
            "enabled = true\nutc_offset = \"+09:00\"\n\n[[jobs]]\nname = \"a\"\nschedule = \"0 9 * * *\"\nurl = \"https://example.com/x\"\n\
             method = \"post\"\nbody = '{\"ids\": []}'\ntimeout_secs = 5\n[jobs.headers]\nAuthorization = \"Bearer ${T}\"\n",
        )
        .unwrap();
        let lookup = |name: &str| (name == "T").then(|| "abc".to_string());
        let scheduler = Scheduler::new(&config, &lookup).unwrap().unwrap();
        let job = &scheduler.jobs[0];

        assert_eq!(scheduler.offset_seconds, 9 * 3600);
        assert_eq!(job.method, ureq::http::Method::POST);
        assert_eq!(job.headers, vec![("Authorization".to_string(), "Bearer abc".to_string())]);
        assert_eq!(job.body.as_deref(), Some("{\"ids\": []}"));
        assert_eq!(job.timeout, Duration::from_secs(5));
    }

    #[derive(Default)]
    struct Seen(Mutex<Vec<(String, Option<String>, String, String)>>);

    /// A server on a free port that records the requests it gets and answers `status`.
    async fn target(status: u16) -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let router = Router::new()
            .route(
                "/hook",
                any(move |State(seen): State<Arc<Seen>>, method: axum::http::Method, headers: HeaderMap, body: String| async move {
                    let auth = headers.get("authorization").map(|v| v.to_str().unwrap().to_string());
                    let content_length = headers.get("content-length").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
                    seen.0.lock().unwrap().push((method.to_string(), auth, body, content_length));
                    (axum::http::StatusCode::from_u16(status).unwrap(), "answer")
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        (url, seen)
    }

    fn scheduler_for(url: &str, extra: &str) -> Scheduler {
        let mut config = job(extra);
        config.url = url.to_string();
        Scheduler::new(&cron(vec![config], true), &no_env).unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_job_sends_its_request() {
        let (url, seen) = target(200).await;
        let extra = "method = \"POST\"\nbody = \"{\\\"ids\\\":[\\\"LBL1\\\"]}\"\n[headers]\nAuthorization = \"Bearer abc\"\n";
        let scheduler = scheduler_for(&url, extra);

        let answer = tokio::task::spawn_blocking({
            let job = scheduler.jobs[0].clone();
            move || send(&job)
        })
        .await
        .unwrap()
        .unwrap();

        assert_eq!(answer, Answer { status: 200, body: "answer".into() });
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1.as_deref(), Some("Bearer abc"));
        assert_eq!(seen[0].2, "{\"ids\":[\"LBL1\"]}");
    }

    #[tokio::test]
    async fn a_status_that_is_not_2xx_is_an_answer() {
        let (url, _) = target(503).await;
        let scheduler = scheduler_for(&url, "");
        let job = scheduler.jobs[0].clone();

        let answer = tokio::task::spawn_blocking(move || send(&job)).await.unwrap().unwrap();
        assert_eq!(answer.status, 503);
    }

    #[tokio::test]
    async fn a_target_that_is_down_is_an_error_not_a_panic() {
        // nothing listens on port 1
        let scheduler = scheduler_for("http://127.0.0.1:1/hook", "timeout_secs = 2");
        let job = scheduler.jobs[0].clone();

        assert!(tokio::task::spawn_blocking(move || send(&job)).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn fire_starts_the_matching_jobs_and_skips_one_that_is_running() {
        let (url, seen) = target(200).await;
        let scheduler = scheduler_for(&url, "schedule = \"30 6 * * *\"");

        assert_eq!(scheduler.fire(&at(DAY)), 0, "00:00 does not match 06:30");
        let matching = at(DAY + 6 * 3600 + 30 * 60);
        assert_eq!(scheduler.fire(&matching), 1);
        // the first request has not finished when the same minute comes again
        assert_eq!(scheduler.fire(&matching), 0);

        for _ in 0..100 {
            if !scheduler.jobs[0].running.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(seen.0.lock().unwrap().len(), 1);
        assert_eq!(scheduler.fire(&matching), 1, "it can run again once it has finished");
    }

    #[test]
    fn headers_are_kept_in_the_order_of_the_map() {
        let mut headers = BTreeMap::new();
        headers.insert("B".to_string(), "2".to_string());
        headers.insert("A".to_string(), "1".to_string());
        let mut config = job("");
        config.headers = headers;

        let job = Job::new(&config, &no_env).unwrap();
        assert_eq!(job.headers[0].0, "A");
    }
}

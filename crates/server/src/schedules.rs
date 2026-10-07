//! Scheduled flows and test suites.
//!
//! A schedule is a 5-field cron line (local time) plus what to run: a saved
//! flow (`iphone-use-mcp flow run`) or a test suite (`iphone-use-mcp test`).
//! The daemon owns the clock because it is the process that is always up;
//! the work itself runs through the same `iphone-use-mcp` binary an operator
//! would use, so a scheduled run gets every gate a hand-started one gets
//! (risk confirmation, compat, owner lease, failure diagnosis).
//!
//! Before each attempt the scheduler reads `/agent/status` like any agent:
//! another session holding the phone, or a person using or watching it,
//! postpones the run; a locked phone parks it as `waiting_unlock`; a released
//! phone is reconnected first. A run that cannot start before its window
//! closes is recorded as `missed`. Failures and misses raise a macOS
//! notification and, when configured, a webhook.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Cron
// ---------------------------------------------------------------------------

/// Broken-down local time, the part cron looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub minute: u32,
    pub hour: u32,
    /// 1..=31
    pub day: u32,
    /// 1..=12
    pub month: u32,
    /// 0 = Sunday
    pub weekday: u32,
}

/// The system's local time for a unix timestamp.
pub fn local_time(unix: u64) -> LocalTime {
    let t = unix as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which we own.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    LocalTime {
        minute: tm.tm_min as u32,
        hour: tm.tm_hour as u32,
        day: tm.tm_mday as u32,
        month: tm.tm_mon as u32 + 1,
        weekday: tm.tm_wday as u32,
    }
}

/// A parsed standard 5-field cron expression: minute hour day month weekday.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    days_restricted: bool,
    weekdays_restricted: bool,
}

fn parse_field(field: &str, min: u32, max: u32, name: &str) -> Result<(Vec<bool>, bool), String> {
    let mut set = vec![false; max as usize + 1];
    let restricted = field != "*";
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .map_err(|_| format!("{name}: bad step in {part:?}"))?;
                if step == 0 {
                    return Err(format!("{name}: step must be at least 1"));
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a
                .parse()
                .map_err(|_| format!("{name}: bad value in {part:?}"))?;
            let b: u32 = b
                .parse()
                .map_err(|_| format!("{name}: bad value in {part:?}"))?;
            (a, b)
        } else {
            let a: u32 = range
                .parse()
                .map_err(|_| format!("{name}: bad value in {part:?}"))?;
            // `5/15` means "from 5, every 15" like Vixie cron.
            (a, if part.contains('/') { max } else { a })
        };
        if start < min || end > max || start > end {
            return Err(format!("{name}: {part:?} is outside {min}-{max}"));
        }
        let mut value = start;
        while value <= end {
            set[value as usize] = true;
            value += step;
        }
    }
    Ok((set, restricted))
}

impl Cron {
    pub fn parse(expression: &str) -> Result<Cron, String> {
        let expanded = match expression.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err("cron needs 5 fields: minute hour day-of-month month day-of-week".into());
        };
        let (minutes, _) = parse_field(minute, 0, 59, "minute")?;
        let (hours, _) = parse_field(hour, 0, 23, "hour")?;
        let (days, days_restricted) = parse_field(day, 1, 31, "day-of-month")?;
        let (months, _) = parse_field(month, 1, 12, "month")?;
        // 0 and 7 are both Sunday.
        let (mut weekdays, weekdays_restricted) = parse_field(weekday, 0, 7, "day-of-week")?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        weekdays.truncate(7);
        Ok(Cron {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_restricted,
            weekdays_restricted,
        })
    }

    pub fn matches(&self, t: &LocalTime) -> bool {
        if !self.minutes[t.minute as usize]
            || !self.hours[t.hour as usize]
            || !self.months[t.month as usize]
        {
            return false;
        }
        let day = self.days[t.day as usize];
        let weekday = self.weekdays[t.weekday as usize];
        // Vixie rule: when both day fields are restricted, either may match.
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => day || weekday,
            _ => day && weekday,
        }
    }

    /// The first minute strictly after `after` that matches, searching up to
    /// a year and a day ahead (`None` for an impossible line like `0 0 31 2 *`).
    pub fn next_after(&self, after: u64, local: &dyn Fn(u64) -> LocalTime) -> Option<u64> {
        let mut candidate = (after / 60 + 1) * 60;
        let limit = after + 367 * 24 * 3600;
        while candidate <= limit {
            if self.matches(&local(candidate)) {
                return Some(candidate);
            }
            candidate += 60;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Flow,
    Test,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cron: String,
    pub kind: JobKind,
    /// Registry id or absolute flow file (flow); absolute suite path (test).
    pub target: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    /// The operator's explicit OK, given once at creation, for a job that
    /// sends, publishes, pays or deletes. Passed on as `--confirm`.
    #[serde(default)]
    pub confirm_side_effects: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How long after its scheduled minute a run may still start.
    #[serde(default = "default_window_mins")]
    pub window_mins: u32,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<u64>,
}

fn default_true() -> bool {
    true
}
fn default_window_mins() -> u32 {
    DEFAULT_WINDOW_MINS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Pending,
    Postponed,
    WaitingUnlock,
    Running,
    Passed,
    Failed,
    Missed,
    Skipped,
}

impl RunState {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            RunState::Pending | RunState::Postponed | RunState::WaitingUnlock | RunState::Running
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub schedule_id: String,
    pub scheduled_for: u64,
    pub deadline: u64,
    pub state: RunState,
    #[serde(default)]
    pub manual: bool,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub seq: u64,
}

pub const MAX_SCHEDULES: usize = 50;
/// Finished runs kept per schedule.
pub const RUNS_KEPT: usize = 20;
pub const DEFAULT_WINDOW_MINS: u32 = 60;
const POSTPONE_SECS: u64 = 180;
const UNLOCK_RECHECK_SECS: u64 = 60;
const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const RECONNECT_WAIT: Duration = Duration::from_secs(150);
const TICK: Duration = Duration::from_secs(15);
const OUTPUT_KEPT: usize = 600;

impl Store {
    fn next_id(&mut self, prefix: &str, now: u64) -> String {
        self.seq += 1;
        format!("{prefix}{now:x}{:02x}", self.seq % 256)
    }

    /// Drop finished runs beyond [`RUNS_KEPT`] per schedule, and the runs of
    /// schedules that no longer exist.
    fn prune(&mut self) {
        let ids: std::collections::BTreeSet<&str> =
            self.schedules.iter().map(|s| s.id.as_str()).collect();
        let mut kept: BTreeMap<String, usize> = BTreeMap::new();
        let mut runs = std::mem::take(&mut self.runs);
        runs.sort_by(|a, b| b.scheduled_for.cmp(&a.scheduled_for));
        runs.retain(|run| {
            if !ids.contains(run.schedule_id.as_str()) {
                return false;
            }
            if run.state.is_open() {
                return true;
            }
            let count = kept.entry(run.schedule_id.clone()).or_default();
            *count += 1;
            *count <= RUNS_KEPT
        });
        runs.reverse();
        self.runs = runs;
    }

    /// Advance every schedule to `now`: queue a run for each that came due
    /// (one per schedule however many minutes were missed), expire runs whose
    /// window closed, and return the runs that just ended as `missed` or
    /// `skipped` so the caller can notify.
    pub fn plan(&mut self, now: u64, local: &dyn Fn(u64) -> LocalTime) -> Vec<Run> {
        let mut ended = Vec::new();
        for run in self
            .runs
            .iter_mut()
            .filter(|r| r.state.is_open() && r.state != RunState::Running)
        {
            if now > run.deadline {
                run.state = RunState::Missed;
                run.finished_at = Some(now);
                run.reason = Some(match run.reason.take() {
                    Some(why) => format!("window closed before the phone was free: {why}"),
                    None => "window closed before the run could start".to_string(),
                });
                ended.push(run.clone());
            }
        }
        let mut queued = Vec::new();
        for index in 0..self.schedules.len() {
            let schedule = &self.schedules[index];
            let Ok(cron) = Cron::parse(&schedule.cron) else {
                continue;
            };
            if !schedule.enabled {
                continue;
            }
            let Some(due) = schedule.next_run_at else {
                self.schedules[index].next_run_at = cron.next_after(now, local);
                continue;
            };
            if now < due {
                continue;
            }
            let window = u64::from(schedule.window_mins.max(1)) * 60;
            let busy = self
                .runs
                .iter()
                .any(|r| r.schedule_id == schedule.id && r.state.is_open());
            queued.push((index, due, window, busy));
            self.schedules[index].next_run_at = cron.next_after(now, local);
        }
        for (index, due, window, busy) in queued {
            let schedule_id = self.schedules[index].id.clone();
            let id = self.next_id("r", now);
            let mut run = Run {
                id,
                schedule_id,
                scheduled_for: due,
                deadline: due + window,
                state: RunState::Pending,
                manual: false,
                attempts: 0,
                next_attempt_at: None,
                reason: None,
                started_at: None,
                finished_at: None,
                duration_ms: None,
                exit_code: None,
                summary: None,
                error: None,
                artifacts: None,
            };
            if busy {
                run.state = RunState::Skipped;
                run.finished_at = Some(now);
                run.reason = Some("the previous run of this schedule had not finished".into());
                ended.push(run.clone());
            }
            self.runs.push(run);
        }
        self.prune();
        ended
    }

    /// The open run to attempt now, oldest first.
    pub fn next_due_run(&self, now: u64) -> Option<String> {
        self.runs
            .iter()
            .filter(|r| {
                matches!(
                    r.state,
                    RunState::Pending | RunState::Postponed | RunState::WaitingUnlock
                )
            })
            .filter(|r| r.next_attempt_at.is_none_or(|at| at <= now))
            .min_by_key(|r| r.scheduled_for)
            .map(|r| r.id.clone())
    }
}

// ---------------------------------------------------------------------------
// Gate: may a run start now?
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Run,
    Reconnect,
    Postpone(String),
    WaitUnlock,
}

/// The three ways `/agent/status` says the screen is locked.
pub fn status_locked(status: &serde_json::Value) -> bool {
    status.get("device_state").and_then(|v| v.as_str()) == Some("locked")
        || status.get("setup_blocked_on").and_then(|v| v.as_str()) == Some("locked")
        || status.get("wda_locked").and_then(|v| v.as_bool()) == Some(true)
}

/// Decide from one `/agent/status` body. `me` is this run's owner name.
pub fn gate(status: &serde_json::Value, me: &str) -> Gate {
    if let Some(owner) = status.get("owner").and_then(|v| v.as_str()) {
        if owner != me {
            return Gate::Postpone(format!("another session ({owner}) is using the phone"));
        }
    }
    if status.get("human_handoff").and_then(|v| v.as_bool()) == Some(true) {
        return Gate::Postpone("the phone was handed to a person".into());
    }
    if status
        .get("viewer_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0
    {
        return Gate::Postpone("someone is watching the phone live".into());
    }
    if status_locked(status) {
        return Gate::WaitUnlock;
    }
    if status.get("drivable").and_then(|v| v.as_bool()) == Some(true) {
        return Gate::Run;
    }
    let device = status
        .get("device_state")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let blocker = status
        .get("setup_blocked_on")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !blocker.is_empty() {
        return Gate::Postpone(format!("setup is blocked on {blocker}"));
    }
    match device {
        "released" | "offline" | "degraded" | "reconnecting" => Gate::Reconnect,
        other => Gate::Postpone(format!(
            "the phone is not drivable (device_state={other:?})"
        )),
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSchedule {
    pub cron: String,
    pub kind: JobKind,
    pub target: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    #[serde(default)]
    pub confirm_side_effects: bool,
    #[serde(default)]
    pub webhook: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub window_mins: Option<u32>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn valid_input_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Shape checks that need no other process. `Err` is `(error code, message)`.
pub fn check_request(request: &CreateSchedule) -> Result<Cron, (&'static str, String)> {
    let cron = Cron::parse(&request.cron).map_err(|e| ("invalid_cron", e))?;
    let target = request.target.trim();
    if target.is_empty() || target.len() > 1024 || target.contains('\0') {
        return Err((
            "invalid_target",
            "target must be a registry id or an absolute path".into(),
        ));
    }
    if request.kind == JobKind::Test && !Path::new(target).is_absolute() {
        return Err((
            "invalid_target",
            "a test target must be an absolute suite path".into(),
        ));
    }
    if request.kind == JobKind::Test && !request.inputs.is_empty() {
        return Err(("invalid_inputs", "inputs apply to flows only".into()));
    }
    if request.inputs.len() > 16
        || request
            .inputs
            .iter()
            .any(|(k, v)| !valid_input_name(k) || v.len() > 4096 || v.contains('\0'))
    {
        return Err((
            "invalid_inputs",
            "inputs are up to 16 NAME=value pairs".into(),
        ));
    }
    if let Some(hook) = &request.webhook {
        if !(hook.starts_with("https://") || hook.starts_with("http://")) || hook.len() > 2048 {
            return Err(("invalid_webhook", "webhook must be an http(s) URL".into()));
        }
    }
    if let Some(name) = &request.name {
        if name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err((
                "invalid_name",
                "name is up to 100 printable characters".into(),
            ));
        }
    }
    if request
        .window_mins
        .is_some_and(|w| !(1..=24 * 60).contains(&w))
    {
        return Err(("invalid_window", "window_mins must be 1..1440".into()));
    }
    Ok(cron)
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Where the scheduler reaches the daemon it lives in, and how it runs jobs.
pub struct SchedulerConfig {
    pub store_path: PathBuf,
    pub artifacts_dir: PathBuf,
    /// `http://127.0.0.1:<port>` of this daemon.
    pub self_url: String,
    /// The agent token or password, if the daemon has one.
    pub credential: Option<String>,
    /// `iphone-use-mcp` next to the daemon.
    pub mcp: Option<PathBuf>,
    pub notify: bool,
}

pub struct Scheduler {
    config: SchedulerConfig,
    store: Mutex<Store>,
    wake: tokio::sync::Notify,
    http: reqwest::Client,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    local: fn(u64) -> LocalTime,
    /// One attempt at a time (a single phone).
    attempt_lock: tokio::sync::Mutex<()>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_store(path: &Path) -> Store {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(
                "schedules: unreadable store {} ({error}); starting empty",
                path.display()
            );
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            Store::default()
        }),
        Err(_) => Store::default(),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Arc<Scheduler> {
        Self::with_clock(config, Arc::new(unix_now), local_time)
    }

    pub fn with_clock(
        config: SchedulerConfig,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        local: fn(u64) -> LocalTime,
    ) -> Arc<Scheduler> {
        let store = read_store(&config.store_path);
        Arc::new(Scheduler {
            config,
            store: Mutex::new(store),
            wake: tokio::sync::Notify::new(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(150))
                .build()
                .expect("reqwest client"),
            clock,
            local,
            attempt_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, store: &Store) {
        let bytes = match serde_json::to_vec_pretty(store) {
            Ok(bytes) => bytes,
            Err(error) => return tracing::warn!("schedules: serialize: {error}"),
        };
        if let Some(parent) = self.config.store_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = write_private(&self.config.store_path, &bytes) {
            tracing::warn!(
                "schedules: save {}: {error}",
                self.config.store_path.display()
            );
        }
    }

    // ---- API ----------------------------------------------------------------

    pub fn list(&self) -> serde_json::Value {
        let store = self.lock();
        let schedules: Vec<serde_json::Value> = store
            .schedules
            .iter()
            .map(|schedule| {
                let mut value = serde_json::to_value(schedule).unwrap_or_default();
                let last = store
                    .runs
                    .iter()
                    .filter(|r| r.schedule_id == schedule.id)
                    .max_by_key(|r| r.scheduled_for);
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "last_run".into(),
                        serde_json::to_value(last).unwrap_or_default(),
                    );
                    if let Some(hook) = object.get_mut("webhook") {
                        // Webhook URLs often embed a secret; show the host only.
                        let shown = hook
                            .as_str()
                            .and_then(|url| reqwest::Url::parse(url).ok())
                            .map(|url| {
                                format!("{}://{}/…", url.scheme(), url.host_str().unwrap_or("?"))
                            })
                            .unwrap_or_else(|| "set".into());
                        *hook = serde_json::json!(shown);
                    }
                }
                value
            })
            .collect();
        serde_json::json!({ "ok": true, "schedules": schedules, "mcp_available": self.config.mcp.is_some() })
    }

    pub fn runs(&self, schedule_id: Option<&str>) -> Option<serde_json::Value> {
        let store = self.lock();
        if let Some(id) = schedule_id {
            if !store.schedules.iter().any(|s| s.id == id) {
                return None;
            }
        }
        let mut runs: Vec<&Run> = store
            .runs
            .iter()
            .filter(|r| schedule_id.is_none_or(|id| r.schedule_id == id))
            .collect();
        runs.sort_by(|a, b| b.scheduled_for.cmp(&a.scheduled_for));
        Some(serde_json::json!({ "ok": true, "runs": runs }))
    }

    /// Validate with `iphone-use-mcp` (`flow validate` / `test --validate`)
    /// and refuse a side-effect job without the explicit flag.
    async fn validate_target(
        &self,
        request: &CreateSchedule,
    ) -> Result<(), (&'static str, String)> {
        let Some(mcp) = &self.config.mcp else {
            return Err((
                "mcp_unavailable",
                "iphone-use-mcp is not installed next to the daemon".into(),
            ));
        };
        let args: Vec<String> = match request.kind {
            JobKind::Flow => vec!["flow".into(), "validate".into(), request.target.clone()],
            JobKind::Test => vec!["test".into(), request.target.clone(), "--validate".into()],
        };
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(mcp)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| ("invalid_target", "validation timed out".to_string()))?
        .map_err(|e| {
            (
                "invalid_target",
                format!("could not run iphone-use-mcp: {e}"),
            )
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let why = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            return Err(("invalid_target", truncate(&why, 400)));
        }
        let summary: serde_json::Value = String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .unwrap_or_default();
        let side_effect = summary.get("risk").and_then(|v| v.as_str()) == Some("side_effect")
            || summary.get("side_effect").and_then(|v| v.as_bool()) == Some(true);
        if side_effect && !request.confirm_side_effects {
            return Err((
                "confirm_required",
                "this job sends, publishes, pays or deletes; schedule it again with confirm_side_effects=true after checking the target and inputs".into(),
            ));
        }
        Ok(())
    }

    pub async fn create(
        &self,
        request: CreateSchedule,
    ) -> Result<serde_json::Value, (&'static str, String)> {
        let cron = check_request(&request)?;
        if self.lock().schedules.len() >= MAX_SCHEDULES {
            return Err((
                "too_many_schedules",
                format!("at most {MAX_SCHEDULES} schedules"),
            ));
        }
        self.validate_target(&request).await?;
        let now = self.now();
        let mut store = self.lock();
        let id = store.next_id("s", now);
        let schedule = Schedule {
            id,
            name: request
                .name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty()),
            cron: request.cron.trim().to_string(),
            kind: request.kind,
            target: request.target.trim().to_string(),
            inputs: request.inputs,
            confirm_side_effects: request.confirm_side_effects,
            webhook: request.webhook,
            enabled: request.enabled.unwrap_or(true),
            window_mins: request.window_mins.unwrap_or(DEFAULT_WINDOW_MINS),
            created_at: now,
            next_run_at: cron.next_after(now, &self.local),
        };
        let value = serde_json::to_value(&schedule).unwrap_or_default();
        store.schedules.push(schedule);
        self.save(&store);
        Ok(serde_json::json!({ "ok": true, "schedule": value }))
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut store = self.lock();
        let before = store.schedules.len();
        store.schedules.retain(|s| s.id != id);
        let removed = store.schedules.len() != before;
        if removed {
            store.prune();
            self.save(&store);
        }
        removed
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Option<serde_json::Value> {
        let now = self.now();
        let mut store = self.lock();
        let schedule = store.schedules.iter_mut().find(|s| s.id == id)?;
        schedule.enabled = enabled;
        schedule.next_run_at = if enabled {
            Cron::parse(&schedule.cron)
                .ok()
                .and_then(|c| c.next_after(now, &self.local))
        } else {
            None
        };
        let value = serde_json::to_value(&*schedule).unwrap_or_default();
        self.save(&store);
        Some(serde_json::json!({ "ok": true, "schedule": value }))
    }

    /// Queue a run of `id` now (the web page's and CLI's "run now").
    pub fn run_now(&self, id: &str) -> Option<Result<serde_json::Value, (&'static str, String)>> {
        let now = self.now();
        let mut store = self.lock();
        let schedule = store.schedules.iter().find(|s| s.id == id)?.clone();
        if store
            .runs
            .iter()
            .any(|r| r.schedule_id == id && r.state.is_open())
        {
            return Some(Err((
                "run_open",
                "a run of this schedule is already queued or running".into(),
            )));
        }
        let run_id = store.next_id("r", now);
        let run = Run {
            id: run_id,
            schedule_id: schedule.id,
            scheduled_for: now,
            deadline: now + u64::from(schedule.window_mins.max(1)) * 60,
            state: RunState::Pending,
            manual: true,
            attempts: 0,
            next_attempt_at: None,
            reason: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            exit_code: None,
            summary: None,
            error: None,
            artifacts: None,
        };
        let value = serde_json::to_value(&run).unwrap_or_default();
        store.runs.push(run);
        self.save(&store);
        drop(store);
        self.wake.notify_one();
        Some(Ok(serde_json::json!({ "ok": true, "run": value })))
    }

    // ---- loop ---------------------------------------------------------------

    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.tick().await;
                let _ = tokio::time::timeout(TICK, me.wake.notified()).await;
            }
        });
    }

    /// Plan, then make at most one attempt. Public for tests.
    pub async fn tick(&self) {
        let ended = {
            let now = self.now();
            let mut store = self.lock();
            // A run left `running` by a daemon that died mid-run never finished.
            for run in store
                .runs
                .iter_mut()
                .filter(|r| r.state == RunState::Running)
            {
                if self.attempt_lock.try_lock().is_ok() {
                    run.state = RunState::Failed;
                    run.finished_at = Some(now);
                    run.error = Some(
                        "the daemon stopped while this run was in progress; outcome unknown".into(),
                    );
                }
            }
            let ended = store.plan(now, &self.local);
            self.save(&store);
            ended
        };
        for run in &ended {
            self.notify(run).await;
        }
        let next = self.lock().next_due_run(self.now());
        if let Some(run_id) = next {
            self.attempt(&run_id).await;
        }
    }

    fn owner_for(run: &Run) -> String {
        format!("schedule-{}", run.schedule_id)
    }

    async fn status(&self) -> Option<serde_json::Value> {
        let mut req = self
            .http
            .get(format!("{}/agent/status", self.config.self_url));
        if let Some(token) = &self.config.credential {
            req = req.bearer_auth(token);
        }
        req.send().await.ok()?.json().await.ok()
    }

    async fn post(&self, path: &str, owner: &str, body: &str) {
        let mut req = self
            .http
            .post(format!("{}{path}", self.config.self_url))
            .header("x-phone-control", "1")
            .header("x-phone-owner", owner)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = &self.config.credential {
            req = req.bearer_auth(token);
        }
        if let Err(error) = req.send().await {
            tracing::warn!("schedules: POST {path}: {error}");
        }
    }

    fn update_run(&self, run_id: &str, change: impl FnOnce(&mut Run)) -> Option<Run> {
        let mut store = self.lock();
        let run = store.runs.iter_mut().find(|r| r.id == run_id)?;
        change(run);
        let copy = run.clone();
        self.save(&store);
        Some(copy)
    }

    async fn attempt(&self, run_id: &str) {
        let Ok(_guard) = self.attempt_lock.try_lock() else {
            return;
        };
        let Some((run, schedule)) = ({
            let store = self.lock();
            store
                .runs
                .iter()
                .find(|r| r.id == run_id)
                .cloned()
                .and_then(|run| {
                    let schedule = store
                        .schedules
                        .iter()
                        .find(|s| s.id == run.schedule_id)
                        .cloned()?;
                    Some((run, schedule))
                })
        }) else {
            return;
        };
        let owner = Self::owner_for(&run);
        let mut verdict = match self.status().await {
            Some(status) => gate(&status, &owner),
            None => Gate::Postpone("the daemon status could not be read".into()),
        };
        if verdict == Gate::Reconnect {
            self.post("/agent/mode", &owner, r#"{"mode":"agent"}"#)
                .await;
            let deadline = tokio::time::Instant::now() + RECONNECT_WAIT;
            verdict = Gate::Postpone("the phone did not come back after reconnecting".into());
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(status) = self.status().await else {
                    continue;
                };
                match gate(&status, &owner) {
                    Gate::Reconnect => continue,
                    other => {
                        verdict = other;
                        break;
                    }
                }
            }
        }
        let now = self.now();
        match verdict {
            Gate::Run => self.execute(run, schedule, owner).await,
            Gate::Reconnect => {}
            Gate::Postpone(reason) => {
                self.update_run(run_id, |run| {
                    run.state = RunState::Postponed;
                    run.attempts += 1;
                    run.reason = Some(reason);
                    run.next_attempt_at = Some(now + POSTPONE_SECS);
                });
            }
            Gate::WaitUnlock => {
                self.update_run(run_id, |run| {
                    run.state = RunState::WaitingUnlock;
                    run.attempts += 1;
                    run.reason =
                        Some("the iPhone is locked; the run starts when it is unlocked".into());
                    run.next_attempt_at = Some(now + UNLOCK_RECHECK_SECS);
                });
            }
        }
    }

    fn command_args(schedule: &Schedule, artifacts: &Path) -> Vec<String> {
        let mut args: Vec<String> = match schedule.kind {
            JobKind::Flow => vec!["flow".into(), "run".into(), schedule.target.clone()],
            JobKind::Test => vec!["test".into(), schedule.target.clone(), "--json".into()],
        };
        for (name, value) in &schedule.inputs {
            args.push("--input".into());
            args.push(format!("{name}={value}"));
        }
        if schedule.confirm_side_effects {
            args.push("--confirm".into());
        }
        args.push("--artifacts-dir".into());
        args.push(artifacts.display().to_string());
        args
    }

    /// One line saying how the run went, read from the command's JSON.
    fn summarize(kind: JobKind, stdout: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok().or_else(|| {
            stdout
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str(line).ok())
        })?;
        match kind {
            JobKind::Test => Some(format!(
                "{} passed, {} failed{}",
                json.get("passed").and_then(|v| v.as_u64()).unwrap_or(0),
                json.get("failed").and_then(|v| v.as_u64()).unwrap_or(0),
                json.get("error")
                    .and_then(|v| v.as_str())
                    .map(|e| format!(" — {e}"))
                    .unwrap_or_default()
            )),
            JobKind::Flow => Some(match json.get("ok").and_then(|v| v.as_bool()) {
                Some(true) => "flow passed".to_string(),
                _ => format!(
                    "flow failed: {}{}",
                    json.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown"),
                    json.get("failed_step")
                        .map(|s| format!(" at step {s}"))
                        .unwrap_or_default()
                ),
            }),
        }
    }

    async fn execute(&self, run: Run, schedule: Schedule, owner: String) {
        let Some(mcp) = self.config.mcp.clone() else {
            let ended = self.update_run(&run.id, |run| {
                run.state = RunState::Failed;
                run.finished_at = Some(unix_now());
                run.error = Some("iphone-use-mcp is not installed next to the daemon".into());
            });
            if let Some(ended) = ended {
                self.notify(&ended).await;
            }
            return;
        };
        let started = self.now();
        let artifacts = self.config.artifacts_dir.join(&run.id);
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&artifacts);
        }
        self.update_run(&run.id, |run| {
            run.state = RunState::Running;
            run.attempts += 1;
            run.started_at = Some(started);
            run.next_attempt_at = None;
            run.reason = None;
        });
        let clock = std::time::Instant::now();
        let mut command = tokio::process::Command::new(&mcp);
        command
            .args(Self::command_args(&schedule, &artifacts))
            .current_dir(&artifacts)
            .env("PHONE_REMOTE_URL", &self.config.self_url)
            .env("PHONE_REMOTE_OWNER", &owner)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        match &self.config.credential {
            Some(token) => command.env("PHONE_REMOTE_TOKEN", token),
            None => command.env_remove("PHONE_REMOTE_TOKEN"),
        };
        let outcome = tokio::time::timeout(RUN_TIMEOUT, command.output()).await;
        let duration_ms = clock.elapsed().as_millis() as u64;
        let (state, exit_code, summary, error) = match outcome {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let _ = write_private(&artifacts.join("stdout.txt"), stdout.as_bytes());
                let _ = write_private(&artifacts.join("stderr.txt"), stderr.as_bytes());
                let code = output.status.code();
                let summary = Self::summarize(schedule.kind, &stdout);
                if output.status.success() {
                    (RunState::Passed, code, summary, None)
                } else {
                    let last = stderr
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("");
                    let error = if last.is_empty() {
                        format!("exit {}", code.unwrap_or(-1))
                    } else {
                        truncate(last, OUTPUT_KEPT)
                    };
                    (RunState::Failed, code, summary, Some(error))
                }
            }
            Ok(Err(error)) => (
                RunState::Failed,
                None,
                None,
                Some(format!("could not start iphone-use-mcp: {error}")),
            ),
            Err(_) => (
                RunState::Failed,
                None,
                None,
                Some(format!(
                    "timed out after {} minutes; stopped",
                    RUN_TIMEOUT.as_secs() / 60
                )),
            ),
        };
        // Hand the phone back whatever happened.
        self.post("/agent/owner", &owner, r#"{"release":true}"#)
            .await;
        let finished = self.now();
        let ended = self.update_run(&run.id, |run| {
            run.state = state;
            run.finished_at = Some(finished);
            run.duration_ms = Some(duration_ms);
            run.exit_code = exit_code;
            run.summary = summary;
            run.error = error;
            run.artifacts = Some(artifacts.display().to_string());
        });
        if let Some(ended) = ended {
            if ended.state != RunState::Passed {
                self.notify(&ended).await;
            }
        }
    }

    async fn notify(&self, run: &Run) {
        let schedule = self
            .lock()
            .schedules
            .iter()
            .find(|s| s.id == run.schedule_id)
            .cloned();
        let Some(schedule) = schedule else { return };
        let label = schedule
            .name
            .clone()
            .unwrap_or_else(|| schedule.target.clone());
        let what = match run.state {
            RunState::Failed => "failed",
            RunState::Missed => "missed",
            RunState::Skipped => "skipped",
            _ => return,
        };
        let detail = run
            .error
            .clone()
            .or_else(|| run.summary.clone())
            .or_else(|| run.reason.clone())
            .unwrap_or_default();
        tracing::warn!("schedule {} ({label}) {what}: {detail}", schedule.id);
        if self.config.notify && cfg!(target_os = "macos") {
            let message = truncate(&format!("{label} {what}: {detail}"), 200);
            let _ = tokio::process::Command::new("/usr/bin/osascript")
                .args([
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 1 of argv) with title (item 2 of argv)",
                    "-e",
                    "end run",
                    &message,
                    "iphone-use schedule",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status()
                .await;
        }
        if let Some(hook) = &schedule.webhook {
            let body = serde_json::json!({
                "event": format!("run_{what}"),
                "schedule": { "id": schedule.id, "name": schedule.name, "kind": schedule.kind, "target": schedule.target },
                "run": run,
            });
            if let Err(error) = self
                .http
                .post(hook)
                .timeout(Duration::from_secs(10))
                .json(&body)
                .send()
                .await
            {
                tracing::warn!("schedule {} webhook failed: {error}", schedule.id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Process-wide handle (the router's handlers read it)
// ---------------------------------------------------------------------------

static SCHEDULER: Mutex<Option<Arc<Scheduler>>> = Mutex::new(None);

pub fn install(scheduler: Arc<Scheduler>) {
    *SCHEDULER.lock().unwrap_or_else(|e| e.into_inner()) = Some(scheduler);
}

pub fn current() -> Option<Arc<Scheduler>> {
    SCHEDULER.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Start the scheduler for this daemon. `host`/`port` are the listen address.
pub fn start(
    host: &str,
    port: u16,
    credential: Option<String>,
    state_dir: &Path,
) -> Arc<Scheduler> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let reach = match host {
        "0.0.0.0" | "" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        other if other.contains(':') => format!("[{other}]"),
        other => other.to_string(),
    };
    let scheduler = Scheduler::new(SchedulerConfig {
        store_path: state_dir.join("schedules.json"),
        artifacts_dir: state_dir.join("schedule-runs"),
        self_url: format!("http://{reach}:{port}"),
        credential,
        mcp: crate::flows::mcp_binary(),
        notify: std::env::var_os("IPHONE_USE_SCHEDULE_NO_NOTIFY").is_none(),
    });
    install(scheduler.clone());
    scheduler.spawn();
    scheduler
}

// ---------------------------------------------------------------------------
// HTTP handlers (`/agent/schedules…`, `/schedules`)
// ---------------------------------------------------------------------------

use axum::{
    body::Body,
    extract::{Path as UrlPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};

use crate::http::{authorize_extension, secured, AppState};

const SCHEDULES_HTML: &str = include_str!("../../../web/schedules.html");

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    secured(
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    )
}

fn error_response(status: StatusCode, error: &str, message: &str) -> Response {
    json_response(
        status,
        serde_json::json!({ "ok": false, "error": error, "message": message }),
    )
}

fn scheduler_or_503() -> Result<Arc<Scheduler>, Response> {
    current().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "scheduler_unavailable",
            "this daemon is not running the scheduler",
        )
    })
}

fn valid_id(id: &str) -> bool {
    (2..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub async fn page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !crate::http::is_authed(&state, &headers) {
        return secured(Redirect::to("/login?next=%2Fschedules").into_response());
    }
    secured(Html(SCHEDULES_HTML).into_response())
}

pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, false) {
        return response;
    }
    match scheduler_or_503() {
        Ok(scheduler) => json_response(StatusCode::OK, scheduler.list()),
        Err(response) => response,
    }
}

pub async fn all_runs(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, false) {
        return response;
    }
    match scheduler_or_503() {
        Ok(scheduler) => json_response(StatusCode::OK, scheduler.runs(None).unwrap_or_default()),
        Err(response) => response,
    }
}

pub async fn runs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, false) {
        return response;
    }
    let scheduler = match scheduler_or_503() {
        Ok(scheduler) => scheduler,
        Err(response) => return response,
    };
    match valid_id(&id).then(|| scheduler.runs(Some(&id))).flatten() {
        Some(value) => json_response(StatusCode::OK, value),
        None => error_response(
            StatusCode::NOT_FOUND,
            "no_such_schedule",
            "no schedule with that id",
        ),
    }
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, true) {
        return response;
    }
    let scheduler = match scheduler_or_503() {
        Ok(scheduler) => scheduler,
        Err(response) => return response,
    };
    let request: CreateSchedule = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &error.to_string(),
            )
        }
    };
    match scheduler.create(request).await {
        Ok(value) => json_response(StatusCode::CREATED, value),
        Err(("mcp_unavailable", message)) => {
            error_response(StatusCode::SERVICE_UNAVAILABLE, "mcp_unavailable", &message)
        }
        Err((code, message)) => error_response(StatusCode::BAD_REQUEST, code, &message),
    }
}

pub async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, true) {
        return response;
    }
    let scheduler = match scheduler_or_503() {
        Ok(scheduler) => scheduler,
        Err(response) => return response,
    };
    if valid_id(&id) && scheduler.delete(&id) {
        json_response(
            StatusCode::OK,
            serde_json::json!({ "ok": true, "deleted": id }),
        )
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            "no_such_schedule",
            "no schedule with that id",
        )
    }
}

/// `PATCH /agent/schedules/:id {"enabled": bool}`.
pub async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, true) {
        return response;
    }
    let scheduler = match scheduler_or_503() {
        Ok(scheduler) => scheduler,
        Err(response) => return response,
    };
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Update {
        enabled: bool,
    }
    let update: Update = match serde_json::from_str(&body) {
        Ok(update) => update,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &error.to_string(),
            )
        }
    };
    match valid_id(&id)
        .then(|| scheduler.set_enabled(&id, update.enabled))
        .flatten()
    {
        Some(value) => json_response(StatusCode::OK, value),
        None => error_response(
            StatusCode::NOT_FOUND,
            "no_such_schedule",
            "no schedule with that id",
        ),
    }
}

pub async fn run_now(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Err(response) = authorize_extension(&state, &headers, true) {
        return response;
    }
    let scheduler = match scheduler_or_503() {
        Ok(scheduler) => scheduler,
        Err(response) => return response,
    };
    match valid_id(&id).then(|| scheduler.run_now(&id)).flatten() {
        Some(Ok(value)) => json_response(StatusCode::ACCEPTED, value),
        Some(Err((code, message))) => error_response(StatusCode::CONFLICT, code, &message),
        None => error_response(
            StatusCode::NOT_FOUND,
            "no_such_schedule",
            "no schedule with that id",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "local" calendar for tests: treat unix time as UTC.
    fn utc(unix: u64) -> LocalTime {
        let days = unix / 86_400;
        let secs = unix % 86_400;
        // 1970-01-01 was a Thursday.
        let weekday = ((days + 4) % 7) as u32;
        // Civil-from-days (Howard Hinnant).
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        LocalTime {
            minute: (secs / 60 % 60) as u32,
            hour: (secs / 3600) as u32,
            day,
            month,
            weekday,
        }
    }

    // 2026-10-07 00:00:00 UTC, a Wednesday.
    const WED: u64 = 1_791_331_200;

    #[test]
    fn the_test_calendar_is_right() {
        assert_eq!(
            utc(WED),
            LocalTime {
                minute: 0,
                hour: 0,
                day: 7,
                month: 10,
                weekday: 3
            }
        );
    }

    #[test]
    fn cron_fields_parse_and_match() {
        let every_15 = Cron::parse("*/15 * * * *").unwrap();
        assert!(every_15.matches(&utc(WED + 15 * 60)));
        assert!(!every_15.matches(&utc(WED + 16 * 60)));
        let weekdays_9 = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(weekdays_9.matches(&utc(WED + 9 * 3600)));
        assert!(
            !weekdays_9.matches(&utc(WED + 4 * 86_400 + 9 * 3600)),
            "Sunday"
        );
        let sunday_7 = Cron::parse("0 0 * * 7").unwrap();
        assert!(sunday_7.matches(&utc(WED + 4 * 86_400)), "7 is Sunday too");
        let list = Cron::parse("5,35 8-10 * * *").unwrap();
        assert!(list.matches(&utc(WED + 10 * 3600 + 35 * 60)));
        assert!(!list.matches(&utc(WED + 11 * 3600 + 5 * 60)));
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );
        // Vixie: both day fields restricted → either matches.
        let either = Cron::parse("0 0 1 * 3").unwrap();
        assert!(either.matches(&utc(WED)), "a Wednesday that is not the 1st");
        for bad in [
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
            "* * 0 * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn next_after_is_strictly_later_and_minute_aligned() {
        let cron = Cron::parse("30 9 * * *").unwrap();
        assert_eq!(cron.next_after(WED, &utc), Some(WED + 9 * 3600 + 30 * 60));
        assert_eq!(
            cron.next_after(WED + 9 * 3600 + 30 * 60, &utc),
            Some(WED + 86_400 + 9 * 3600 + 30 * 60)
        );
        assert_eq!(
            Cron::parse("0 0 31 2 *").unwrap().next_after(WED, &utc),
            None,
            "Feb 31 never comes"
        );
    }

    fn schedule(id: &str, cron: &str, next: Option<u64>) -> Schedule {
        Schedule {
            id: id.into(),
            name: None,
            cron: cron.into(),
            kind: JobKind::Flow,
            target: "system/x".into(),
            inputs: BTreeMap::new(),
            confirm_side_effects: false,
            webhook: None,
            enabled: true,
            window_mins: 60,
            created_at: WED,
            next_run_at: next,
        }
    }

    #[test]
    fn plan_queues_one_run_per_due_schedule_and_moves_the_next_time_on() {
        let mut store = Store {
            schedules: vec![schedule("s1", "0 9 * * *", Some(WED + 9 * 3600))],
            ..Default::default()
        };
        assert!(store.plan(WED + 8 * 3600, &utc).is_empty());
        assert!(store.runs.is_empty(), "not due yet");
        // The daemon slept through 9:00 until 9:20 — still inside the window.
        let ended = store.plan(WED + 9 * 3600 + 20 * 60, &utc);
        assert!(ended.is_empty());
        assert_eq!(store.runs.len(), 1);
        let run = &store.runs[0];
        assert_eq!(run.state, RunState::Pending);
        assert_eq!(run.scheduled_for, WED + 9 * 3600);
        assert_eq!(run.deadline, WED + 10 * 3600);
        assert_eq!(
            store.schedules[0].next_run_at,
            Some(WED + 86_400 + 9 * 3600)
        );
        assert_eq!(
            store.next_due_run(WED + 9 * 3600 + 20 * 60).as_deref(),
            Some(run.id.as_str())
        );
    }

    #[test]
    fn a_run_that_never_got_the_phone_is_missed_when_its_window_closes() {
        let mut store = Store {
            schedules: vec![schedule("s1", "0 9 * * *", Some(WED + 9 * 3600))],
            ..Default::default()
        };
        store.plan(WED + 9 * 3600, &utc);
        store.runs[0].state = RunState::Postponed;
        store.runs[0].reason = Some("another session (x) is using the phone".into());
        let ended = store.plan(WED + 10 * 3600 + 1, &utc);
        assert_eq!(ended.len(), 1);
        assert_eq!(store.runs[0].state, RunState::Missed);
        assert!(store.runs[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("another session"));
    }

    #[test]
    fn an_occurrence_while_the_last_run_is_open_is_skipped_not_stacked() {
        let mut store = Store {
            schedules: vec![schedule("s1", "*/5 * * * *", Some(WED + 300))],
            ..Default::default()
        };
        store.plan(WED + 300, &utc);
        store.runs[0].state = RunState::Running;
        let ended = store.plan(WED + 600, &utc);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].state, RunState::Skipped);
        assert_eq!(store.runs.iter().filter(|r| r.state.is_open()).count(), 1);
    }

    #[test]
    fn a_disabled_or_new_schedule_does_not_fire_on_the_first_tick() {
        let mut off = schedule("s1", "* * * * *", Some(WED));
        off.enabled = false;
        let mut store = Store {
            schedules: vec![off, schedule("s2", "* * * * *", None)],
            ..Default::default()
        };
        store.plan(WED + 3600, &utc);
        assert!(store.runs.is_empty());
        assert_eq!(store.schedules[1].next_run_at, Some(WED + 3660));
    }

    #[test]
    fn history_keeps_the_last_runs_per_schedule() {
        let mut store = Store {
            schedules: vec![schedule("s1", "* * * * *", None)],
            ..Default::default()
        };
        for n in 0..(RUNS_KEPT as u64 + 5) {
            let mut run = store_run("s1", WED + n * 60);
            run.state = RunState::Passed;
            store.runs.push(run);
        }
        store.runs.push(store_run("gone", WED));
        store.prune();
        assert_eq!(store.runs.len(), RUNS_KEPT);
        assert!(
            store.runs.iter().all(|r| r.scheduled_for >= WED + 5 * 60),
            "oldest dropped"
        );
    }

    fn store_run(schedule_id: &str, at: u64) -> Run {
        Run {
            id: format!("r{at}"),
            schedule_id: schedule_id.into(),
            scheduled_for: at,
            deadline: at + 3600,
            state: RunState::Pending,
            manual: false,
            attempts: 0,
            next_attempt_at: None,
            reason: None,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            exit_code: None,
            summary: None,
            error: None,
            artifacts: None,
        }
    }

    #[test]
    fn the_gate_reads_status_like_a_careful_agent() {
        let me = "schedule-s1";
        let s = |v: serde_json::Value| gate(&v, me);
        assert_eq!(
            s(serde_json::json!({"drivable": true, "owner": null})),
            Gate::Run
        );
        assert_eq!(
            s(serde_json::json!({"drivable": true, "owner": me})),
            Gate::Run
        );
        assert!(
            matches!(s(serde_json::json!({"drivable": true, "owner": "claude-7"})), Gate::Postpone(w) if w.contains("claude-7"))
        );
        assert!(matches!(
            s(serde_json::json!({"drivable": true, "viewer_count": 1})),
            Gate::Postpone(_)
        ));
        assert!(matches!(
            s(serde_json::json!({"drivable": false, "released": true, "human_handoff": true})),
            Gate::Postpone(_)
        ));
        assert_eq!(
            s(serde_json::json!({"drivable": false, "device_state": "locked"})),
            Gate::WaitUnlock
        );
        assert_eq!(
            s(
                serde_json::json!({"drivable": false, "setup_blocked_on": "locked", "device_state": "blocked"})
            ),
            Gate::WaitUnlock
        );
        assert_eq!(
            s(serde_json::json!({"drivable": false, "device_state": "released"})),
            Gate::Reconnect
        );
        assert!(
            matches!(s(serde_json::json!({"drivable": false, "device_state": "blocked", "setup_blocked_on": "trust"})), Gate::Postpone(w) if w.contains("trust"))
        );
    }

    fn request(cron: &str, kind: JobKind, target: &str) -> CreateSchedule {
        CreateSchedule {
            cron: cron.into(),
            kind,
            target: target.into(),
            inputs: BTreeMap::new(),
            confirm_side_effects: false,
            webhook: None,
            name: None,
            window_mins: None,
            enabled: None,
        }
    }

    #[test]
    fn create_requests_are_checked_before_anything_runs() {
        assert!(check_request(&request("0 9 * * *", JobKind::Flow, "system/x")).is_ok());
        assert_eq!(
            check_request(&request("0 25 * * *", JobKind::Flow, "system/x"))
                .unwrap_err()
                .0,
            "invalid_cron"
        );
        assert_eq!(
            check_request(&request("0 9 * * *", JobKind::Test, "rel/suite.yaml"))
                .unwrap_err()
                .0,
            "invalid_target"
        );
        let mut inputs = request("0 9 * * *", JobKind::Test, "/abs/suite.yaml");
        inputs.inputs.insert("q".into(), "x".into());
        assert_eq!(check_request(&inputs).unwrap_err().0, "invalid_inputs");
        let mut hook = request("0 9 * * *", JobKind::Flow, "system/x");
        hook.webhook = Some("file:///etc/passwd".into());
        assert_eq!(check_request(&hook).unwrap_err().0, "invalid_webhook");
    }

    #[test]
    fn job_commands_pass_confirm_and_inputs_through() {
        let mut job = schedule("s1", "* * * * *", None);
        job.inputs.insert("query".into(), "Health".into());
        job.confirm_side_effects = true;
        let args = Scheduler::command_args(&job, Path::new("/tmp/a"));
        assert_eq!(
            args,
            [
                "flow",
                "run",
                "system/x",
                "--input",
                "query=Health",
                "--confirm",
                "--artifacts-dir",
                "/tmp/a"
            ]
        );
        job.kind = JobKind::Test;
        job.inputs.clear();
        job.confirm_side_effects = false;
        job.target = "/s.yaml".into();
        assert_eq!(
            Scheduler::command_args(&job, Path::new("/tmp/a")),
            ["test", "/s.yaml", "--json", "--artifacts-dir", "/tmp/a"]
        );
        assert_eq!(
            Scheduler::summarize(JobKind::Test, r#"{"passed":2,"failed":1}"#).as_deref(),
            Some("2 passed, 1 failed")
        );
        assert_eq!(
            Scheduler::summarize(
                JobKind::Flow,
                r#"{"ok":false,"error":"expectation_timeout","failed_step":3}"#
            )
            .as_deref(),
            Some("flow failed: expectation_timeout at step 3")
        );
    }
}

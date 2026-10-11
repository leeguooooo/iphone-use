//! The phone's system log, from `com.apple.syslog_relay`, kept in memory for
//! an agent to read back: a ring of the last [`RING_CAPACITY`] lines with a
//! sequence number each, read with a cursor and filters.
//!
//! The relay streams text records separated by NUL bytes (each ending in a
//! newline), in the classic syslog shape:
//! `Oct 11 07:01:02 iPhone SpringBoard(FrontBoard)[57] <Notice>: message`.
//! Lines that do not parse are kept whole as the message.
//!
//! One capture runs per daemon ([`global`]). It stops on its own after
//! [`IDLE_STOP`] without a read, so a forgotten capture does not hold the
//! relay open. Lines never go to the daemon's own log.

use anyhow::Result;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const SERVICE: &str = "com.apple.syslog_relay";
pub const RING_CAPACITY: usize = 5_000;
pub const IDLE_STOP: Duration = Duration::from_secs(15 * 60);
const DEFAULT_LIMIT: usize = 200;
/// A message longer than this is cut (bounds the ring's memory).
const MAX_MESSAGE: usize = 4 * 1024;
/// Bytes buffered without a separator before they are taken as one record.
const MAX_PENDING: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogLine {
    pub seq: u64,
    /// `Oct 11 07:01:02`, the phone's local time (no year).
    pub ts: Option<String>,
    pub process: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    pub pid: Option<u32>,
    /// Lowercase: `debug`, `info`, `notice`, `warning`, `error`, `fault`, …
    pub level: Option<String>,
    pub message: String,
}

/// The parsed fields of one record, before it gets a sequence number.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub ts: Option<String>,
    pub device: Option<String>,
    pub process: Option<String>,
    pub subsystem: Option<String>,
    pub pid: Option<u32>,
    pub level: Option<String>,
    pub message: String,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Whether `text` starts with `Mon dd HH:MM:SS` (the day may be space-padded).
fn starts_with_timestamp(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() < 15 || !text.is_char_boundary(15) {
        return false;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    MONTHS.contains(&&text[..3])
        && b[3] == b' '
        && (digit(4) || b[4] == b' ')
        && digit(5)
        && b[6] == b' '
        && digit(7)
        && digit(8)
        && b[9] == b':'
        && digit(10)
        && digit(11)
        && b[12] == b':'
        && digit(13)
        && digit(14)
}

/// Parse one relay record; anything unrecognized comes back as a raw message.
pub fn parse_line(line: &str) -> Parsed {
    parse_fields(line).unwrap_or_else(|| Parsed {
        message: line.to_string(),
        ..Parsed::default()
    })
}

fn parse_fields(line: &str) -> Option<Parsed> {
    if !starts_with_timestamp(line) {
        return None;
    }
    let ts = &line[..15];
    let rest = line[15..].strip_prefix(' ')?;
    // `device process(sub)[pid] <Level>: message`: the first `]` followed by
    // ` <Word>:` ends the header.
    let mut search = 0;
    let (header_end, level, message_start) = loop {
        let close = search + rest[search..].find("] <")?;
        let after = &rest[close + 3..];
        if let Some(end) = after.find(">:") {
            let level = &after[..end];
            if !level.is_empty() && level.chars().all(|c| c.is_ascii_alphabetic()) {
                break (close, level, close + 3 + end + 2);
            }
        }
        search = close + 1;
    };
    let head = &rest[..header_end];
    let open = head.rfind('[')?;
    let pid = head[open + 1..].parse::<u32>().ok()?;
    let head = &head[..open];
    // `device process(sub)`, the process possibly with a parenthesized
    // subsystem that may itself hold spaces.
    let (who, subsystem) = match head.strip_suffix(')') {
        Some(inner) => {
            let open = matching_open(inner)?;
            (&inner[..open], Some(inner[open + 1..].to_string()))
        }
        None => (head, None),
    };
    let (device, process) = who.rsplit_once(' ')?;
    if process.is_empty() {
        return None;
    }
    let message = &rest[message_start..];
    Some(Parsed {
        ts: Some(ts.to_string()),
        device: Some(device.to_string()),
        process: Some(process.to_string()),
        subsystem: subsystem.filter(|s| !s.is_empty()),
        pid: Some(pid),
        level: Some(level.to_ascii_lowercase()),
        message: message.strip_prefix(' ').unwrap_or(message).to_string(),
    })
}

/// The index of the `(` that opens the group closing at the end of `inner`
/// (whose final `)` has been stripped).
fn matching_open(inner: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in inner.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => return Some(i),
            '(' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Severity order for the `level` filter; unknown levels rank as `notice`.
fn severity(level: &str) -> u8 {
    match level {
        "debug" => 0,
        "info" => 1,
        "notice" | "default" => 2,
        "warning" | "warn" => 3,
        "error" | "err" => 4,
        "fault" | "critical" | "crit" | "alert" | "emergency" | "emerg" => 5,
        _ => 2,
    }
}

/// Split relay bytes into records. NUL separates records; a newline followed
/// by a new timestamp does too (some iOS versions omit the NUL), while other
/// newlines stay inside a multi-line message.
fn split_records(chunk: &str) -> Vec<String> {
    let mut out = Vec::new();
    for piece in chunk.split('\0') {
        let mut current = String::new();
        for (i, part) in piece.split('\n').enumerate() {
            if i > 0 && starts_with_timestamp(part) && !current.is_empty() {
                out.push(std::mem::take(&mut current));
            } else if i > 0 {
                current.push('\n');
            }
            current.push_str(part);
        }
        let trimmed = current.trim_end_matches(['\n', '\r']);
        if !trimmed.trim().is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn cut(mut message: String) -> String {
    if message.len() > MAX_MESSAGE {
        let mut end = MAX_MESSAGE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    message
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogFilter {
    /// Return lines after this sequence number (the previous read's `next`).
    pub since: Option<u64>,
    /// Exact process name, case-insensitive.
    pub process: Option<String>,
    /// Case-insensitive substring of the message.
    pub contains: Option<String>,
    /// This level or more severe (`error` also returns `fault`).
    pub level: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogRead {
    pub items: Vec<LogLine>,
    /// Pass as `since` to continue after this read.
    pub next: u64,
    pub capturing: bool,
    /// Lines after `since` no longer held (the ring dropped them, or a clear).
    #[serde(skip_serializing_if = "is_zero")]
    pub missed: u64,
    /// Why the last capture ended on its own (relay closed, idle stop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The line store: sequence numbers start at 1 and never repeat, even across
/// a clear, so a reader's cursor stays meaningful.
#[derive(Debug)]
pub struct Ring {
    lines: VecDeque<LogLine>,
    capacity: usize,
    last_seq: u64,
}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity.min(RING_CAPACITY)),
            capacity: capacity.max(1),
            last_seq: 0,
        }
    }

    pub fn push(&mut self, raw: &str) {
        self.push_parsed(parse_line(raw));
    }

    /// Store a record another source already parsed (the simulator's
    /// `log stream`, see `crate::simulator`).
    pub fn push_parsed(&mut self, parsed: Parsed) {
        self.last_seq += 1;
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(LogLine {
            seq: self.last_seq,
            ts: parsed.ts,
            process: parsed.process,
            subsystem: parsed.subsystem,
            pid: parsed.pid,
            level: parsed.level,
            message: cut(parsed.message),
        });
    }

    pub fn clear(&mut self) {
        self.lines.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Matching lines and the cursor to continue from. Without `since` the
    /// newest `limit` matches come back; with it, the oldest `limit` after it
    /// (so paging forward never skips a line).
    pub fn read(&self, filter: &LogFilter) -> (Vec<LogLine>, u64, u64) {
        let limit = filter
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, RING_CAPACITY);
        let process = filter
            .process
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(str::to_lowercase);
        let contains = filter
            .contains
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(str::to_lowercase);
        let min_level = filter
            .level
            .as_deref()
            .filter(|l| !l.is_empty())
            .map(|l| severity(&l.to_ascii_lowercase()));
        let matches = |line: &LogLine| {
            process.as_ref().is_none_or(|want| {
                line.process
                    .as_deref()
                    .is_some_and(|p| p.to_lowercase() == *want)
            }) && contains
                .as_ref()
                .is_none_or(|want| line.message.to_lowercase().contains(want))
                && min_level.is_none_or(|min| {
                    line.level
                        .as_deref()
                        .is_some_and(|level| severity(level) >= min)
                })
        };
        let since = filter.since.unwrap_or(0);
        let oldest = self.lines.front().map_or(self.last_seq + 1, |l| l.seq);
        let missed = if filter.since.is_some() && since.saturating_add(1) < oldest {
            // Lines after the cursor that are no longer held (dropped by
            // the ring, or cleared).
            oldest - since - 1
        } else {
            0
        };
        let after = self.lines.iter().filter(|line| line.seq > since);
        if filter.since.is_some() {
            let mut items = Vec::new();
            let mut next = self.last_seq.max(since);
            for line in after {
                if !matches(line) {
                    continue;
                }
                if items.len() == limit {
                    // More remain: continue right after the last one returned.
                    next = items.last().map_or(since, |l: &LogLine| l.seq);
                    break;
                }
                items.push(line.clone());
            }
            (items, next, missed)
        } else {
            let mut items: Vec<LogLine> = after
                .rev()
                .filter(|l| matches(l))
                .take(limit)
                .cloned()
                .collect();
            items.reverse();
            (items, self.last_seq, 0)
        }
    }
}

struct State {
    ring: Ring,
    udid: Option<String>,
    /// Bumped by every start/stop; a pump whose generation is stale exits.
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
    last_read: Instant,
    stopped: Option<String>,
}

/// The process-wide syslog capture.
pub struct SyslogCapture {
    state: Arc<Mutex<State>>,
    idle_stop: Duration,
}

/// The daemon's one capture.
pub fn global() -> &'static SyslogCapture {
    static CAPTURE: OnceLock<SyslogCapture> = OnceLock::new();
    CAPTURE.get_or_init(|| SyslogCapture::new(RING_CAPACITY, IDLE_STOP))
}

impl SyslogCapture {
    pub fn new(capacity: usize, idle_stop: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                ring: Ring::new(capacity),
                udid: None,
                generation: 0,
                task: None,
                last_read: Instant::now(),
                stopped: None,
            })),
            idle_stop,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start capturing `udid`'s log. Already capturing that phone: kept
    /// running. Another phone: that capture stops first.
    pub async fn start(&self, udid: &str) -> Result<()> {
        {
            let mut state = self.lock();
            if state.udid.as_deref() == Some(udid) && state.task.is_some() {
                state.last_read = Instant::now();
                return Ok(());
            }
        }
        let stream = crate::lockdown::open_service(udid, SERVICE).await?;
        self.start_with(udid, stream);
        Ok(())
    }

    /// Start capturing from an already open relay stream.
    pub fn start_with<S: AsyncRead + Send + Unpin + 'static>(&self, udid: &str, stream: S) {
        let mut state = self.lock();
        if let Some(task) = state.task.take() {
            task.abort();
        }
        state.generation += 1;
        state.udid = Some(udid.to_string());
        state.last_read = Instant::now();
        state.stopped = None;
        let generation = state.generation;
        state.task = Some(tokio::spawn(pump(
            self.state.clone(),
            generation,
            stream,
            self.idle_stop,
        )));
    }

    /// Start capturing newline-separated lines from another source than the
    /// relay (a simulator's `log stream`), each parsed with `parse` (`None`
    /// skips a line). `guard` lives as long as the capture: a child process
    /// with kill-on-drop ends with it.
    pub fn start_lines<S, G>(&self, source: &str, stream: S, parse: LineParser, guard: G)
    where
        S: AsyncRead + Send + Unpin + 'static,
        G: Send + 'static,
    {
        let mut state = self.lock();
        if let Some(task) = state.task.take() {
            task.abort();
        }
        state.generation += 1;
        state.udid = Some(source.to_string());
        state.last_read = Instant::now();
        state.stopped = None;
        let generation = state.generation;
        state.task = Some(tokio::spawn(pump_lines(
            self.state.clone(),
            generation,
            stream,
            parse,
            self.idle_stop,
            guard,
        )));
    }

    /// Stop capturing; the lines read so far stay.
    pub fn stop(&self) {
        let mut state = self.lock();
        if let Some(task) = state.task.take() {
            task.abort();
        }
        state.generation += 1;
        state.udid = None;
    }

    /// Drop the buffered lines (the capture keeps running).
    pub fn clear(&self) {
        self.lock().ring.clear();
    }

    pub fn capturing(&self) -> bool {
        self.lock().task.is_some()
    }

    /// The phone being captured.
    pub fn udid(&self) -> Option<String> {
        let state = self.lock();
        state.task.as_ref().and(state.udid.clone())
    }

    /// Lines matching `filter`; counts as activity for the idle stop.
    pub fn read(&self, filter: &LogFilter) -> LogRead {
        let mut state = self.lock();
        state.last_read = Instant::now();
        let (items, next, missed) = state.ring.read(filter);
        LogRead {
            items,
            next,
            capturing: state.task.is_some(),
            missed,
            stopped: state.stopped.clone(),
        }
    }
}

/// Read the relay into the ring until it closes, the capture is replaced or
/// stopped, or nobody has read for `idle_stop`.
async fn pump<S: AsyncRead + Unpin>(
    state: Arc<Mutex<State>>,
    generation: u64,
    mut stream: S,
    idle_stop: Duration,
) {
    let check_every = (idle_stop / 4).clamp(Duration::from_millis(10), Duration::from_secs(30));
    let mut ticker = tokio::time::interval(check_every);
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let reason = loop {
        tokio::select! {
            read = stream.read(&mut chunk) => match read {
                Ok(0) => break "the phone closed the log relay".to_string(),
                Ok(n) => {
                    pending.extend_from_slice(&chunk[..n]);
                    // Keep a partial record (after the last NUL) for the next read.
                    let cut_at = match pending.iter().rposition(|b| *b == 0) {
                        Some(at) => at + 1,
                        None if pending.len() > MAX_PENDING => pending.len(),
                        None => continue,
                    };
                    let complete: Vec<u8> = pending.drain(..cut_at).collect();
                    let text = String::from_utf8_lossy(&complete);
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.generation != generation {
                        return;
                    }
                    for record in split_records(&text) {
                        guard.ring.push(&record);
                    }
                }
                Err(error) => break format!("the log relay failed: {error}"),
            },
            _ = ticker.tick() => {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.generation != generation {
                    return;
                }
                if guard.last_read.elapsed() >= idle_stop {
                    break format!("stopped after {} min without a read", idle_stop.as_secs() / 60);
                }
            }
        }
    };
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.generation == generation {
        if !pending.is_empty() {
            for record in split_records(&String::from_utf8_lossy(&pending)) {
                guard.ring.push(&record);
            }
        }
        guard.generation += 1;
        guard.task = None;
        guard.udid = None;
        guard.stopped = Some(reason);
    }
}

/// Parses one line of a line-oriented log source; `None` skips it.
pub type LineParser = fn(&str) -> Option<Parsed>;

/// [`pump`] for a newline-separated source: same cursor, idle stop and
/// generation rules. `guard` is dropped when the pump ends or is aborted.
async fn pump_lines<S: AsyncRead + Unpin, G>(
    state: Arc<Mutex<State>>,
    generation: u64,
    mut stream: S,
    parse: LineParser,
    idle_stop: Duration,
    guard: G,
) {
    let _guard = guard;
    let check_every = (idle_stop / 4).clamp(Duration::from_millis(10), Duration::from_secs(30));
    let mut ticker = tokio::time::interval(check_every);
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let push_lines = |guard: &mut State, bytes: &[u8]| {
        for line in String::from_utf8_lossy(bytes).split('\n') {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            if let Some(parsed) = parse(line) {
                guard.ring.push_parsed(parsed);
            }
        }
    };
    let reason = loop {
        tokio::select! {
            read = stream.read(&mut chunk) => match read {
                Ok(0) => break "the log stream ended".to_string(),
                Ok(n) => {
                    pending.extend_from_slice(&chunk[..n]);
                    let cut_at = match pending.iter().rposition(|b| *b == b'\n') {
                        Some(at) => at + 1,
                        None if pending.len() > MAX_PENDING => pending.len(),
                        None => continue,
                    };
                    let complete: Vec<u8> = pending.drain(..cut_at).collect();
                    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.generation != generation {
                        return;
                    }
                    push_lines(&mut guard, &complete);
                }
                Err(error) => break format!("the log stream failed: {error}"),
            },
            _ = ticker.tick() => {
                let guard = state.lock().unwrap_or_else(|e| e.into_inner());
                if guard.generation != generation {
                    return;
                }
                if guard.last_read.elapsed() >= idle_stop {
                    break format!("stopped after {} min without a read", idle_stop.as_secs() / 60);
                }
            }
        }
    };
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.generation == generation {
        push_lines(&mut guard, &pending);
        guard.generation += 1;
        guard.task = None;
        guard.udid = None;
        guard.stopped = Some(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// `#[tokio::test]` expands to `::core::…`, which this crate's `core`
    /// dependency shadows; tests build their own runtime.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_relay_line_yields_its_fields() {
        let parsed = parse_line(
            "Oct 11 07:01:02 Davians-iPhone SpringBoard(FrontBoard)[57] <Notice>: Scene became active",
        );
        assert_eq!(
            parsed,
            Parsed {
                ts: Some("Oct 11 07:01:02".into()),
                device: Some("Davians-iPhone".into()),
                process: Some("SpringBoard".into()),
                subsystem: Some("FrontBoard".into()),
                pid: Some(57),
                level: Some("notice".into()),
                message: "Scene became active".into(),
            }
        );
    }

    #[test]
    fn variants_parse_padded_days_no_subsystem_and_brackets_in_the_message() {
        let parsed = parse_line("Oct  1 23:59:59 iPhone kernel[0] <Error>: a [b] <c>: d");
        assert_eq!(parsed.ts.as_deref(), Some("Oct  1 23:59:59"));
        assert_eq!(parsed.process.as_deref(), Some("kernel"));
        assert_eq!(parsed.subsystem, None);
        assert_eq!(parsed.pid, Some(0));
        assert_eq!(parsed.level.as_deref(), Some("error"));
        assert_eq!(parsed.message, "a [b] <c>: d");

        let nested =
            parse_line("Oct 11 07:01:02 My iPhone backboardd(CoreBrightness (x))[66] <Fault>: m");
        assert_eq!(nested.device.as_deref(), Some("My iPhone"));
        assert_eq!(nested.process.as_deref(), Some("backboardd"));
        assert_eq!(nested.subsystem.as_deref(), Some("CoreBrightness (x)"));
        assert_eq!(nested.level.as_deref(), Some("fault"));

        let empty = parse_line("Oct 11 07:01:02 iPhone MyApp[123] <Info>:");
        assert_eq!(empty.process.as_deref(), Some("MyApp"));
        assert_eq!(empty.message, "");
    }

    #[test]
    fn unparsed_lines_stay_raw() {
        for raw in [
            "=== hello",
            "Foo 11 07:01:02 iPhone x[1] <Notice>: m",
            "Oct 11 07:01:02 iPhone x[abc] <Notice>: m",
            "Oct 11 07:01:02 iPhone no header here",
            "Oct 11 07:01",
        ] {
            let parsed = parse_line(raw);
            assert_eq!(parsed.message, raw);
            assert_eq!(parsed.process, None, "{raw}");
            assert_eq!(parsed.ts, None);
        }
    }

    #[test]
    fn records_split_on_nul_and_on_a_newline_before_a_timestamp() {
        let text = "Oct 11 07:01:02 i a[1] <Notice>: one\n\0Oct 11 07:01:03 i a[1] <Notice>: two\nsecond line\nOct 11 07:01:04 i b[2] <Error>: three\n\0\0";
        assert_eq!(
            split_records(text),
            [
                "Oct 11 07:01:02 i a[1] <Notice>: one",
                "Oct 11 07:01:03 i a[1] <Notice>: two\nsecond line",
                "Oct 11 07:01:04 i b[2] <Error>: three",
            ]
        );
    }

    fn line(process: &str, level: &str, message: &str) -> String {
        format!("Oct 11 07:01:02 iPhone {process}[1] <{level}>: {message}")
    }

    #[test]
    fn the_ring_keeps_the_newest_lines_with_increasing_sequence_numbers() {
        let mut ring = Ring::new(3);
        for i in 1..=5 {
            ring.push(&line("a", "Notice", &i.to_string()));
        }
        assert_eq!(ring.len(), 3);
        let (items, next, missed) = ring.read(&LogFilter::default());
        assert_eq!(items.iter().map(|l| l.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!((next, missed), (5, 0));
        // A cursor older than the ring reports the lines it lost.
        let (items, _, missed) = ring.read(&LogFilter {
            since: Some(1),
            ..Default::default()
        });
        assert_eq!(items.len(), 3);
        assert_eq!(missed, 1);
        ring.clear();
        assert!(ring.is_empty());
        ring.push(&line("a", "Notice", "6"));
        let (items, next, _) = ring.read(&LogFilter {
            since: Some(5),
            ..Default::default()
        });
        assert_eq!(items[0].seq, 6, "sequence numbers continue across a clear");
        assert_eq!(next, 6);
    }

    #[test]
    fn filters_and_cursor_paging() {
        let mut ring = Ring::new(100);
        ring.push(&line("MyApp", "Notice", "launch"));
        ring.push(&line("SpringBoard", "Error", "boom"));
        ring.push(&line("MyApp", "Error", "Network FAILED"));
        ring.push(&line("MyApp", "Fault", "crash"));
        ring.push("raw text line");

        let read = |filter: LogFilter| ring.read(&filter);
        let (items, _, _) = read(LogFilter {
            process: Some("myapp".into()),
            ..Default::default()
        });
        assert_eq!(items.len(), 3);
        let (items, _, _) = read(LogFilter {
            level: Some("error".into()),
            ..Default::default()
        });
        assert_eq!(
            items.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            ["boom", "Network FAILED", "crash"],
            "error and more severe; raw lines have no level"
        );
        let (items, _, _) = read(LogFilter {
            contains: Some("failed".into()),
            ..Default::default()
        });
        assert_eq!(items.len(), 1);
        let (items, _, _) = read(LogFilter {
            contains: Some("raw text".into()),
            ..Default::default()
        });
        assert_eq!(items[0].process, None);

        // Without a cursor: the newest `limit`.
        let (items, next, _) = read(LogFilter {
            limit: Some(2),
            ..Default::default()
        });
        assert_eq!(items.iter().map(|l| l.seq).collect::<Vec<_>>(), [4, 5]);
        assert_eq!(next, 5);
        // With a cursor: the oldest `limit` after it, then the rest.
        let (items, next, _) = read(LogFilter {
            since: Some(0),
            limit: Some(2),
            ..Default::default()
        });
        assert_eq!(items.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(next, 2);
        let (items, next, _) = read(LogFilter {
            since: Some(next),
            limit: Some(10),
            ..Default::default()
        });
        assert_eq!(items.iter().map(|l| l.seq).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(next, 5);
        // Nothing new: the cursor stays, and filtered-out lines move it on.
        let (items, next, _) = read(LogFilter {
            since: Some(2),
            process: Some("SpringBoard".into()),
            ..Default::default()
        });
        assert!(items.is_empty());
        assert_eq!(next, 5);
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let mut ring = Ring::new(2);
        ring.push(&"é".repeat(MAX_MESSAGE));
        let (items, _, _) = ring.read(&LogFilter::default());
        assert!(items[0].message.len() <= MAX_MESSAGE + '…'.len_utf8());
        assert!(items[0].message.ends_with('…'));
    }

    async fn wait_for(capture: &SyslogCapture, done: impl Fn(&SyslogCapture) -> bool) {
        for _ in 0..200 {
            if done(capture) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    #[test]
    fn a_capture_fills_the_ring_from_the_relay_and_stops_when_it_closes() {
        block_on(a_capture_fills_the_ring_from_the_relay_and_stops_when_it_closes_body());
    }

    async fn a_capture_fills_the_ring_from_the_relay_and_stops_when_it_closes_body() {
        let capture = SyslogCapture::new(100, Duration::from_secs(60));
        let (mut device, host) = tokio::io::duplex(1 << 16);
        capture.start_with("UDID-1", host);
        assert!(capture.capturing());
        assert_eq!(capture.udid().as_deref(), Some("UDID-1"));
        // A record split across writes is joined.
        device
            .write_all(b"Oct 11 07:01:02 iPhone MyApp[9] <Error>: first\n\0Oct 11 07:01:03 iPh")
            .await
            .unwrap();
        device
            .write_all(b"one MyApp[9] <Notice>: second\n\0")
            .await
            .unwrap();
        wait_for(&capture, |c| c.read(&LogFilter::default()).items.len() == 2).await;
        let read = capture.read(&LogFilter::default());
        assert_eq!(read.items[1].message, "second");
        assert_eq!(read.items[1].pid, Some(9));
        assert!(read.capturing);

        capture.clear();
        assert!(capture.read(&LogFilter::default()).items.is_empty());

        drop(device);
        wait_for(&capture, |c| !c.capturing()).await;
        let read = capture.read(&LogFilter::default());
        assert!(!read.capturing);
        assert!(read.stopped.unwrap().contains("closed"));
    }

    #[test]
    fn stop_keeps_the_lines_and_a_restart_replaces_the_stream() {
        block_on(stop_keeps_the_lines_and_a_restart_replaces_the_stream_body());
    }

    async fn stop_keeps_the_lines_and_a_restart_replaces_the_stream_body() {
        let capture = SyslogCapture::new(100, Duration::from_secs(60));
        let (mut device, host) = tokio::io::duplex(1 << 16);
        capture.start_with("UDID-1", host);
        device.write_all(b"one\n\0").await.unwrap();
        wait_for(&capture, |c| c.read(&LogFilter::default()).items.len() == 1).await;
        capture.stop();
        assert!(!capture.capturing());
        assert_eq!(capture.udid(), None);
        // The old stream no longer feeds the ring.
        let _ = device.write_all(b"two\n\0").await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(capture.read(&LogFilter::default()).items.len(), 1);
        assert_eq!(
            capture.read(&LogFilter::default()).stopped,
            None,
            "a requested stop is not reported"
        );

        let (mut device2, host2) = tokio::io::duplex(1 << 16);
        capture.start_with("UDID-2", host2);
        device2.write_all(b"three\n\0").await.unwrap();
        wait_for(&capture, |c| c.read(&LogFilter::default()).items.len() == 2).await;
        assert_eq!(capture.udid().as_deref(), Some("UDID-2"));
    }

    #[test]
    fn a_capture_nobody_reads_stops_on_its_own() {
        block_on(a_capture_nobody_reads_stops_on_its_own_body());
    }

    async fn a_capture_nobody_reads_stops_on_its_own_body() {
        let capture = SyslogCapture::new(100, Duration::from_millis(60));
        let (_device, host) = tokio::io::duplex(1 << 16);
        capture.start_with("UDID-1", host);
        // Check without `read`, which would count as activity.
        for _ in 0..100 {
            if !capture.capturing() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!capture.capturing());
        let read = capture.read(&LogFilter::default());
        assert!(read.stopped.unwrap().contains("without a read"));
    }

    #[test]
    fn a_line_source_feeds_the_same_ring_and_drops_its_guard_when_it_ends() {
        block_on(a_line_source_feeds_the_same_ring_and_drops_its_guard_when_it_ends_body());
    }

    async fn a_line_source_feeds_the_same_ring_and_drops_its_guard_when_it_ends_body() {
        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        fn parse(line: &str) -> Option<Parsed> {
            (!line.starts_with("header")).then(|| Parsed {
                process: Some("Sim".into()),
                level: Some("error".into()),
                message: line.to_string(),
                ..Parsed::default()
            })
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture = SyslogCapture::new(100, Duration::from_secs(60));
        let (mut source, host) = tokio::io::duplex(1 << 16);
        capture.start_lines("SIM-UDID", host, parse, Flag(dropped.clone()));
        assert_eq!(capture.udid().as_deref(), Some("SIM-UDID"));
        source.write_all(b"header line\nfirst\r\nsec").await.unwrap();
        source.write_all(b"ond\n").await.unwrap();
        wait_for(&capture, |c| c.read(&LogFilter::default()).items.len() == 2).await;
        let read = capture.read(&LogFilter {
            level: Some("error".into()),
            ..LogFilter::default()
        });
        assert_eq!(read.items[0].message, "first");
        assert_eq!(read.items[1].message, "second");
        assert_eq!(read.items[1].process.as_deref(), Some("Sim"));
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));

        source.write_all(b"tail without newline").await.unwrap();
        drop(source);
        wait_for(&capture, |c| !c.capturing()).await;
        let read = capture.read(&LogFilter::default());
        assert_eq!(read.items.last().unwrap().message, "tail without newline");
        assert!(read.stopped.unwrap().contains("ended"));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

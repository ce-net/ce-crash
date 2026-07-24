//! ce-crash — crash capsule construction, grouping, and rendering.
//!
//! A CRASH CAPSULE is one JSON document: what died, how (exit/signal/exception), and the
//! evidence (stderr tail or stack, argv, environment manifest — env var NAMES only, never
//! values). Capsules are content-addressed mesh blobs; the INDEX is a `level=error` ce-debug
//! event carrying `fields.crash_cid`, so crashes appear in the normal error stream and their
//! full evidence is one blob fetch away. The same shape is produced by the wrapper-runner
//! here (any language, zero code changes) and by the SDK last-words hooks
//! (`clients/py/ce_crash.py`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The ce-debug suite service this crate reports into and queries.
pub const SERVICE: &str = "ce.debug";
pub const CTL_TOPIC: &str = "ce.debug/ctl";
/// How many trailing stderr lines the wrapper-runner keeps.
pub const STDERR_TAIL_LINES: usize = 100;

/// One crash capsule. Every field the Python hooks or the wrapper-runner may not have is
/// optional/defaulted, so one struct parses both shapes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Capsule {
    pub app: String,
    /// Stamped by the collector on the index event; often empty inside the capsule itself.
    #[serde(default)]
    pub node: String,
    /// unix millis at crash
    pub ts_ms: u64,
    /// unix millis at process start (wrapper-runner only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ts_ms: Option<u64>,
    /// "exit" | "signal" | "exception"
    pub exit_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// exception class name (SDK hooks only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exc_type: Option<String>,
    /// one-line what-happened (fingerprinted by ce-debug grouping)
    pub msg: String,
    /// stack trace (SDK hooks only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// last N stderr lines (wrapper-runner only)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stderr_tail: Vec<String>,
    pub argv: Vec<String>,
    /// interpreter version (SDK hooks only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub cwd: String,
    /// environment variable NAMES only — never values
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// last events this process emitted via its ce_debug buffer (SDK hooks only)
    #[serde(default)]
    pub recent: Vec<Value>,
}

/// Build the wrapper-runner capsule from an observed child exit.
pub fn build_run_capsule(
    app: &str,
    argv: &[String],
    exit_code: Option<i32>,
    signal: Option<i32>,
    stderr_tail: Vec<String>,
    start_ts_ms: u64,
    ts_ms: u64,
) -> Capsule {
    let status_line = match (signal, exit_code) {
        (Some(s), _) => format!("killed by signal {s}"),
        (None, Some(c)) => format!("exit code {c}"),
        (None, None) => "abnormal exit".to_string(),
    };
    // The last non-empty stderr line is usually the actual error ("RuntimeError: ...");
    // fingerprint on that so `ce-crash list` groups by cause, not by exit code.
    let msg = stderr_tail
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| status_line.clone());
    Capsule {
        app: app.to_string(),
        node: String::new(),
        ts_ms,
        start_ts_ms: Some(start_ts_ms),
        exit_kind: if signal.is_some() { "signal" } else { "exit" }.to_string(),
        exit_code,
        signal,
        exc_type: None,
        msg,
        stack: None,
        stderr_tail,
        argv: argv.to_vec(),
        python: None,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        cwd: std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        env_keys: {
            let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
            keys.sort();
            keys
        },
        recent: Vec::new(),
    }
}

/// The ce-debug index event for a sealed capsule: level=error with `fields.crash_cid`.
pub fn index_event(capsule: &Capsule, cid: &str) -> Value {
    let mut fields = json!({"crash_cid": cid, "exit_kind": capsule.exit_kind});
    if let Some(c) = capsule.exit_code {
        fields["exit_code"] = json!(c);
    }
    if let Some(s) = capsule.signal {
        fields["signal"] = json!(s);
    }
    if let Some(t) = &capsule.exc_type {
        fields["exc_type"] = json!(t);
    }
    let mut e = json!({
        "ts_ms": capsule.ts_ms, "app": capsule.app, "node": "",
        "level": "error", "msg": capsule.msg, "fields": fields,
    });
    // Stack for the index event: the SDK stack if present, else the stderr tail.
    if let Some(stack) = &capsule.stack {
        e["stack"] = json!(stack);
    } else if !capsule.stderr_tail.is_empty() {
        e["stack"] = json!(capsule.stderr_tail.join("\n"));
    }
    e
}

/// One group of same-cause crashes, mirrored from ce-debug's error grouping
/// (fingerprint = app + first line of msg).
#[derive(Debug, Clone, Serialize)]
pub struct CrashGroup {
    pub fingerprint: String,
    pub app: String,
    pub count: u64,
    pub last_ts_ms: u64,
    pub last_cid: String,
    pub last_node: String,
    pub sample_msg: String,
}

fn fingerprint(app: &str, msg: &str) -> String {
    let first = msg.lines().next().unwrap_or_default();
    format!("{}:{}", app, first.chars().take(120).collect::<String>())
}

/// Group crash events (those carrying `fields.crash_cid`) by fingerprint, newest first.
/// Non-crash error events are ignored.
pub fn group_crashes(events: &[Value]) -> Vec<CrashGroup> {
    let mut groups: Vec<CrashGroup> = Vec::new();
    for e in events {
        let Some(cid) = e["fields"]["crash_cid"].as_str() else { continue };
        let app = e["app"].as_str().unwrap_or_default();
        let msg = e["msg"].as_str().unwrap_or_default();
        let ts = e["ts_ms"].as_u64().unwrap_or(0);
        let fp = fingerprint(app, msg);
        match groups.iter_mut().find(|g| g.fingerprint == fp) {
            Some(g) => {
                g.count += 1;
                if ts >= g.last_ts_ms {
                    g.last_ts_ms = ts;
                    g.last_cid = cid.to_string();
                    g.last_node = e["node"].as_str().unwrap_or_default().to_string();
                    g.sample_msg = msg.to_string();
                }
            }
            None => groups.push(CrashGroup {
                fingerprint: fp,
                app: app.to_string(),
                count: 1,
                last_ts_ms: ts,
                last_cid: cid.to_string(),
                last_node: e["node"].as_str().unwrap_or_default().to_string(),
                sample_msg: msg.to_string(),
            }),
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.last_ts_ms));
    groups
}

/// Render a capsule for humans: what / when / where / evidence.
pub fn render_capsule(c: &Capsule, cid: &str, node: &str) -> String {
    let mut out = String::new();
    let status = match (&c.signal, &c.exit_code) {
        (Some(s), _) => format!("killed by signal {s}"),
        (None, Some(code)) => format!("exit code {code}"),
        (None, None) => c.exit_kind.clone(),
    };
    let cause = c.exc_type.as_deref().map(|t| format!("{t} — ")).unwrap_or_default();
    out.push_str(&format!("{}  {}  ({})\n", c.app, fmt_datetime(c.ts_ms), status));
    out.push_str(&format!("what:     {cause}{}\n", c.msg));
    let node_disp = if node.is_empty() { c.node.as_str() } else { node };
    if !node_disp.is_empty() {
        out.push_str(&format!("node:     {node_disp}\n"));
    }
    if !c.cwd.is_empty() {
        out.push_str(&format!("cwd:      {}\n", c.cwd));
    }
    if !c.argv.is_empty() {
        out.push_str(&format!("argv:     {}\n", c.argv.join(" ")));
    }
    if !c.platform.is_empty() {
        out.push_str(&format!("platform: {}\n", c.platform));
    }
    if let Some(py) = &c.python {
        out.push_str(&format!("python:   {}\n", py.lines().next().unwrap_or(py)));
    }
    if let Some(start) = c.start_ts_ms {
        let alive_s = c.ts_ms.saturating_sub(start) as f64 / 1000.0;
        out.push_str(&format!("lifetime: {alive_s:.1}s (started {})\n", fmt_datetime(start)));
    }
    if let Some(stack) = &c.stack {
        out.push_str("stack:\n");
        for l in stack.lines() {
            out.push_str(&format!("  {l}\n"));
        }
    }
    if !c.stderr_tail.is_empty() {
        out.push_str(&format!("stderr tail (last {} lines):\n", c.stderr_tail.len()));
        for l in &c.stderr_tail {
            out.push_str(&format!("  {l}\n"));
        }
    }
    if !c.recent.is_empty() {
        out.push_str(&format!("recent events ({} buffered before death):\n", c.recent.len()));
        for e in &c.recent {
            out.push_str(&format!(
                "  {}  {:<5} {}\n",
                fmt_time(e["ts_ms"].as_u64().unwrap_or(0)),
                e["level"].as_str().unwrap_or("?"),
                e["msg"].as_str().unwrap_or(""),
            ));
        }
    }
    out.push_str(&format!("env:      {} variables (names recorded, values never)\n", c.env_keys.len()));
    out.push_str(&format!("capsule:  {cid}\n"));
    out
}

/// unix millis -> "YYYY-MM-DD HH:MM:SS UTC" without a date crate (civil-from-days).
pub fn fmt_datetime(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Howard Hinnant's civil_from_days, era-based.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

pub fn fmt_time(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash_event(app: &str, msg: &str, ts: u64, cid: &str) -> Value {
        json!({"ts_ms": ts, "app": app, "node": "n0de", "level": "error", "msg": msg,
               "fields": {"crash_cid": cid, "exit_kind": "exit"}})
    }

    #[test]
    fn run_capsule_msg_is_last_stderr_line() {
        let tail = vec![
            "Traceback (most recent call last):".to_string(),
            "  File \"<string>\", line 1, in <module>".to_string(),
            "RuntimeError: demo crash: sensor voltage out of range".to_string(),
            "".to_string(),
        ];
        let c = build_run_capsule("demo", &["python3".into(), "-c".into()], Some(1), None, tail, 100, 5100);
        assert_eq!(c.msg, "RuntimeError: demo crash: sensor voltage out of range");
        assert_eq!(c.exit_kind, "exit");
        assert_eq!(c.exit_code, Some(1));
        assert_eq!(c.signal, None);
        assert_eq!(c.start_ts_ms, Some(100));
        assert!(!c.env_keys.is_empty());
        assert!(!c.platform.is_empty());
        // env: names only — no '=' (a value separator) may sneak into keys
        assert!(c.env_keys.iter().all(|k| !k.contains('=')));
    }

    #[test]
    fn run_capsule_signal_and_empty_stderr() {
        let c = build_run_capsule("demo", &["sleeper".into()], None, Some(9), vec![], 0, 1);
        assert_eq!(c.exit_kind, "signal");
        assert_eq!(c.msg, "killed by signal 9");
        let c2 = build_run_capsule("demo", &["x".into()], Some(2), None, vec!["  ".into()], 0, 1);
        assert_eq!(c2.msg, "exit code 2");
    }

    #[test]
    fn index_event_carries_cid_and_stderr_stack() {
        let c = build_run_capsule("demo", &["x".into()], Some(1), None,
            vec!["a".into(), "boom".into()], 0, 7);
        let e = index_event(&c, "cid123");
        assert_eq!(e["level"], "error");
        assert_eq!(e["fields"]["crash_cid"], "cid123");
        assert_eq!(e["fields"]["exit_code"], 1);
        assert_eq!(e["msg"], "boom");
        assert_eq!(e["stack"], "a\nboom");
        assert_eq!(e["ts_ms"], 7);
    }

    #[test]
    fn grouping_filters_and_fingerprints() {
        let events = vec![
            crash_event("a", "boom at x", 5, "cid-new"),
            crash_event("a", "boom at x", 1, "cid-old"),
            crash_event("a", "other", 3, "cid-o"),
            crash_event("b", "boom at x", 4, "cid-b"), // same msg, different app = different group
            json!({"ts_ms": 9, "app": "a", "level": "error", "msg": "no cid", "fields": {}}),
        ];
        let groups = group_crashes(&events);
        assert_eq!(groups.len(), 3); // the no-cid error is ignored
        let boom_a = groups.iter().find(|g| g.app == "a" && g.sample_msg.contains("boom")).unwrap();
        assert_eq!(boom_a.count, 2);
        assert_eq!(boom_a.last_ts_ms, 5);
        assert_eq!(boom_a.last_cid, "cid-new");
        assert_eq!(groups[0].last_ts_ms, 5); // newest group first
    }

    #[test]
    fn render_python_shaped_capsule() {
        // Fixture in the exact shape clients/py/ce_crash.py emits.
        let raw = json!({
            "app": "py-app", "node": "", "ts_ms": 1_753_358_400_000u64,
            "exit_kind": "exception", "exc_type": "ValueError",
            "msg": "ValueError: kaboom", "stack": "Traceback...\nValueError: kaboom",
            "argv": ["app.py"], "python": "3.12.0 (main)", "platform": "macOS-15",
            "cwd": "/tmp", "env_keys": ["HOME", "PATH"],
            "recent": [{"ts_ms": 1_753_358_399_000u64, "level": "info", "msg": "starting"}]
        });
        let c: Capsule = serde_json::from_value(raw).unwrap();
        let out = render_capsule(&c, "cidX", "abcd1234");
        assert!(out.contains("what:     ValueError — ValueError: kaboom"));
        assert!(out.contains("node:     abcd1234"));
        assert!(out.contains("stack:"));
        assert!(out.contains("recent events (1 buffered before death):"));
        assert!(out.contains("starting"));
        assert!(out.contains("capsule:  cidX"));
        assert!(out.contains("2 variables (names recorded, values never)"));
    }

    #[test]
    fn render_run_shaped_capsule() {
        let c = build_run_capsule("crash-demo", &["python3".into(), "-c".into(), "raise".into()],
            Some(1), None, vec!["RuntimeError: demo".into()], 1_000, 3_000);
        let out = render_capsule(&c, "cidY", "");
        assert!(out.contains("(exit code 1)"));
        assert!(out.contains("stderr tail (last 1 lines):"));
        assert!(out.contains("RuntimeError: demo"));
        assert!(out.contains("lifetime: 2.0s"));
    }

    #[test]
    fn datetime_render() {
        // 2026-07-24 00:00:00 UTC = 1784851200s
        assert_eq!(fmt_datetime(1_784_851_200_000), "2026-07-24 00:00:00 UTC");
        assert_eq!(fmt_datetime(0), "1970-01-01 00:00:00 UTC");
    }
}

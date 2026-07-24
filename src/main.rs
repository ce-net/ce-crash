//! ce-crash — post-mortem capture and forensics for every app on the mesh.
//!
//!   ce-crash run [--app A] -- <cmd...>     supervise a command; on abnormal exit seal a
//!                                          crash capsule (blob) + index it into ce-debug
//!   ce-crash list [--app A]                crash groups (error events carrying crash_cid)
//!   ce-crash why <app> [--node N]          newest crash for app: fetch capsule, render
//!   Common: --provider <node-id> --cap <token> --json
//!
//! The read side is one ce.debug/ctl call each — the same envelope every suite CLI uses.
//! `run` is the universal v1: crash capture for ANY app in ANY language, zero code changes.

use std::collections::VecDeque;
use std::io::BufRead;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ce_crash::{Capsule, CTL_TOPIC, SERVICE, STDERR_TAIL_LINES};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => run(&args).await,
        Some("list") => list(&args).await,
        Some("why") => why(&args).await,
        _ => {
            eprintln!(
                "usage: ce-crash <run [--app A] -- <cmd...> | list [--app A] | why <app> [--node N]> [--provider ID] [--cap T] [--json]"
            );
            Ok(())
        }
    }
}

// ----- run: the wrapper-runner -----

async fn run(args: &[String]) -> Result<()> {
    let sep = args
        .iter()
        .position(|a| a == "--")
        .context("usage: ce-crash run [--app A] -- <cmd...>")?;
    let cmd = &args[sep + 1..];
    anyhow::ensure!(!cmd.is_empty(), "no command after --");
    let app = flag(&args[..sep], "--app").unwrap_or_else(|| basename(&cmd[0]));

    let start_ts_ms = ce_crash::now_ms();
    let mut child = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {}", cmd[0]))?;

    // Tee stderr: pass every line through to our stderr, keep the last N for the capsule.
    let stderr = child.stderr.take().context("child stderr not captured")?;
    let tail_thread = std::thread::spawn(move || {
        let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            eprintln!("{line}");
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<String>>()
    });

    let status = child.wait()?;
    let stderr_tail = tail_thread.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }

    let exit_code = status.code();
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    let capsule = ce_crash::build_run_capsule(
        &app,
        cmd,
        exit_code,
        signal,
        stderr_tail,
        start_ts_ms,
        ce_crash::now_ms(),
    );
    match emit(&capsule, flag(args, "--cap")).await {
        Ok(cid) => eprintln!("ce-crash: capsule {cid} indexed for {app}"),
        Err(e) => match write_fallback(&capsule) {
            Ok(path) => eprintln!("ce-crash: mesh unreachable ({e}); capsule written to {path}"),
            Err(e2) => eprintln!("ce-crash: capsule lost ({e}; fallback failed: {e2})"),
        },
    }
    // Propagate the child's fate to our caller.
    std::process::exit(exit_code.unwrap_or_else(|| 128 + signal.unwrap_or(0)));
}

/// Seal the capsule: blob-upload it, then ingest the index event into ce-debug.
async fn emit(capsule: &Capsule, cap: Option<String>) -> Result<String> {
    let ce = ce_rs::CeClient::local();
    let cid = ce.put_blob(serde_json::to_vec_pretty(capsule)?).await?;
    let provider = resolve_provider(&ce, None).await?;
    let mut body = json!({"op": "ingest", "args": {"events": [ce_crash::index_event(capsule, &cid)]}});
    if let Some(cap) = cap.or_else(|| std::env::var("CE_DEBUG_CAP").ok()) {
        body["cap"] = json!(cap);
    }
    let raw = ce.request(&provider, CTL_TOPIC, &serde_json::to_vec(&body)?, 30_000).await?;
    let v: Value = serde_json::from_slice(&raw)?;
    if let Some(err) = v["error"].as_str() {
        anyhow::bail!("{err}");
    }
    Ok(cid)
}

fn write_fallback(capsule: &Capsule) -> Result<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = std::path::PathBuf::from(home).join(".local/share/ce/ce-crash");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-{}.json", capsule.app, capsule.ts_ms));
    std::fs::write(&path, serde_json::to_vec_pretty(capsule)?)?;
    Ok(path.display().to_string())
}

// ----- read side -----

async fn query_crashes(args: &[String], app: Option<String>, limit: usize) -> Result<Vec<Value>> {
    let ce = ce_rs::CeClient::local();
    let provider = resolve_provider(&ce, flag(args, "--provider")).await?;
    let mut body = json!({"op": "query", "args": {"app": app, "level": "error", "limit": limit}});
    if let Some(cap) = flag(args, "--cap").or_else(|| std::env::var("CE_DEBUG_CAP").ok()) {
        body["cap"] = json!(cap);
    }
    let raw = ce.request(&provider, CTL_TOPIC, &serde_json::to_vec(&body)?, 60_000).await?;
    let v: Value = serde_json::from_slice(&raw)?;
    if let Some(err) = v["error"].as_str() {
        anyhow::bail!("{err}");
    }
    Ok(v["result"]["events"].as_array().cloned().unwrap_or_default())
}

async fn list(args: &[String]) -> Result<()> {
    let events = query_crashes(args, flag(args, "--app"), 1000).await?;
    let groups = ce_crash::group_crashes(&events);
    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&groups)?);
        return Ok(());
    }
    if groups.is_empty() {
        println!("no crashes recorded");
        return Ok(());
    }
    for g in &groups {
        println!(
            "{:>4}x  last {}  {:<16} {}  capsule {}",
            g.count,
            ce_crash::fmt_time(g.last_ts_ms),
            g.app,
            g.sample_msg.lines().next().unwrap_or(""),
            &g.last_cid[..12.min(g.last_cid.len())],
        );
    }
    Ok(())
}

async fn why(args: &[String]) -> Result<()> {
    let app = positional(args, 2).context("usage: ce-crash why <app> [--node N]")?;
    let node_filter = flag(args, "--node");
    let events = query_crashes(args, Some(app.clone()), 500).await?;
    // Newest first from the store; pick the first crash event (matching --node if given).
    let event = events
        .iter()
        .find(|e| {
            e["fields"]["crash_cid"].as_str().is_some()
                && node_filter
                    .as_deref()
                    .map_or(true, |n| e["node"].as_str().unwrap_or_default().starts_with(n))
        })
        .with_context(|| format!("no crash recorded for '{app}'"))?;
    let cid = event["fields"]["crash_cid"].as_str().unwrap_or_default().to_string();
    let node = event["node"].as_str().unwrap_or_default().to_string();

    let ce = ce_rs::CeClient::local();
    let capsule: Option<Capsule> = match ce.get_blob(&cid).await {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    };
    if args.iter().any(|a| a == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"event": event, "cid": cid, "capsule": capsule}))?
        );
        return Ok(());
    }
    match capsule {
        Some(c) => print!("{}", ce_crash::render_capsule(&c, &cid, &node)),
        None => {
            // Blob unreachable: still answer from the index event alone.
            println!(
                "{}  {}  (capsule blob {} not fetchable — index event only)",
                app,
                ce_crash::fmt_datetime(event["ts_ms"].as_u64().unwrap_or(0)),
                cid
            );
            println!("what:  {}", event["msg"].as_str().unwrap_or(""));
            println!("node:  {node}");
            if let Some(stack) = event["stack"].as_str() {
                println!("stack:");
                for l in stack.lines() {
                    println!("  {l}");
                }
            }
        }
    }
    Ok(())
}

// ----- shared helpers -----

async fn resolve_provider(ce: &ce_rs::CeClient, explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(p) = std::env::var("CE_DEBUG_PROVIDER").ok().filter(|s| !s.is_empty()) {
        return Ok(p);
    }
    if let Ok(ids) = ce.find_service(SERVICE).await {
        if let Some(first) = ids.into_iter().next() {
            return Ok(first);
        }
    }
    Ok(ce.status().await.context("no ce.debug provider found and local node down")?.node_id)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// The nth non-flag argument (flags and their values skipped).
fn positional(args: &[String], n: usize) -> Option<String> {
    let mut i = 2; // skip binary name + subcommand
    let mut seen = 2;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += if args[i] == "--json" { 1 } else { 2 };
            continue;
        }
        if seen == n {
            return Some(args[i].clone());
        }
        seen += 1;
        i += 1;
    }
    None
}

fn basename(cmd: &str) -> String {
    std::path::Path::new(cmd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string())
}

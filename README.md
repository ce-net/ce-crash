# ce-crash — post-mortem capture for every app on the mesh

STATUS: WORKING (v1: wrapper-runner + Python last-words hooks + read CLI).

## What / why

An app dies at 03:12 on a board in another building. `ce-crash why board-sensor` — from any
machine — answers with the evidence: exit status, the last stderr lines or stack trace, its
environment manifest, when, on which node. No ssh, no scrolling journalctl on the wrong machine.

## v1 scope (honest)

The original design captured crashes at the **node supervisor seam** (the supervisor observes
every app exit). That seam is not available to an app: the substrate is locked, and a supervisor
crash hook would be a substrate change. Supervisor-seam capture is therefore **v2 — it requires
its own substrate discussion first**. v1 ships what needs no substrate change and is already
universal:

1. **`ce-crash run -- <cmd...>`** — a wrapper-runner that supervises any command. On nonzero
   exit or death-by-signal it seals a crash capsule. This gives crash capture for ANY app in
   ANY language with zero code changes: wrap the launch, done.
2. **SDK last-words hooks** — `clients/py/ce_crash.py` (stdlib-only). `ce_crash.install(app)`
   arms `sys.excepthook` + an atexit hook; an unhandled exception or `sys.exit(nonzero)` seals
   a capsule with the exception type, message, and full stack. It wraps the same mesh-call
   subset the ce-debug Python client uses (vendored, ~60 lines — noted in the file) rather
   than modifying ce-debug.
3. **The read CLI** — `ce-crash list` / `ce-crash why <app>`.

## The crash capsule

One JSON document, sealed at death:

```json
{
  "app": "crash-demo", "node": "", "ts_ms": 1784900000000,
  "exit_kind": "exit | signal | exception",
  "exit_code": 1, "signal": null, "exc_type": "RuntimeError",
  "msg": "RuntimeError: demo crash: sensor voltage out of range",
  "stack": "Traceback ...",            "stderr_tail": ["last 100 lines ..."],
  "argv": ["python3", "-c", "..."],    "python": "3.12 ...",
  "platform": "macos-aarch64",         "cwd": "/where/it/ran",
  "env_keys": ["HOME", "PATH", "..."],
  "recent": [ "last 50 ce_debug events this process buffered, if a client was passed" ]
}
```

The capsule is uploaded to the local node's content-addressed blob store (`POST /blobs` ->
CID). The INDEX is a `level=error` event ingested into the ce-debug service
([ce-net/ce-debug](https://github.com/ce-net/ce-debug), topic `ce.debug/ctl`) carrying
`fields.crash_cid` — so crashes appear in the normal error stream (`ce-debug errors` sees
them too) AND their full evidence is one blob fetch away. Retention scales the ce-debug way:
capsules are blobs anywhere on the mesh.

**Fail-open:** if the node/mesh is down, the capsule is written to
`~/.local/share/ce/ce-crash/<app>-<ts>.json` and a note is printed on stderr. Crash capture
never takes the dying app down harder.

**Privacy:** the capsule records environment variable NAMES only, never values. Sharp edge,
documented: `argv` and stderr tails are captured verbatim — do not pass secrets on command
lines you wrap.

## Usage

```bash
# Universal capture — any language, zero code changes:
ce-crash run --app board-sensor -- python3 sensor.py
ce-crash run -- ./my-service --port 9000        # app name = argv[0] basename

# Python last-words hooks (richer: exception type + stack + recent event tail):
import ce_crash
ce_crash.install("board-sensor")                 # optionally: debug=<ce_debug client>

# Forensics, from any machine that can reach a ce.debug provider:
ce-crash list                                    # crash groups, newest first
ce-crash list --app board-sensor
ce-crash why board-sensor                        # newest capsule: what/when/where/stack
ce-crash why board-sensor --node 2d7fc92f        # filter by node id prefix
# Common flags: --provider <node-id>  --cap <token>  --json
```

Auth follows ce-debug: the collector's `--auth` mode governs; pass `--cap` (or
`CE_DEBUG_CAP`) when the collector requires a capability granting `debug:write` (sealing)
or `debug:read` (list/why).

## Build / test

```bash
cargo build --release          # the ce-crash binary
cargo test                     # capsule build, grouping, render (fixtures)
python3 clients/py/test_ce_crash.py   # hook + fallback-shape tests (no node needed)
```

## Roadmap

1. v2: supervisor-seam capture (node observes every app exit) — needs a substrate
   discussion; not buildable app-side today.
2. Restart counts + app version/CID in the capsule (node-facts / appmgr metadata).
3. MCP skin (`crash_why`, `crash_list` — the agent's first move on "it died"); UI panel.
4. Last-words hooks for the other SDK languages (TS/Go), same capsule shape.

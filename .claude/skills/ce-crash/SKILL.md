---
name: ce-crash
description: Crash forensics - wrap any command or arm Python hooks so crashes seal evidence capsules (blob CIDs indexed as ce-debug errors); read them with list/why. Read before adding crash capture or investigating a death.
---
# ce-crash usage
Universal: `ce-crash run [--app A] -- <cmd...>` supervises any command; nonzero exit/signal seals a
capsule {exit, stderr tail(100), argv, platform, env NAMES only} -> blob CID -> level=error event
with fields.crash_cid into ce.debug. Python-rich: `import ce_crash; ce_crash.install(app)` (hooks
sys.excepthook/atexit; full stack + recent event tail). Fail-open: node down -> capsule lands in
~/.local/share/ce/ce-crash/. Read: `ce-crash list [--app]`, `ce-crash why <app> [--node prefix]
[--json]` (fetches the capsule by CID). Sharp edge: argv/stderr are captured verbatim - never pass
secrets on wrapped command lines. Supervisor-seam capture is v2 (substrate discussion required).

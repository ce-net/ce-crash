# ce-crash — post-mortem capture for every app on the mesh

STATUS: DESIGN (repo seeded 2026-07-24; no code yet).

## What / why

An app dies at 03:12 on a board in another building. `ce-crash why board-sensor` — from any
machine — answers with the evidence: exit status, the last seconds of its logs, its environment
manifest, when, on which node, which build. No ssh, no scrolling journalctl on the wrong machine.

## Design

**Capture at the supervisor seam.** The node supervisor already observes every app exit. A crash
reporter (host-composed adapter, not an app patch) triggers on abnormal exit and assembles a
CRASH CAPSULE:

- exit status / signal, timestamps (start, exit), restart count
- the app's tail from ce-debug (last N ring events for that app@node — the suite composes)
- stderr/stdout tail from the supervisor's log
- environment manifest: app version/CID, manifest hash, node id, os/arch (node-facts), ce version
- optional app-provided last-words file (`<data_dir>/apps/<app>/crash.json`, written by SDK
  panic/exception hooks — the ce-debug clients already capture the stack; this persists it)

The capsule is one JSON blob -> content-addressed mesh blob (CID); the capsule INDEX (app, node,
ts, CID) is reported into ce-debug as a `level=error` event with `fields.crash_cid` — so crashes
appear in the normal error stream AND carry their full evidence by reference. Retention scales the
ce-debug way: capsules are blobs anywhere on the mesh.

**Read side.** `ce-crash why <app> [--node N]` = query ce-debug for the newest crash event, fetch
the capsule by CID, render. `ce-crash list` groups by fingerprint (same grouping rules). Ability:
`debug:read` covers the index; capsule blobs inherit blob access (documented sharp edge:
capsules contain env + log tails — cecapabilities.toml spells out what never to include: secrets,
identity keys, payload bodies).

## Skins

CLI (`why`, `list`); MCP (`crash_why`, `crash_list` — the agent's first move on "it died");
`<ce-crash-panel>` UI component later; SDK last-words hooks land in the ce-debug clients.

## Plan of record

1. Supervisor-seam reporter + capsule format + ce-debug indexing.
2. `why`/`list` CLI + MCP.
3. Last-words hooks in ce-debug SDKs; UI panel.

Reference: ce-debug (store/wire/grouping), ce-recover (stuck-node playbook), node-facts (env).

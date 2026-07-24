"""ce_crash -- last-words crash capsules for Python apps on the mesh (stdlib only).

    import ce_crash
    ce_crash.install("my-app")

    # optionally hand it your ce_debug client so the capsule carries the process's
    # recent event tail:
    #   dbg = ce_debug.connect("my-app")
    #   ce_crash.install("my-app", debug=dbg)

On an unhandled exception, or an abnormal exit via sys.exit(nonzero), the process's last
words become a CRASH CAPSULE: one JSON document containing what died, why, and the
environment manifest. The capsule is uploaded to the local node's content-addressed blob
store (POST /blobs -> CID) and indexed into the ce.debug service as a level="error" event
carrying fields.crash_cid -- so the crash appears in the normal error stream AND its full
evidence is one blob fetch away. Read it back from any machine:

    ce-crash list
    ce-crash why my-app

Fail-open: if the node/mesh is unreachable the capsule is written to
~/.local/share/ce/ce-crash/<app>-<ts>.json instead, and a note is printed on stderr.
Nothing here ever raises into the dying app.

Privacy: the capsule records environment variable NAMES only (env_keys), never values.

The mesh-call subset below (~60 lines: token discovery, POST /blobs, provider resolution,
the {"op","args"} envelope over POST /mesh/request on ce.debug/ctl) is VENDORED from
ce-debug clients/py/ce_debug.py so this module stays one stdlib-only file with zero
imports beyond the standard library.
"""

import atexit
import json
import os
import pathlib
import platform
import sys
import time
import traceback
import urllib.parse
import urllib.request

SERVICE = "ce.debug"
CTL_TOPIC = "ce.debug/ctl"
RECENT_CAP = 50

_state = {
    "installed": False,
    "app": None,
    "debug": None,          # optional ce_debug.DebugClient; its buffer -> capsule "recent"
    "emitted": False,
    "exit_code": None,      # recorded by the sys.exit wrapper
    "prev_excepthook": None,
    "prev_exit": None,
}


# -- vendored mesh-call subset (from ce-debug clients/py/ce_debug.py) --

def _api_token():
    tok = os.environ.get("CE_API_TOKEN")
    if tok:
        return tok
    for p in (
        pathlib.Path.home() / "Library/Application Support/ce/api.token",
        pathlib.Path.home() / ".local/share/ce/api.token",
    ):
        try:
            return p.read_text().strip()
        except OSError:
            pass
    return None


def _node_url():
    return os.environ.get("CE_NODE_URL", "http://127.0.0.1:8844").rstrip("/")


def _http(method, path, body=None, raw=None):
    req = urllib.request.Request(_node_url() + path, method=method)
    tok = _api_token()
    if tok:
        req.add_header("Authorization", "Bearer " + tok)
    if raw is not None:
        data = raw
        req.add_header("Content-Type", "application/octet-stream")
    elif body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    else:
        data = None
    with urllib.request.urlopen(req, data=data, timeout=15) as r:
        return json.loads(r.read().decode() or "{}")


def _resolve_provider():
    prov = os.environ.get("CE_DEBUG_PROVIDER")
    if prov:
        return prov
    try:
        found = _http("GET", "/discovery/find/" + urllib.parse.quote(SERVICE, safe=""))
        if found.get("providers"):
            return found["providers"][0]
    except Exception:
        pass
    return _http("GET", "/status")["node_id"]


def _ingest(event):
    body = {"op": "ingest", "args": {"events": [event]}}
    cap = os.environ.get("CE_DEBUG_CAP")
    if cap:
        body["cap"] = cap
    reply = _http("POST", "/mesh/request", body={
        "to": _resolve_provider(), "topic": CTL_TOPIC,
        "payload_hex": json.dumps(body).encode().hex(), "timeout_ms": 30000,
    })
    out = json.loads(bytes.fromhex(reply["payload_hex"]).decode() or "{}")
    if "error" in out:
        raise RuntimeError(out["error"])


# -- capsule --

def _build_capsule(exit_kind, exc_type, msg, stack):
    recent = []
    dbg = _state["debug"]
    if dbg is not None:
        buf = getattr(dbg, "buf", None)
        if isinstance(buf, list):
            recent = list(buf[-RECENT_CAP:])
    return {
        "app": _state["app"],
        "node": "",  # the collector stamps the authenticated sender node
        "ts_ms": int(time.time() * 1000),
        "exit_kind": exit_kind,          # "exception" | "exit"
        "exc_type": exc_type,
        "msg": msg,
        "stack": stack,
        "argv": list(sys.argv),
        "python": sys.version,
        "platform": platform.platform(),
        "cwd": os.getcwd(),
        "env_keys": sorted(os.environ.keys()),  # names ONLY, never values
        "recent": recent,
    }


def _fallback_write(capsule):
    d = pathlib.Path.home() / ".local/share/ce/ce-crash"
    d.mkdir(parents=True, exist_ok=True)
    path = d / "{}-{}.json".format(capsule["app"], capsule["ts_ms"])
    path.write_text(json.dumps(capsule, indent=2))
    return path


def _emit(capsule):
    if _state["emitted"]:
        return
    _state["emitted"] = True
    try:
        cid = _http("POST", "/blobs", raw=json.dumps(capsule).encode())["hash"]
        event = {
            "ts_ms": capsule["ts_ms"], "app": capsule["app"], "node": "",
            "level": "error", "msg": capsule["msg"],
            "fields": {"crash_cid": cid, "exit_kind": capsule["exit_kind"]},
        }
        if capsule.get("exc_type"):
            event["fields"]["exc_type"] = capsule["exc_type"]
        if capsule.get("stack"):
            event["stack"] = capsule["stack"]
        _ingest(event)
        print("ce-crash: capsule {} indexed for {}".format(cid, capsule["app"]),
              file=sys.stderr)
    except Exception as e:  # fail-open, never raise into the dying app
        try:
            path = _fallback_write(capsule)
            print("ce-crash: mesh unreachable ({}); capsule written to {}".format(e, path),
                  file=sys.stderr)
        except Exception as e2:
            print("ce-crash: capsule lost ({}; fallback failed: {})".format(e, e2),
                  file=sys.stderr)


# -- hooks --

def _excepthook(exc_type, exc, tb):
    # Let the previous hook print the traceback first, then emit the capsule.
    prev = _state["prev_excepthook"] or sys.__excepthook__
    try:
        prev(exc_type, exc, tb)
    except Exception:
        pass
    msg = traceback.format_exception_only(exc_type, exc)[-1].strip()
    stack = "".join(traceback.format_exception(exc_type, exc, tb))
    _emit(_build_capsule("exception", exc_type.__name__, msg, stack))


def _wrapped_exit(code=None):
    _state["exit_code"] = code
    _state["prev_exit"](code)


def _atexit_hook():
    code = _state["exit_code"]
    if _state["emitted"] or code in (None, 0):
        return
    msg = "exit code {}".format(code) if isinstance(code, int) else str(code)
    _emit(_build_capsule("exit", None, msg, None))


def install(app, debug=None):
    """Arm the last-words hooks for this process. Idempotent."""
    if _state["installed"]:
        _state["app"] = app
        if debug is not None:
            _state["debug"] = debug
        return
    _state["installed"] = True
    _state["app"] = app
    _state["debug"] = debug
    _state["prev_excepthook"] = sys.excepthook
    sys.excepthook = _excepthook
    _state["prev_exit"] = sys.exit
    sys.exit = _wrapped_exit
    atexit.register(_atexit_hook)

"""The VOWP explainer, scene by scene.

Each scene is a function draw(cv, t) over its own local time t (seconds). The
video plays every scene from 0 to its duration; the README diagrams are a single
frame of a scene at a chosen "still" time, so the pictures and the video always
agree.
"""

from pixkit import (PAL, Canvas, panel, chip, db_cyl, server, laptop, desktop, phone,
                    lock, key_icon, envelope, doc_icon, spinner, arrow, packet,
                    worker_chip, job_node, text_w, ease, clamp01, mix)

W, H = 640, 360


# ---------------------------------------------------------------- chrome
def chrome(cv, idx, title, prog=None):
    cv.rect(0, 0, W, 17, "bg2")
    cv.rect(0, 17, W, 1, "outline")
    # tiny 3-block mark
    cv.rect(8, 5, 3, 7, "main")
    cv.rect(12, 7, 3, 5, "light")
    cv.rect(16, 3, 3, 9, "pale")
    cv.text(24, 5, "VOWP", "light", shadow=None)
    cv.text(56, 5, "a VPS framework for AI agents", "dim2", shadow=None)
    if idx:
        cv.text(W - 8, 5, f"{idx:02d} · {title}", "pale", shadow=None, align="right")
    if prog is not None:
        cv.rect(0, H - 3, W, 3, "outline")
        cv.rect(0, H - 3, int(W * prog), 3, "main")


def caption(cv, lines, t, t0=0.5, cps=48, y=302, col="white", still=False):
    n = 10 ** 6 if still else int(max(0.0, t - t0) * cps)
    for i, line in enumerate(lines):
        if n <= 0:
            break
        x = W // 2 - (len(line) * 12 - 2) // 2
        cv.typed(x, y + i * 22, line, n, col if i == 0 else "pale", scale=2, cursor=not still)
        n -= len(line)


def grid_bg(cv, t=0.0):
    for y in range(26, 296, 12):
        for x in range(6, W, 12):
            cv.px(x, y, "panel2")


def on(t, t0):
    return t >= t0


def flash_frame(cv, x, y, w, h, t, t0, col="white"):
    if t0 <= t < t0 + 0.12:
        cv.frame(x - 1, y - 1, w + 2, h + 2, col)


# ================================================================ 01 title
def s_title(cv, t, still=False):
    grid_bg(cv, t)
    # drifting packets in the background
    for i in range(9):
        y = 40 + (i * 29) % 240
        x = int((t * (18 + i * 4) + i * 83) % (W + 40)) - 20
        cv.rect(x, y, 3, 3, "deep")
    reveal = clamp01(t / 1.0)
    word = "VOWP"
    sc = 8
    tw = len(word) * 6 * sc - sc
    x0 = W // 2 - tw // 2
    # shadow block
    cv.dither(x0 + 6, 82, tw, 56, "outline", 0.5)
    cv.text(x0, 76, word, "light", scale=sc, shadow="deep")
    if reveal < 1:
        cv.rect(x0 + int(tw * reveal), 70, tw, 70, "bg")
    if on(t, 1.0):
        n = int((t - 1.0) * 40)
        cv.typed(W // 2 - (29 * 12) // 2, 156, "A VPS framework for AI agents", n, "white", scale=2)
        cv.typed(W // 2 - (20 * 12) // 2, 178, "to work on projects.", n - 29, "pale", scale=2)
    if on(t, 2.4):
        s = "global DB · project DBs · universal sessions · job graphs · loops · inbox"
        cv.text(W // 2, 218, s, "dim", align="center")
    if on(t, 3.0):
        x = W // 2 - 150
        for lab, tone in (("always-on home", "green"), ("any harness", "blue"),
                          ("runs for days", "violet"), ("MIT · v0.4.0", "amber")):
            x += chip(cv, x, 240, lab, tone) + 6


# ================================================================ 02 why a VPS
def s_why(cv, t, still=False):
    grid_bg(cv)
    lid = 1.0 - ease((t - 1.6) / 0.9)
    panel(cv, 30, 34, 250, 238, title="your laptop", tone="grey", fill="bg2")
    laptop(cv, 105, 70, lid=lid, t=t, w=96)
    if lid < 0.05:
        for i in range(3):
            zt = (t * 0.8 + i * 0.33) % 1.0
            cv.text(214 + int(zt * 14), 82 - int(zt * 30), "z" if i % 2 else "Z", "dim", shadow=None)
    items_l = [("sleeps when the lid closes", 2.0), ("changes networks", 2.5),
               ("holds local state", 3.0), ("agents stop with it", 3.5)]
    for i, (s, t0) in enumerate(items_l):
        if on(t, t0):
            cv.text(56, 140 + i * 16, "✗", "red")
            cv.text(70, 140 + i * 16, s, "pale")
    if on(t, 3.5):
        cv.text(155, 228, "a control surface,", "dim", align="center")
        cv.text(155, 240, "not a home", "dim", align="center")

    panel(cv, 360, 34, 250, 238, title="the VPS", tone="green", fill="panel")
    server(cv, 457, 54, units=4, t=t, w=56)
    up = 41 * 86400 + 6 * 3600 + 12 * 60 + int(t * 1) + 33
    d, r = divmod(up, 86400)
    hh, r = divmod(r, 3600)
    mm, ss = divmod(r, 60)
    cv.text(485, 126, f"uptime {d}d {hh:02d}:{mm:02d}:{ss:02d}", "light", align="center")
    items_r = [("always on", 2.2), ("one address", 2.7), ("one database", 3.2), ("agents keep working", 3.7)]
    for i, (s, t0) in enumerate(items_r):
        if on(t, t0):
            cv.text(386, 146 + i * 16, "✓", "light")
            cv.text(400, 146 + i * 16, s, "pale")
    if on(t, 4.0):
        cv.text(485, 228, "the home for your agents", "light", align="center")
        cv.text(485, 240, "and the single source of truth", "dim", align="center")
    if on(t, 4.3):
        ph = int(t * 20)
        arrow(cv, 286, 150, 352, 150, "amber", dash=3, phase=-ph)
        cv.text(319, 136, "move", "amber", align="center", shadow=None)
        cv.text(319, 158, "home", "amber", align="center", shadow=None)
    caption(cv, ["A laptop sleeps. A VPS stays on.",
                 "VOWP makes the always-on machine the home."], t, t0=0.6, still=still)


# ================================================================ 03 the home
def s_home(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 24)
    # control surfaces
    cv.text(70, 26, "control surfaces", "dim", align="center")
    laptop(cv, 42, 40, t=t, w=56, label="Mac")
    desktop(cv, 42, 108, t=t, w=56, label="desktop")
    phone(cv, 59, 184, t=t, label="phone")
    for i, y in enumerate((62, 128, 204)):
        arrow(cv, 104, y, 196, y, "shadow", dash=2, phase=-ph)
        for k in range(2):
            packet(cv, 104, y, 194, y, ((t * 0.7 + i * 0.3 + k * 0.5) % 1.0), "light")
    cv.text(150, 90, "ssh · key only", "dim", align="center", shadow=None)
    lock(cv, 128, 102)
    cv.text(142, 104, "no open port", "amber", shadow=None)

    hx, hy, hw, hh = 202, 30, 428, 258
    panel(cv, hx, hy, hw, hh, title="HOME · one Linux VPS · ~/.vowp", tone="green", fill="panel")

    # supervisor
    if on(t, 0.3):
        panel(cv, 214, 44, 214, 50, title="supervisor", tone="green", fill="panel2")
        spinner(cv, 232, 70, t * 1.5, "light", 7)
        cv.text(248, 56, "L1 tick every 60 s", "white")
        cv.text(248, 68, f"tick {14208 + int(t):,}", "light")
        cv.text(248, 80, "L1–L9 loops · watchdog", "dim")
    server(cv, 450, 42, units=3, t=t, w=48)
    if on(t, 0.6):
        cv.text(512, 48, "always on", "light")
        cv.text(512, 60, "only writer", "pale")
        cv.text(512, 72, "reconciles from", "dim")
        cv.text(512, 82, "DB + logs + git", "dim")

    # databases
    if on(t, 1.2):
        db_cyl(cv, 216, 112, 32, 36)
        cv.text(256, 112, "global DB  vowp.db", "white")
        cv.text(256, 124, "projects · sessions index", "dim")
        cv.text(256, 134, "machines · leases · capacity", "dim")
        cv.text(256, 144, "machine inbox · secret names", "dim")
    if on(t, 1.8):
        cv.text(440, 106, "project DBs", "white")
        for i, nm in enumerate(("app", "api", "docs")):
            db_cyl(cv, 440 + i * 40, 118, 26, 28, label=nm)
        cv.text(440, 160, "jobs · runs · gates · inbox", "dim")

    # sessions + snapshot
    if on(t, 2.6):
        cv.text(216, 176, "sessions/<id>/log.jsonl", "white")
        for i in range(5):
            doc_icon(cv, 216 + i * 18, 190, "pale" if i < 4 else "light", lines=3 + (i + int(t * 2)) % 2)
        cv.text(310, 194, "append-only", "dim")
        cv.text(310, 204, "any harness", "dim")
        cv.text(440, 176, "snapshot.json", "white")
        doc_icon(cv, 440, 190, "light")
        cv.text(458, 194, "rewritten", "dim")
        cv.text(458, 204, "every tick", "dim")
        if int(t * 2) % 2 == 0 and not still:
            cv.frame(439, 189, 14, 17, "white")

    # workers
    if on(t, 3.4):
        cv.text(216, 222, "workers · stock harnesses in tmux + git worktrees", "white")
        x = 216
        for nm, tone in (("claude code", "blue"), ("opencode", "blue"), ("codex", "blue"), ("verifier", "violet")):
            worker_chip(cv, x, 236, nm, tone, t + x * 0.01)
            x += text_w(nm) + 26
        lock(cv, 216, 262, "amber")
        cv.text(230, 264, "no public port · loopback + SSH tunnels", "amber")
    caption(cv, ["The home owns all of the truth.",
                 "Your devices only steer it."], t, t0=0.8, still=still)


# ================================================================ 04 sessions
_EV_COL = {"U": "pale", "A": "light", "T": "blue", "C": "amber", "S": "violet", "L": "red"}


def _session_events():
    evs = []
    pat = "UATTAATUATATTA"
    tt, i = -8.0, 0
    while tt < 12:
        evs.append((tt, pat[i % len(pat)]))
        tt += 0.34
        i += 1
    for at, kinds in ((2.5, "CS"), (4.6, "L"), (7.0, "CS")):
        for k, kind in enumerate(kinds):
            evs.append((at + k * 0.05, kind))
    evs.sort()
    return evs


_EVS = _session_events()


def s_sessions(cv, t, still=False):
    grid_bg(cv)
    harness = 0 if t < 2.5 else (1 if t < 7.0 else 2)
    machine = 0 if t < 7.0 else 1
    surface = 0 if t < 4.6 else 3

    cv.text(20, 32, "harness", "dim")
    xs = []
    x = 80
    for i, nm in enumerate(("Claude Code", "opencode", "Codex CLI", "generic CLI")):
        xs.append(x)
        x += chip(cv, x, 29, nm, "pale" if i == harness else "grey") + 8
    cv.text(20, 52, "machine", "dim")
    mx = []
    x = 80
    for i, nm in enumerate(("VPS home", "Mac · agent node")):
        mx.append(x)
        x += chip(cv, x, 49, nm, "pale" if i == machine else "grey") + 8
    cv.text(380, 52, "model: any rung of a profile ladder", "dim")

    # the tape
    panel(cv, 16, 80, 608, 62, title="session s-7f3a · sessions/s-7f3a/log.jsonl · append-only",
          tone="green", fill="bg2")
    evs = [e for e in _EVS if e[0] <= t]
    cap = 37
    start = max(0, len(evs) - cap)
    for k, (et, kind) in enumerate(evs[start:]):
        bx = 24 + k * 16
        col = _EV_COL[kind]
        fresh = (t - et) < 0.15 and not still
        cv.rect(bx, 96, 14, 18, "white" if fresh else col)
        cv.rect(bx, 113, 14, 1, "black")
        cv.text(bx + 4, 101, kind, "black", shadow=None)
    cv.rect(24 + min(len(evs), cap) * 16, 96, 2, 18, "light" if int(t * 3) % 2 or still else "bg2")
    # tape legend
    x = 24
    for kind, lab in (("U", "user"), ("A", "assistant"), ("T", "tool"), ("C", "capsule"),
                      ("S", "switch"), ("L", "lease takeover")):
        cv.rect(x, 124, 7, 7, _EV_COL[kind])
        cv.text(x + 10, 124, lab, "dim", shadow=None)
        x += text_w(lab) + 24

    # harness pointer

    # events for switches
    for at, msg in ((2.5, "capsule: Claude Code → opencode"), (4.6, "takeover: lease moves to phone"),
                    (7.0, "capsule: VPS → Mac agent node, Codex CLI")):
        if at <= t < at + 2.0 and not still:
            cv.text(320, 150, msg, "amber", align="center")

    # surfaces
    cv.text(20, 172, "surfaces", "dim")
    names = ("opencode desktop", "Claude app", "vowp chat", "phone · SSH", "MCP client")
    x = 20
    for i, nm in enumerate(names):
        w = text_w(nm) + 14
        act = i == surface
        panel(cv, x, 186, w, 40, tone="green" if act else "grey", fill="panel" if act else "bg2")
        cv.text(x + 7, 192, nm, "white" if act else "dim", shadow=None)
        if act:
            key_icon(cv, x + 7, 208, "amber")
            cv.text(x + 24, 208, "writes", "amber", shadow=None)
        else:
            cv.text(x + 7, 208, "follows", "dim2", shadow=None)
            cv.text(x + 7, 216, "read-only", "dim2", shadow=None)
        if act:
            arrow(cv, x + w // 2, 184, x + w // 2, 146, "amber", dash=2, phase=int(t * 20))
        x += w + 8
    cv.text(20, 240, "Exactly one surface holds the write lease. Takeover moves it; the old one follows.",
            "pale")
    cv.text(20, 254, "Export / import as a bundle or an ASIF package. The log never forks.", "dim")
    caption(cv, ["One session. Any harness, model or surface.",
                 "Switch with a capsule; the history stays."], t, t0=0.6, still=still)


# ================================================================ 05 orchestrate
def s_orchestrate(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 24)
    panel(cv, 16, 30, 218, 222, title="orchestrate on the VPS", tone="green")
    server(cv, 28, 46, units=3, t=t, w=48)
    lines_l = ["orchestrator runs", "on the home: an", "opencode server on", "127.0.0.1, or MCP.",
               "", "attach from Mac, desktop", "or phone through an SSH", "tunnel. Close the lid:", "it keeps going."]
    for i, s in enumerate(lines_l):
        cv.text(84 if i < 4 else 28, 50 + i * 12 if i < 4 else 50 + i * 12 + 22, s,
                "pale" if i < 4 else "dim", shadow=None)
    laptop(cv, 36, 206, t=t, w=40)
    phone(cv, 96, 204, t=t)
    arrow(cv, 80, 222, 150, 222, "shadow", dash=2, phase=-ph)
    cv.text(156, 218, "tunnel", "dim", shadow=None)

    panel(cv, 406, 30, 218, 222, title="orchestrate on your machine", tone="blue")
    laptop(cv, 418, 46, t=t, w=56)
    lines_r = ["agent node: Mac", "or Linux box that", "dials OUT to home."]
    for i, s in enumerate(lines_r):
        cv.text(482, 52 + i * 12, s, "pale", shadow=None)
    lines_r2 = ["• local chat or opencode surface", "• works where the files are",
                "• opt-in offloaded jobs return", "  committed work (push/bundle)",
                "• own harness login + keys", "• home-only secrets stay home"]
    for i, s in enumerate(lines_r2):
        if on(t, 1.0 + i * 0.3) or still:
            cv.text(418, 110 + i * 14, s, "dim" if i % 2 else "pale", shadow=None)

    # the home in the middle
    panel(cv, 256, 70, 128, 140, title="HOME", tone="amber", fill="panel2")
    db_cyl(cv, 270, 92, 30, 34)
    db_cyl(cv, 306, 98, 22, 26)
    db_cyl(cv, 334, 98, 22, 26)
    cv.text(320, 140, "global + project", "white", align="center")
    cv.text(320, 150, "DBs, session logs", "white", align="center")
    cv.text(320, 166, "the ONLY writer", "amber", align="center")
    cv.text(320, 178, "every merge", "dim", align="center")
    cv.text(320, 188, "happens here", "dim", align="center")
    for k in range(2):
        arrow(cv, 236, 140, 252, 140, "light")
        packet(cv, 236, 140, 252, 140, (t * 0.9 + k * 0.5) % 1, "light")
        arrow(cv, 404, 140, 388, 140, "blue", dash=2, phase=ph)
        packet(cv, 404, 140, 388, 140, (t * 0.9 + k * 0.5) % 1, "blue")
    cv.text(320, 262, "same sessions · same DB · same inbox · pick per task, switch any time", "pale", align="center")
    caption(cv, ["Orchestrate on the VPS or on your own machine.",
                 "The home stays the single source of truth."], t, t0=0.6, still=still)


# ================================================================ 06 databases
_COLS = ("pending", "ready", "running", "review", "done")
_COL_TONE = {"pending": "grey", "ready": "amber", "running": "blue", "review": "violet", "done": "green"}
_JOBS = [
    ("spec", [(0, "done")]),
    ("auth-api", [(0, "done")]),
    ("billing", [(0, "ready"), (1.0, "running"), (4.0, "review"), (5.5, "done")]),
    ("login-ui", [(0, "running"), (5.0, "review"), (8.0, "done")]),
    ("guide", [(0, "ready"), (2.0, "running"), (6.5, "review"), (9.0, "done")]),
    ("i18n", [(0, "pending"), (3.0, "ready"), (9.6, "running")]),
    ("migrate", [(0, "pending"), (5.6, "ready"), (7.0, "running")]),
    ("e2e-auth", [(0, "pending"), (8.2, "ready")]),
    ("perf", [(0, "pending")]),
]


def _job_col(tl, t):
    cur, since = tl[0][1], tl[0][0]
    for at, col in tl:
        if at <= t:
            cur, since = col, at
    return cur, since


def _slots(t):
    cols = {c_: [] for c_ in _COLS}
    for name, tl in _JOBS:
        col, since = _job_col(tl, t)
        cols[col].append((since, name))
    out = {}
    for col, lst in cols.items():
        for r, (_, name) in enumerate(sorted(lst)):
            out[name] = (col, r)
    return out


def _slot_xy(col, r):
    return 240 + _COLS.index(col) * 77, 62 + r * 15


def s_databases(cv, t, still=False):
    grid_bg(cv)
    panel(cv, 16, 30, 206, 256, title="global · ~/.vowp/vowp.db", tone="green")
    db_cyl(cv, 28, 46, 38, 42)
    cv.text(76, 50, "one per home", "white", shadow=None)
    cv.text(76, 62, "the fleet's", "dim", shadow=None)
    cv.text(76, 72, "memory", "dim", shadow=None)
    rows = [("projects registry", "light"), ("sessions index", "light"), ("surfaces + leases", "light"),
            ("machines + workers", "blue"), ("capacity + limits", "blue"), ("provider failover", "blue"),
            ("machine inbox", "amber"), ("secret names only", "amber"), ("ledger", "dim")]
    for i, (s, col) in enumerate(rows):
        y = 100 + i * 19
        if on(t, 0.2 + i * 0.15) or still:
            cv.rect(28, y, 182, 15, "panel2")
            cv.rect(32, y + 4, 7, 7, col)
            cv.text(46, y + 4, s, "pale", shadow=None)

    snap = _slots(t)
    panel(cv, 232, 30, 392, 158, title="projects/app/project.db", tone="green")
    counts = {c_: 0 for c_ in _COLS}
    for name, (col, r) in snap.items():
        counts[col] += 1
    for i, col in enumerate(_COLS):
        x = 240 + i * 77
        tone = _COL_TONE[col]
        chip(cv, x, 42, f"{col} {counts[col]}", tone, w=72)
    for name, tl in _JOBS:
        col, since = _job_col(tl, t)
        tx, ty = _slot_xy(*snap[name])
        dt = t - since
        if 0 < since and dt < 0.45 and not still:
            prev = _slots(since - 0.01)[name]
            px_, py_ = _slot_xy(*prev)
            k = ease(dt / 0.45)
            tx, ty = int(px_ + (tx - px_) * k), int(py_ + (ty - py_) * k)
        tone = _COL_TONE[col]
        fg = {"grey": "dim", "amber": "amber", "blue": "blue", "violet": "violet", "green": "light"}[tone]
        cv.rect(tx, ty, 72, 13, "panel2" if col != "pending" else "bg2")
        cv.rect(tx, ty, 2, 13, fg)
        cv.rect(tx, ty + 12, 72, 1, "black")
        cv.text(tx + 5, ty + 3, name, "white" if col != "pending" else "dim", shadow=None)
        if col == "running":
            cv.rect(tx + 64, ty + 4, 4, 4, "blue" if int(t * 4) % 2 or still else "blue_d")
        if col == "done":
            cv.text(tx + 63, ty + 3, "✓", "light", shadow=None)
    # tables row
    cv.rect(240, 152, 376, 1, "line")
    x = 240
    for nm in ("jobs", "edges", "runs→sessions", "gate_results", "loops", "schedules", "inbox"):
        w = text_w(nm) + 6
        cv.text(x + 3, 158, nm, "amber" if nm == "inbox" else "pale", shadow=None)
        x += w + 4
    cv.text(240, 172, "queue order: priority, then age · owns-locks · measured capacity", "dim", shadow=None)

    for i, (nm, nums) in enumerate((("projects/api", (3, 2, 1, 14, 0)), ("projects/docs", (1, 0, 0, 22, 1)))):
        x = 232 + i * 200
        panel(cv, x, 200, 192, 60, title=nm, tone="green")
        lab = ("pend", "ready", "run", "done", "inbox")
        for k, (l, n) in enumerate(zip(lab, nums)):
            cx = x + 8 + k * 36
            cv.text(cx, 214, f"{n}", "amber" if l == "inbox" and n else "white", scale=2, shadow=None)
            cv.text(cx, 232, l, "dim", shadow=None)
        cv.text(x + 8, 246, "paused · resume any time" if i else "ticking in fairness order", "dim2",
                shadow=None)
    cv.text(428, 272, "a broken project parks alone; the rest keep going", "dim", align="center", shadow=None)
    caption(cv, ["A global DB for the fleet, one DB per project:",
                 "queue, runs, gates, sessions, schedules, inbox."], t, t0=0.6, still=still)


# ================================================================ 07 job graph
_G_NODES = {
    "goal":   ("plan", "auth v2", 16, 122, [(0, "running", "planner"), (1.2, "done", None)]),
    "spec":   ("spec", "api-spec", 140, 122, [(0, "pending", None), (1.2, "ready", None), (1.5, "running", "claude"),
                                              (3.0, "reviewing", "verifier"), (3.6, "done", None)]),
    "api":    ("build", "auth-api", 264, 46, [(0, "pending", None), (3.6, "ready", None), (3.8, "running", "claude"),
                                               (6.4, "gating", "cmd gates"), (7.0, "reviewing", "verifier"),
                                               (7.8, "done", None)]),
    "ui":     ("build", "login-ui", 264, 122, [(0, "pending", None), (3.6, "ready", None), (3.9, "running", "opencode"),
                                                (6.0, "reviewing", "verifier"), (6.8, "done", None)]),
    "guide":  ("docs", "auth-guide", 264, 198, [(0, "pending", None), (3.6, "ready", None), (4.1, "running", "codex"),
                                                 (5.4, "reviewing", "verifier"), (6.2, "done", None)]),
    "e2e":    ("test", "e2e-auth", 388, 122, [(0, "pending", None), (7.8, "ready", None), (8.0, "running", "claude"),
                                               (9.3, "done", None)]),
    "merge":  ("merge", "→ main", 512, 122, [(0, "pending", None), (9.3, "merging", "merge queue"),
                                                   (10.4, "done", None)]),
}
_G_EDGES = [("goal", "spec"), ("spec", "api"), ("spec", "ui"), ("spec", "guide"),
            ("api", "e2e"), ("ui", "e2e"), ("e2e", "merge"), ("guide", "merge")]


def _node_state(tl, t):
    st, wk = tl[0][1], tl[0][2]
    for at, s, w in tl:
        if at <= t:
            st, wk = s, w
    return st, wk


def s_graph(cv, t, still=False):
    grid_bg(cv)
    cv.text(16, 28, "goal → job graph (a DAG). Each job node has a brief, owned paths, needs and gates.", "pale")
    ph = int(t * 20)
    nodes = {k: _node_state(v[4], t) for k, v in _G_NODES.items()}
    for a, b in _G_EDGES:
        ka, kb = _G_NODES[a], _G_NODES[b]
        x0, y0 = ka[2] + 112, ka[3] + 13
        x1, y1 = kb[2] - 2, kb[3] + 13
        done = nodes[a][0] == "done"
        col = "shadow" if done else "line"
        mx_ = x0 + (x1 - x0) // 2 if (a, b) != ("guide", "merge") else x1 - 10
        if y0 == y1:
            arrow(cv, x0 - 12, y0, x1, y1, col, dash=0 if done else 2, phase=-ph)
        else:
            cv.hline(x0 - 12, mx_, y0, col, 0 if done else 2, -ph)
            cv.vline(mx_, y0, y1, col, 0 if done else 2, -ph)
            arrow(cv, mx_, y1, x1, y1, col, dash=0 if done else 2, phase=-ph)
        if done and nodes[b][0] in ("ready", "running") and not still:
            packet(cv, mx_, y1, x1, y1, (t * 1.5) % 1, "light")
    for k, (kind, name, x, y, tl) in _G_NODES.items():
        st, wk = nodes[k]
        job_node(cv, x, y, kind, name, st, w=100, t=t, worker=wk)
    # legend
    cv.text(16, 244, "states", "dim")
    x = 60
    for st, tone in (("pending", "grey"), ("ready", "amber"), ("running", "blue"), ("gating", "violet"),
                     ("reviewing", "violet"), ("fixing", "red"), ("merging", "green"), ("done", "green")):
        x += chip(cv, x, 241, st, tone) + 4
    cv.text(16, 262, "edges: needs · verifies · fixes · escalates · blocks.  "
            "One worktree + branch job/<id> per job.", "dim")
    cv.text(16, 274, "No two running jobs own the same paths. Only the merge queue writes main.", "dim")
    caption(cv, ["A goal becomes a job graph.",
                 "Workers take ready nodes in their own worktree."], t, t0=0.6, still=still)


# ================================================================ 08 verify / fix loop
_LOOP = [  # (time, event)
    (0.0, "build"), (1.4, "gate1"), (2.2, "verify1"), (3.2, "fail1"),
    (3.6, "fix1"), (5.0, "gate2"), (5.6, "verify2"), (6.4, "fail2"),
    (6.8, "fix2"), (8.2, "gate3"), (8.8, "verify3"), (9.6, "fail3"),
    (10.0, "escalate"), (10.6, "inbox"),
]


def _loop_phase(t):
    cur = "build"
    for at, ev in _LOOP:
        if at <= t:
            cur = ev
    return cur


def s_loop(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 20)
    p = _loop_phase(t)
    order = [e for _, e in _LOOP]
    idx = order.index(p)

    def past(ev):
        return order.index(ev) <= idx

    # main state of the original job
    if p == "build":
        bst, bw = "running", "claude"
    elif p.startswith("gate"):
        bst, bw = "gating", "cmd gates"
    elif p.startswith("verify"):
        bst, bw = "reviewing", "verifier"
    elif p.startswith("fail") or p.startswith("fix"):
        bst, bw = "fixing", "fix job"
    else:
        bst, bw = "parked", None
    job_node(cv, 16, 50, "build", "auth-api", bst, w=104, t=t, worker=bw,
             sub="waits for you" if bst == "parked" else None)
    gst = "gating" if p.startswith("gate") else ("done" if idx > 1 else "pending")
    job_node(cv, 150, 50, "gates", "cmd·owns·scan", "gating" if p.startswith("gate") else "pending",
             w=104, t=t, sub="cargo test -p auth")
    vfail = p.startswith("fail") or p in ("fix1", "fix2") and False
    vst = "reviewing" if p.startswith("verify") else ("failed" if p.startswith("fail") else "pending")
    job_node(cv, 284, 50, "verify", "independent", vst, w=104, t=t,
             sub="pass or findings" if vst != "failed" else "2 findings ✗")
    job_node(cv, 418, 50, "merge", "→ main", "pending", w=96, t=t, sub="only on pass")
    arrow(cv, 122, 63, 147, 63, "shadow" if idx >= 1 else "line")
    arrow(cv, 256, 63, 281, 63, "shadow" if idx >= 2 else "line")
    arrow(cv, 390, 63, 415, 63, "line", dash=2, phase=-ph)
    cv.text(402, 52, "pass", "dim", shadow=None, align="center")

    # fix lane
    def fix_state(n):
        f = f"fix{n}"
        if p == f:
            return "running"
        if past(f):
            return "failed" if past(f"fail{n + 1}") else "done"
        return "pending"

    for n, x in ((1, 150), (2, 284)):
        st = fix_state(n)
        if st == "failed":
            st_draw = "failed"
        else:
            st_draw = st
        job_node(cv, x, 132, "fix", f"fix #{n}", st_draw, w=104, t=t,
                 worker="claude" if st == "running" else None,
                 sub="same worktree" if st != "running" else None)
    # arrows: verify fail -> fix, fix -> back to gates
    col_f1 = "red" if past("fail1") else "line"
    cv.vline(336, 86, 108, col_f1, 2, -ph)
    cv.hline(202, 336, 108, col_f1, 2, -ph)
    arrow(cv, 202, 108, 202, 129, col_f1, dash=2, phase=-ph)
    cv.text(270, 98, "✗ fail → fix", "red" if past("fail1") else "dim2", align="center", shadow=None)
    col_f2 = "red" if past("fail2") else "line"
    arrow(cv, 350, 86, 350, 129, col_f2, dash=2, phase=-ph)
    col_b = "shadow" if past("gate2") else "line"
    cv.hline(150 - 8, 150, 150, col_b)
    cv.vline(142, 78, 150, col_b, 2, ph)
    arrow(cv, 142, 78, 147, 78, col_b)
    cv.text(70, 104, "back to", "dim", align="center", shadow=None)
    cv.text(70, 114, "gating", "dim", align="center", shadow=None)
    cv.hline(256, 284, 150, "line", 2, -ph)

    # attempts counter
    fixes = (1 if past("fix1") else 0) + (1 if past("fix2") else 0)
    panel(cv, 418, 132, 96, 40, tone="red" if fixes == 2 and past("fail3") else "grey", fill="bg2")
    cv.text(426, 138, "fix rounds", "dim", shadow=None)
    cv.text(426, 150, f"{fixes} / 2", "red" if fixes == 2 else "white", scale=2, shadow=None)

    # escalation
    if past("fail3"):
        arrow(cv, 515, 152, 526, 152, "amber")
    if past("escalate"):
        cv.text(530, 140, "on_exhausted", "dim", shadow=None)
        cv.text(530, 150, "= escalate", "amber", shadow=None)
        job_node(cv, 524, 50, "decision", "auth-api-dec", "decision", w=104, t=t, sub="blocks needs", badge="open")
        arrow(cv, 566, 134, 566, 80, "amber", dash=2, phase=ph)
        arrow(cv, 616, 78, 616, 192, "amber", dash=2, phase=-ph)
        cv.text(612, 176, "inbox", "amber", shadow=None, align="right")

    # other work keeps moving
    cv.text(16, 132, "meanwhile", "dim")
    others = [("login-ui", 2.0, 6.0), ("billing", 0.0, 4.0), ("guide", 5.0, 9.5), ("i18n", 8.0, 99)]
    for i, (nm, a, b) in enumerate(others):
        y = 144 + i * 11
        if t < a:
            cv.text(16, y, f"{nm} ready", "amber", shadow=None)
        elif t < b:
            cv.text(16, y, f"{nm} running", "blue", shadow=None)
        else:
            cv.text(16, y, f"{nm} merged ✓", "light", shadow=None)

    # inbox card
    if past("inbox") or still:
        k = 1.0 if still else ease((t - 10.6) / 0.4)
        y0 = 196 + int((1 - k) * 100)
        panel(cv, 16, y0, 608, 92, tone="amber", fill="panel")
        envelope(cv, 24, y0 + 8, "amber")
        cv.text(46, y0 + 10, "INBOX · decision · project app · job auth-api", "amber", shadow=None)
        cv.text(616, y0 + 10, "you decide", "amber", shadow=None, align="right")
        lines = [
            ("Why: 2 fix rounds used. `cargo test -p auth` still exits 101 after each fix.", "white"),
            ("Verifier: refresh-token test races the session lease (findings F1, F2).", "pale"),
            ("Evidence: gate tails, diff and verifier report are linked in the item.", "dim"),
            ("Options: [A] answer + retry   [B] replan   [C] park   [D] cancel", "light"),
            ("Only auth-api and the jobs that need it wait. Everything else keeps running.", "pale"),
        ]
        n = 10 ** 6 if still else int((t - 11.0) * 150)
        for i, (s, col) in enumerate(lines):
            if n <= 0:
                break
            cv.typed(24, y0 + 26 + i * 12, s, n, col, cursor=not still, shadow=None)
            n -= len(s)
    else:
        cv.text(320, 226, "verify → fix → verify, bounded: max_attempts = 2", "dim", align="center")
        cv.text(320, 240, "a gate must run the thing, never just `test -f`", "dim2", align="center")
    if not (past("inbox") or still):
        caption(cv, ["Verify fails, a fix runs, gates run again.",
                     "Two failed fixes go to your inbox."], t, t0=0.6, still=still)


# ================================================================ 09 long horizon
_FEED = [
    (0.6, "d1 09:02", "goal set from your phone: auth v2 (planner splits 7 jobs)", "pale"),
    (1.3, "d1 18:40", "laptop closed — sessions and jobs keep running on the home", "light"),
    (2.2, "d2 03:15", "provider limit hit → capsule hands off to the next ladder rung", "blue"),
    (3.1, "d2 21:48", "supervisor crashed → watchdog restarts L1 (heartbeat > 180 s)", "red"),
    (4.0, "d3 08:30", "inbox: you answer auth-api from your phone; only it was waiting", "amber"),
    (4.9, "d4 14:05", "capacity brake: RAM pressure, no new runs until it clears", "violet"),
    (5.8, "d5 02:00", "nightly gc --apply + backup (never secrets)", "dim"),
    (6.7, "d6 06:00", "cron: weekly dependency audit job created by a schedule", "pale"),
    (7.6, "d7 11:20", "auth v2 merged; promote waits for your yes in the inbox", "light"),
]


def s_long(cv, t, still=False):
    grid_bg(cv)
    T = 9.0
    k = 1.0 if still else clamp01(t / T)
    day = 1 + k * 7
    x0, x1 = 24, 616
    cv.rect(x0, 46, x1 - x0, 6, "panel2")
    cv.rect(x0, 46, int((x1 - x0) * k), 6, "main")
    for d in range(8):
        x = x0 + int((x1 - x0) * d / 7)
        cv.vline(x, 42, 55, "line")
        if d < 7:
            cv.text(x + 4, 30, f"day {d + 1}", "dim", shadow=None)
    # day/night strip
    for i in range(0, x1 - x0, 2):
        f = (i / (x1 - x0)) * 7
        night = (f % 1) < 0.35
        if i < (x1 - x0) * k:
            cv.rect(x0 + i, 56, 2, 2, "blue_d" if night else "amber_d")
    mx = x0 + int((x1 - x0) * k)
    cv.rect(mx - 1, 40, 3, 20, "white")
    for at, stamp, s, col in _FEED:
        fx = x0 + int((x1 - x0) * (at / T))
        if t >= at or still:
            cv.rect(fx - 1, 60, 3, 5, col)

    # counters
    vals = [("ticks", int(10080 * k)), ("jobs merged", int(38 * k)), ("fix loops", int(9 * k)),
            ("handoffs", int(3 * k)), ("you needed", int(4 * k))]
    for i, (lab, v) in enumerate(vals):
        x = 24 + i * 120
        panel(cv, x, 72, 112, 40, tone="green" if i != 4 else "amber", fill="panel")
        cv.text(x + 8, 78, f"{v:,}", "white" if i != 4 else "amber", scale=2, shadow=None)
        cv.text(x + 8, 98, lab, "dim", shadow=None)

    panel(cv, 16, 122, 608, 140, title="supervisor.log (condensed)", tone="green", fill="bg2")
    shown = [f for f in _FEED if f[0] <= t or still]
    for i, (at, stamp, s, col) in enumerate(shown):
        y = 134 + i * 14
        cv.text(26, y, stamp, "dim2", shadow=None)
        n = 10 ** 6 if still else int((t - at) * 80)
        cv.typed(90, y, s, n, col, cursor=False, shadow=None)
    cv.text(320, 270, "Nothing important lives only in memory: a restart reconciles from DB, logs and git.",
            "dim", align="center")
    caption(cv, ["Long-horizon work: days, not minutes.",
                 "The home keeps ticking while you sleep."], t, t0=0.6, still=still)


# ================================================================ 10 secrets
def s_secrets(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 22)
    panel(cv, 16, 30, 196, 112, title="you set it", tone="grey", fill="bg2")
    cv.text(24, 44, "$ vowp secret set STRIPE_KEY", "light", shadow=None)
    n = 10 ** 6 if still else int(max(0, t - 0.5) * 8)
    cv.text(24, 56, "value (hidden): " + "*" * min(10, n), "dim", shadow=None)
    cv.text(24, 74, "or a one-time page:", "pale", shadow=None)
    cv.text(24, 86, "$ vowp secret request NAME", "light", shadow=None)
    cv.text(24, 98, "→ http://127.0.0.1/…", "dim", shadow=None)
    cv.text(24, 110, "  expires in 10 min, used once", "dim", shadow=None)
    cv.text(24, 126, "never paste a secret in a chat", "red", shadow=None)

    arrow(cv, 214, 86, 250, 86, "amber", dash=2, phase=-ph)
    if not still:
        packet(cv, 214, 86, 250, 86, (t * 0.8) % 1, "amber")
    panel(cv, 254, 30, 132, 112, title="vault", tone="amber", fill="panel")
    db_cyl(cv, 264, 46, 30, 34, tone="amber")
    lock(cv, 274, 60, "black")
    cv.text(302, 48, "vowp-vault", "white", shadow=None)
    cv.text(302, 58, "user, or", "dim", shadow=None)
    cv.text(302, 68, "0600 files", "dim", shadow=None)
    cv.text(264, 92, "DB keeps the name", "pale", shadow=None)
    cv.text(264, 102, "and last 4 only", "pale", shadow=None)
    cv.text(264, 118, "never backed up", "amber", shadow=None)

    arrow(cv, 388, 86, 424, 86, "amber", dash=2, phase=-ph)
    panel(cv, 428, 30, 196, 112, title="worker run", tone="blue", fill="panel")
    cv.text(436, 44, "injected by NAME", "white", shadow=None)
    cv.text(436, 56, "env_pass = [\"STRIPE_KEY\"]", "blue", shadow=None)
    cv.text(436, 74, "the model only ever sees:", "dim", shadow=None)
    if on(t, 1.6) or still:
        cv.rect(436, 86, 180, 26, "panel2")
        cv.frame(436, 86, 180, 26, "shadow")
        cv.text(442, 90, "STRIPE_KEY saved,", "light", shadow=None)
        cv.text(442, 100, "ends in …4f2a", "light", shadow=None)
    cv.text(436, 122, "value never in a prompt", "pale", shadow=None)

    panel(cv, 16, 156, 608, 92, title="where the value can NOT leak", tone="green", fill="bg2")
    items = [
        ("redacted", "snapshot · session events · capsules · reports · run mirror"),
        ("blocked", "secret_scan gate on worker jobs and in the merge queue (rule names only)"),
        ("excluded", "backups never include secrets; harness logins stay with the harness"),
        ("by name", "plans, briefs and chats carry names, never values"),
    ]
    for i, (tag, s) in enumerate(items):
        if on(t, 2.2 + i * 0.5) or still:
            y = 170 + i * 15
            chip(cv, 24, y - 2, tag, "green", w=62)
            cv.text(94, y, s, "pale", shadow=None)
    if on(t, 4.4) or still:
        cv.text(24, 234, "Honest limit: code running in a process that holds a secret can print it.", "amber",
                shadow=None)
    caption(cv, ["Secrets are referenced by name.",
                 "Values never enter a chat, a plan or a backup."], t, t0=0.6, still=still)


# ================================================================ 11 delivery
def s_delivery(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 22)
    panel(cv, 16, 30, 608, 104, title="one merge queue per project · one merge at a time", tone="green",
          fill="bg2")
    steps = [("rebase", "on main"), ("repo checks", "check_cmds"), ("secret scan", "diff"), ("fast-forward", "main")]
    for i, (a, b) in enumerate(steps):
        x = 196 + i * 106
        active = (int(t * 1.6) % 5) == i + 1 and not still
        panel(cv, x, 50, 96, 36, tone="blue" if active else "green", fill="panel")
        cv.text(x + 48, 58, a, "white", align="center", shadow=None)
        cv.text(x + 48, 70, b, "dim", align="center", shadow=None)
        if i < 3:
            arrow(cv, x + 97, 68, x + 105, 68, "shadow")
    q = ["e2e-auth", "guide", "billing"]
    for i, nm in enumerate(q):
        y = 46 + i * 14
        cv.rect(28, y, 84, 12, "panel2")
        cv.rect(28, y, 2, 12, "light")
        cv.text(34, y + 3, nm, "white", shadow=None)
    arrow(cv, 116, 68, 192, 68, "light", dash=2, phase=-ph)
    cv.text(28, 96, "conflict? a merge job fixes it in the same worktree. Rejected push? retry x3.", "dim",
            shadow=None)
    cv.text(28, 110, "Done means gates passed with recorded evidence, not an agent saying so.", "pale",
            shadow=None)

    panel(cv, 16, 148, 608, 120, title="Work → Production · vowp promote <change> --to prod",
          tone="amber", fill="bg2")
    pipe = [("guards", "checks"), ("bank", "snap live"), ("diff", "intended"),
            ("your yes", "approval"), ("deploy", "commands"), ("probe", "must pass"), ("record", "+rollback")]
    k = len(pipe) if still else min(len(pipe), int(t * 1.3) + 1)
    for i, (a, b) in enumerate(pipe):
        x = 24 + i * 85
        tone = "amber" if a == "your yes" else ("green" if i < k else "grey")
        panel(cv, x, 168, 78, 36, tone=tone, fill="panel")
        cv.text(x + 39, 176, a, "white" if i < k else "dim", align="center", shadow=None)
        cv.text(x + 39, 188, b, "dim", align="center", shadow=None)
        if i < 6:
            arrow(cv, x + 79, 186, x + 84, 186, "shadow")
    if k >= 7:
        cv.hline(560, 560, 210, "red")
        arrow(cv, 563, 206, 563, 226, "red", dash=2, phase=-ph, head=False)
        arrow(cv, 563, 226, 80, 226, "red", dash=2, phase=ph)
        cv.text(320, 232, "probe fails? vowp rollback redeploys the banked snapshot", "red", align="center",
                shadow=None)
    cv.text(320, 250, "Production, truth, open work and secrets are never auto-deleted.", "dim",
            align="center", shadow=None)
    caption(cv, ["One merge queue. Evidence, not claims.",
                 "Guarded, reversible Work → Production."], t, t0=0.6, still=still)


# ================================================================ 12 the whole picture
def s_overview(cv, t, still=False):
    grid_bg(cv)
    ph = int(t * 22)
    # you
    panel(cv, 10, 30, 104, 258, title="you", tone="grey", fill="bg2")
    laptop(cv, 26, 46, t=t, w=72)
    cv.text(62, 92, "Mac / desktop", "dim", align="center", shadow=None)
    phone(cv, 51, 108, t=t)
    cv.text(62, 152, "phone", "dim", align="center", shadow=None)
    cv.text(62, 172, "steer, watch,", "pale", align="center", shadow=None)
    cv.text(62, 182, "answer inbox", "pale", align="center", shadow=None)
    envelope(cv, 54, 196, "amber")
    cv.text(62, 216, "apps = surfaces", "dim", align="center", shadow=None)
    cv.text(62, 228, "no truth here", "dim", align="center", shadow=None)
    cv.text(62, 248, "or orchestrate", "light", align="center", shadow=None)
    cv.text(62, 258, "locally as an", "light", align="center", shadow=None)
    cv.text(62, 268, "agent node", "light", align="center", shadow=None)
    arrow(cv, 116, 80, 134, 80, "shadow", dash=2, phase=-ph)
    arrow(cv, 134, 210, 116, 210, "amber", dash=2, phase=-ph)
    cv.text(125, 66, "ssh", "dim", align="center", shadow=None)

    # home
    panel(cv, 138, 30, 362, 258, title="HOME · always-on VPS", tone="green", fill="panel")
    panel(cv, 148, 44, 168, 38, tone="green", fill="panel2")
    spinner(cv, 162, 63, t * 1.5, "light", 6)
    cv.text(176, 50, "supervisor", "white", shadow=None)
    cv.text(176, 60, "L1–L9 loops, 60 s tick", "dim", shadow=None)
    cv.text(176, 70, "watchdog · schedules", "dim", shadow=None)
    db_cyl(cv, 326, 44, 26, 30)
    cv.text(339, 78, "global", "pale", align="center", shadow=None)
    for i in range(3):
        db_cyl(cv, 366 + i * 24, 50, 20, 24)
    cv.text(400, 78, "project DBs", "pale", align="center", shadow=None)
    doc_icon(cv, 448, 46, "pale")
    doc_icon(cv, 462, 50, "light")
    cv.text(466, 78, "sessions", "pale", align="center", shadow=None)

    # mini graph
    cv.text(148, 94, "job graph", "dim", shadow=None)
    mini = [("plan", 148, 106, "done"), ("build", 210, 100, "running"), ("build", 210, 122, "done"),
            ("verify", 272, 111, "reviewing"), ("merge", 334, 111, "pending")]
    for kind, x, y, st in mini:
        tone = {"done": "light", "running": "blue", "reviewing": "violet", "pending": "dim"}[st]
        cv.rect(x, y, 52, 16, "panel2")
        cv.frame(x, y, 52, 16, tone)
        cv.text(x + 26, y + 5, kind, tone, align="center", shadow=None)
    arrow(cv, 200, 114, 208, 108, "shadow")
    arrow(cv, 200, 114, 208, 130, "shadow")
    arrow(cv, 262, 108, 270, 118, "shadow")
    arrow(cv, 262, 130, 270, 120, "shadow")
    arrow(cv, 324, 119, 332, 119, "line")
    arrow(cv, 386, 119, 400, 119, "line")
    cv.text(404, 116, "main", "light", shadow=None)

    # loop
    cv.text(148, 150, "verify / fix loop", "dim", shadow=None)
    loop = [("verify ✗", "red", 148), ("fix #1", "red", 214), ("verify ✗", "red", 270),
            ("fix #2", "red", 336), ("inbox", "amber", 392)]
    for lab, col, x in loop:
        w = text_w(lab) + 8
        cv.rect(x, 162, w, 14, "panel2")
        cv.frame(x, 162, w, 14, col)
        cv.text(x + 4, 166, lab, col, shadow=None)
    for xa in (206, 262, 328, 384):
        arrow(cv, xa, 169, xa + 5, 169, "line")
    cv.text(148, 182, "2 fixes failed → decision with the evidence", "amber", shadow=None)

    # sessions tape
    cv.text(148, 200, "universal session log", "dim", shadow=None)
    for k in range(22):
        kind = "UATTAUATA CS ATTAUATAT"[k]
        col = _EV_COL.get(kind, "panel2")
        cv.rect(148 + k * 12, 212, 10, 10, col if kind != " " else "panel2")
    cv.text(148, 228, "same session: Claude Code → opencode → phone", "pale", shadow=None)

    cv.rect(148, 244, 342, 1, "line")
    lock(cv, 148, 252, "amber")
    cv.text(162, 254, "secrets by name · no public port", "amber", shadow=None)
    cv.text(162, 266, "measured capacity · brakes · gc", "dim", shadow=None)

    # workers / machines
    panel(cv, 512, 30, 118, 258, title="machines", tone="blue", fill="bg2")
    cv.text(520, 44, "workers on home", "pale", shadow=None)
    y = 58
    for nm in ("claude code", "opencode", "codex", "verifier"):
        worker_chip(cv, 520, y, nm, "violet" if nm == "verifier" else "blue", t + y * 0.01)
        y += 14
    cv.rect(520, 118, 102, 1, "line")
    cv.text(520, 126, "agent node", "pale", shadow=None)
    laptop(cv, 534, 140, t=t, w=60)
    cv.text(520, 188, "dials out over SSH", "dim", shadow=None)
    cv.text(520, 198, "local surface or", "dim", shadow=None)
    cv.text(520, 208, "offloaded jobs", "dim", shadow=None)
    cv.text(520, 218, "returns commits", "dim", shadow=None)
    arrow(cv, 510, 160, 502, 160, "blue", dash=2, phase=ph)
    cv.text(520, 240, "harness logins", "dim2", shadow=None)
    cv.text(520, 250, "stay with each", "dim2", shadow=None)
    cv.text(520, 260, "harness", "dim2", shadow=None)
    caption(cv, ["The VPS is the home. Agents work for days.",
                 "You come back to merged work and a short inbox."], t, t0=0.6, still=still)


# ================================================================ 13 outro
def s_outro(cv, t, still=False):
    grid_bg(cv)
    sc = 7
    tw = 4 * 6 * sc - sc
    x0 = W // 2 - tw // 2
    cv.dither(x0 + 5, 60, tw, 49, "outline", 0.5)
    cv.text(x0, 54, "VOWP", "light", scale=sc, shadow="deep")
    cv.text(W // 2, 128, "github.com/syver777/vowp", "white", scale=2, align="center")
    cv.text(W // 2, 156, "Paste one setup prompt into Claude Code or opencode.", "pale", align="center")
    cv.text(W // 2, 168, "It asks five questions, shows the whole plan once, then runs it.", "dim",
            align="center")
    x = W // 2 - 190
    for lab, tone in (("Linux VPS home", "green"), ("Mac/Linux nodes", "blue"),
                      ("Claude Code · opencode · Codex", "violet"), ("MIT", "amber")):
        x += chip(cv, x, 190, lab, tone) + 6
    if int(t * 2) % 2 == 0 or still:
        cv.text(W // 2, 222, "> your agents need a home that never sleeps_", "light", align="center")


SCENES = [
    # (key, section title, duration, fn, still time)
    ("title", "", 5.0, s_title, 5.0),
    ("why", "why a VPS", 7.5, s_why, 7.5),
    ("home", "the home", 9.0, s_home, 9.0),
    ("sessions", "universal sessions", 10.0, s_sessions, 5.2),
    ("orchestrate", "orchestrate anywhere", 8.0, s_orchestrate, 8.0),
    ("databases", "databases", 11.0, s_databases, 8.5),
    ("graph", "job graph", 11.5, s_graph, 5.0),
    ("loop", "verify / fix loop", 16.0, s_loop, 16.0),
    ("long-horizon", "long-horizon work", 10.5, s_long, 10.5),
    ("secrets", "secrets", 9.0, s_secrets, 9.0),
    ("delivery", "delivery", 9.0, s_delivery, 9.0),
    ("overview", "the whole picture", 8.0, s_overview, 8.0),
    ("outro", "", 5.0, s_outro, 5.0),
]


def draw_scene(i, t, still=False, prog=None):
    key, title, dur, fn, _ = SCENES[i]
    cv = Canvas(W, H)
    fn(cv, t, still=still)
    chrome(cv, i if title else 0, title, prog)
    return cv

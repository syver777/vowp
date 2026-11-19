"""A short clip for X: switch harness and model, keep the session.

Same pixel kit and look as the explainer (scenes.py). Five scenes, about 39 s:
the news, the lock-in, the switch, the provider ladder, the outro.

    python3 -I render_video.py OUT.mp4 --scenes switch_scenes
"""

from pixkit import (Canvas, panel, chip, arrow, packet, spinner, doc_icon, text_w,
                    ease, clamp01)
from scenes import W, H, caption, grid_bg, on, flash_frame

SID = "s-7f3a"
SW = 5.4          # scene 3: the moment the switch is recorded
_SRC = {"claude": "violet", "agy": "blue"}
_EV_COL = {"U": "pale", "A": "light", "T": "blue", "C": "amber", "S": "violet"}


def chrome(cv, idx, title, prog=None):
    cv.rect(0, 0, W, 17, "bg2")
    cv.rect(0, 17, W, 1, "outline")
    cv.rect(8, 5, 3, 7, "main")
    cv.rect(12, 7, 3, 5, "light")
    cv.rect(16, 3, 3, 9, "pale")
    cv.text(24, 5, "VOWP", "light", shadow=None)
    cv.text(56, 5, "switch harness, keep the session", "dim2", shadow=None)
    if idx:
        cv.text(W - 8, 5, f"{idx:02d} · {title}", "pale", shadow=None, align="right")
    if prog is not None:
        cv.rect(0, H - 3, W, 3, "outline")
        cv.rect(0, H - 3, int(W * prog), 3, "main")


def chip_row(cv, y, items, gap=8):
    total = sum(text_w(lab) + 8 for lab, _ in items) + gap * (len(items) - 1)
    x = W // 2 - total // 2
    for lab, tone in items:
        x += chip(cv, x, y, lab, tone) + gap


def ctext(cv, y, s, col="white", scale=1):
    cv.text(W // 2, y, s, col, scale=scale, align="center")


# ================================================================ 01 the news
def s_news(cv, t, still=False):
    grid_bg(cv)
    lab = "NEW MODEL · 18 NOV 2026"
    w = text_w(lab) + 8
    if still or t > 1.2 or int(t * 5) % 2 == 0:
        chip(cv, W // 2 - w // 2, 28, lab, "amber")
    title = "Gemini 4 Neon"
    n = 99 if still else int(t * 16)
    cv.typed(W // 2 - text_w(title, 4) // 2, 48, title, n, "white", scale=4, cursor=not still)

    grow = 1.0 if still else ease((t - 1.2) / 1.2)
    cards = [(28, "Claude Fable 5.2", "violet", 8, 40, "#1"), (332, "Gemini 4 Neon", "green", 3, 15, "#2")]
    for x, name, tone, pin, pout, rank in cards:
        if not still and t < 0.8:
            continue
        panel(cv, x, 96, 280, 128, title=name, tone=tone, fill="panel")
        col = "violet" if tone == "violet" else "light"
        for k, (lab2, p) in enumerate((("input", pin), ("output", pout))):
            y = 112 + k * 30
            cv.text(x + 12, y, f"{lab2}", "dim", shadow=None)
            cv.text(x + 268, y, f"${p} / 1M tokens", "white", align="right", shadow=None)
            bw = int(244 * p / 40 * grow)
            cv.rect(x + 12, y + 11, 244, 8, "bg2")
            cv.rect(x + 12, y + 11, bw, 8, col)
            cv.rect(x + 12, y + 18, bw, 1, "black")
        cv.text(x + 12, 178, "KingBench 4 · coding", "dim", shadow=None)
        cv.text(x + 268, 178, f"{rank} of 18", col, align="right", shadow=None)
        cv.text(x + 12, 194, "1M context", "dim2", shadow=None)
    if on(t, 2.6) or still:
        flash_frame(cv, 186, 236, 268, 20, t, 2.6, "amber")
        ctext(cv, 238, "62% less per token", "amber", scale=2)
        ctext(cv, 260, "sources: Google pricing, MindStudio KingBench 4", "dim2")
    caption(cv, ["A new model just dropped.",
                 "Close to the best. Under half the price."], t, t0=2.8, still=still)


# ================================================================ 02 the lock-in
def s_trap(cv, t, still=False):
    grid_bg(cv)
    fall_t = 3.0
    panel(cv, 24, 34, 262, 150, title="Claude Code", tone="violet", fill="panel")
    cv.text(36, 46, "session · 3 days of work", "pale", shadow=None)
    for i in range(4 * 14):
        r, k = divmod(i, 14)
        bx, by = 36 + k * 17, 62 + r * 22
        kind = "UATTAATAATATTA"[(i * 5 + r) % 14]
        if not still and t > fall_t:
            d = ((i * 37) % 23) / 23 * 0.9
            dt = t - fall_t - d
            if dt > 0:
                by += int(260 * dt * dt)
                if by > 166:
                    continue
                cv.rect(bx, by, 14, 16, "dim2")
                continue
        cv.rect(bx, by, 14, 16, _EV_COL[kind])
        cv.rect(bx, by + 15, 14, 1, "black")
        cv.text(bx + 4, by + 5, kind, "black", shadow=None)

    if (on(t, 4.6) or still):
        cv.text(155, 112, "(gone)", "dim2", align="center", shadow=None)
    panel(cv, 354, 34, 262, 150, title="Antigravity", tone="blue", fill="panel")
    cv.text(366, 46, "new session", "pale", shadow=None)
    if on(t, 3.4) or still:
        cv.text(366, 64, "context: 0 tokens", "red", shadow=None)
    if on(t, 4.0) or still:
        s = "> so, what is this project again?"
        n = 99 if still else int((t - 4.0) * 30)
        cv.typed(366, 88, s, n, "dim", cursor=not still)

    if on(t, 2.0) or still:
        ph = int(t * 20)
        red = still or t >= fall_t
        arrow(cv, 292, 110, 348, 110, "red" if red else "amber", dash=3, phase=-ph)
        cv.text(320, 96, "switch", "red" if red else "amber", align="center", shadow=None)
        if red:
            cv.text(320, 118, "✗ lost", "red", align="center", shadow=None)

    items = [("✗ re-explain the project", 4.2), ("✗ lose the tool history", 4.6), ("✗ lose the plan", 5.0)]
    for i, (s, t0) in enumerate(items):
        if on(t, t0) or still:
            cv.text(40 + i * 200, 204, s, "red")
    if on(t, 5.4) or still:
        ctext(cv, 228, "the conversation belonged to the tool, not to you", "dim")
    caption(cv, ["Most setups keep the session inside one tool.",
                 "Switch tools and you start from zero."], t, t0=0.6, still=still)


# ================================================================ 03 the switch
def _switch_events():
    evs = []
    pat = "UATTAATAATATTA"
    tt, i = -12.0, 0
    while tt < 2.6:
        evs.append((tt, pat[i % len(pat)], "claude"))
        tt += 0.3
        i += 1
    evs += [(4.4, "C", "claude"), (SW, "S", "agy"), (5.7, "U", "agy")]
    tt, i = 6.1, 1
    while tt < 13.5:
        evs.append((tt, pat[i % len(pat)], "agy"))
        tt += 0.3
        i += 1
    return evs


_SEVS = _switch_events()

_TERM = [
    (2.6, "✓ session frozen · agy probed: ok", "light"),
    (4.4, "✓ capsule: intent, decisions, files, next", "light"),
    (4.9, "✓ detached claude-code", "light"),
    (SW, "✓ attached cli:agy · seeded capsule + 2 turns", "light"),
    (5.9, "✓ switch recorded in the log", "light"),
    (6.6, "agy> picking up from the capsule…", "blue"),
]


def _harness_card(cv, x, name, model, tone, active, t):
    panel(cv, x, 30, 250, 46, tone=tone if active else "grey", fill="panel" if active else "bg2")
    cv.text(x + 10, 38, name, "white" if active else "dim", shadow=None)
    cv.text(x + 10, 52, f"model: {model}", "pale" if active else "dim2", shadow=None)
    if active:
        chip(cv, x + 190, 36, "attached", tone)
        spinner(cv, x + 234, 60, t, "light" if tone != "violet" else "violet", r=4)
    else:
        chip(cv, x + 190, 36, "detached", "grey")


def s_switch(cv, t, still=False):
    grid_bg(cv)
    after = t >= SW
    _harness_card(cv, 24, "Claude Code", "claude-fable-5.2", "violet", not after, t)
    _harness_card(cv, 366, "Antigravity CLI · agy", "gemini-4-neon", "blue", after, t)
    flash_frame(cv, 366, 30, 250, 46, t, SW, "white")
    if on(t, 2.6) or still:
        ph = int(t * 20)
        arrow(cv, 280, 53, 360, 53, "amber", dash=3, phase=-ph)
        cv.text(320, 40, "switch", "amber", align="center", shadow=None)

    # the tape: one log, owned by the home
    panel(cv, 16, 90, 608, 56, title=f"session {SID} · log.jsonl · owned by your home, not the tool",
          tone="green", fill="bg2")
    evs = [e for e in _SEVS if e[0] <= (t if not still else 13.0)]
    cap = 36
    start = max(0, len(evs) - cap)
    cap_x = None
    for k, (et, kind, src) in enumerate(evs[start:]):
        bx = 24 + k * 16
        fresh = (t - et) < 0.15 and not still
        cv.rect(bx, 104, 14, 16, "white" if fresh else _EV_COL[kind])
        cv.text(bx + 4, 109, kind, "black", shadow=None)
        cv.rect(bx, 120, 14, 2, _SRC[src])
        if kind == "C":
            cap_x = bx
    cur = 24 + min(len(evs), cap) * 16
    if still or int(t * 3) % 2:
        cv.rect(cur, 104, 2, 16, "light")
    x = 24
    for lab, colr in (("by Claude Code", "violet"), ("by agy", "blue")):
        cv.rect(x, 130, 10, 2, colr)
        cv.text(x + 14, 128, lab, "dim", shadow=None)
        x += text_w(lab) + 30
    for kind, lab in (("C", "capsule"), ("S", "switch")):
        cv.rect(x, 128, 7, 7, _EV_COL[kind])
        cv.text(x + 10, 128, lab, "dim", shadow=None)
        x += text_w(lab) + 22
    # the capsule travels from the log to the new harness
    if cap_x is not None and 4.4 <= t < SW + 0.3 and not still:
        p = ease((t - 4.5) / (SW - 4.5))
        x0, y0, x1, y1 = cap_x + 2, 100, 440, 72
        doc_icon(cv, int(x0 + (x1 - x0) * p), int(y0 + (y1 - y0) * p) - 8, "amber", w=10, h=12, lines=3)

    # terminal on the home
    panel(cv, 16, 158, 360, 128, title="terminal · your home", tone="green", fill="black")
    cmd = f"$ vowp session switch {SID} --to cli:agy"
    n = 99 if still else int((t - 0.8) * 28)
    cv.typed(26, 172, cmd, n, "white", cursor=False)
    row = 0
    for t0, s, col in _TERM:
        if still or t >= t0:
            cv.text(26, 190 + row * 14, s, col, shadow=None)
            row += 1
    if not still and int(t * 3) % 2 and t > 0.8:
        y = 172 if n < len(cmd) else 190 + row * 14
        xx = 26 + text_w(cmd[:n]) + 2 if n < len(cmd) else 26
        cv.rect(xx, y, 5, 7, "light")

    # what carries over
    panel(cv, 388, 158, 236, 128, title="carried over", tone="amber", fill="panel")
    keep = ["the whole conversation", "decisions + constraints", "files and diffs",
            "open tasks, next steps", f"same session id: {SID}"]
    for i, s in enumerate(keep):
        if still or t >= 7.0 + i * 0.4:
            cv.text(400, 174 + i * 20, "✓", "light")
            cv.text(414, 174 + i * 20, s, "pale", shadow=None)
    caption(cv, ["Same session. New harness. New model.",
                 "The history lives in your home, not the tool."], t, t0=8.6, still=still)


# ================================================================ 04 the ladder
_TOML = [
    ("[profiles.worker]", "light"),
    ("ladder = [", "pale"),
    ('  "cli:agy",                # gemini-4-neon', "pale"),
    ('  "claude-code:claude-fable-5-2",', "pale"),
    ('  "opencode:<provider>/<open-model>",', "pale"),
    ("]", "pale"),
    ('on_quota = "next_then_probe_back"', "amber"),
]


def _rung_state(i, t):
    # (state, note)
    if t < 2.8:
        return [("run", "default · cheapest"), ("idle", "fallback"), ("idle", "fallback")][i]
    if t < 5.8:
        return [("limit", "quota hit"), ("run", "auto switch · reason: limit"), ("idle", "fallback")][i]
    return [("run", "probed back · on neon again"), ("idle", "fallback"), ("idle", "fallback")][i]


def s_ladder(cv, t, still=False):
    grid_bg(cv)
    panel(cv, 16, 30, 300, 156, title="vowp.toml", tone="green", fill="black")
    budget = 10 ** 6 if still else int((t - 0.3) * 60)
    for i, (s, col) in enumerate(_TOML):
        if budget <= 0:
            break
        cv.typed(26, 46 + i * 16, s, budget, col, cursor=False)
        budget -= len(s)

    panel(cv, 332, 30, 292, 156, title="ladder · live", tone="blue", fill="panel")
    names = ["1  agy · gemini-4-neon", "2  claude code · fable-5.2", "3  opencode · open-weight"]
    tones = {"run": "light", "idle": "dim", "limit": "red"}
    tl = 13.0 if still else t
    for i, nm in enumerate(names):
        y = 46 + i * 42
        st, note = _rung_state(i, tl)
        fill = {"run": "panel2", "idle": "bg2", "limit": "red_d"}[st]
        cv.rect(344, y, 268, 32, fill)
        cv.frame(344, y, 268, 32, "black")
        cv.text(352, y + 6, nm, "white" if st != "idle" else "dim", shadow=None)
        cv.text(352, y + 19, note, tones[st], shadow=None)
        if st == "run":
            spinner(cv, 598, y + 16, t, "light", r=4)
        if st == "limit":
            cv.text(598, y + 12, "✗", "red", align="center", shadow=None)
    flash_frame(cv, 344, 88, 268, 32, t, 2.8, "white")
    flash_frame(cv, 344, 46, 268, 32, t, 5.8, "white")
    if 2.8 <= tl < 5.8:
        arrow(cv, 338, 62, 338, 102, "amber", dash=2, phase=int(t * 20))

    if on(t, 1.0) or still:
        ctext(cv, 204, f"one session id through every switch: {SID}", "pale")
    if on(t, 1.6) or still:
        chip_row(cv, 220, (("Claude Code", "violet"), ("Antigravity CLI", "blue"),
                           ("opencode", "green"), ("open-weight models", "amber")))
        ctext(cv, 244, "stock tools, no forks · the logins stay with each tool", "dim")
    caption(cv, ["Don't marry one provider.",
                 "New model next month? Change one line."], t, t0=3.2, still=still)


# ================================================================ 05 outro
def s_outro(cv, t, still=False):
    grid_bg(cv)
    for i in range(9):
        y = 40 + (i * 29) % 240
        x = int((t * (18 + i * 4) + i * 83) % (W + 40)) - 20
        cv.rect(x, y, 3, 3, "deep")
    r1 = 1.0 if still else clamp01(t / 0.6)
    r2 = 1.0 if still else clamp01((t - 0.7) / 0.6)
    a, b = "Models change.", "Your sessions shouldn't."
    xa, xb = W // 2 - text_w(a, 3) // 2, W // 2 - text_w(b, 3) // 2
    cv.text(xa, 104, a, "white", scale=3, shadow="deep")
    cv.rect(xa + int(text_w(a, 3) * r1), 100, 460, 30, "bg")
    cv.text(xb, 144, b, "light", scale=3, shadow="deep")
    cv.rect(xb + int(text_w(b, 3) * r2), 140, 460, 30, "bg")
    if on(t, 1.6) or still:
        ctext(cv, 198, "VOWP · an open-source framework for always-on AI agents", "pale")
    if on(t, 2.0) or still:
        chip_row(cv, 222, (("universal sessions", "green"), ("any harness", "blue"),
                           ("any model", "violet"), ("your own machine", "amber")))
    if (on(t, 2.4) and int(t * 2) % 2 == 0) or still:
        ctext(cv, 260, "> switch the harness, keep the session_", "light")


SCENES = [
    # (key, section title, duration, fn, still time)
    ("news", "the news", 6.0, s_news, 6.0),
    ("trap", "the lock-in", 7.2, s_trap, 7.2),
    ("switch", "the switch", 13.2, s_switch, 13.2),
    ("ladder", "the ladder", 7.8, s_ladder, 7.8),
    ("outro", "", 4.8, s_outro, 4.8),
]


def draw_scene(i, t, still=False, prog=None):
    key, title, dur, fn, _ = SCENES[i]
    cv = Canvas(W, H)
    fn(cv, t, still=still)
    chrome(cv, i + 1 if title else 0, title, prog)
    return cv

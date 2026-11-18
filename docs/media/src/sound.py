"""Sound for the VOWP explainer: a cue sheet taken from the scenes, a small
chiptune synth, and a mixer that builds three soundtracks.

    python3 -I sound.py OUT_DIR [--mcp DIR] [--only A|B|C] [--mux VIDEO OUT.mp4]

  A  code      : synthesized chiptune bed + synthesized effects (no downloads)
  B  models    : Mureka V9 bed + Mirelo SFX 2.0 effects  (needs --mcp)
  C  best mix  : ElevenLabs Music bed, Mirelo for the big moments, synth for
                 the precise UI ticks and blips      (needs --mcp)

--mcp is a folder of WAVs generated through the North Noir MCP:
music_mureka9.wav, music_eleven.wav and whoosh, pop, lid, ok, fail, inbox,
typing, lock, glitch, merge, hum, outro .wav.
"""
import argparse
import os
import subprocess
import sys
import wave

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scenes  # noqa: E402

SR = 48000
TOTAL = sum(s[2] for s in scenes.SCENES)
RNG = np.random.default_rng(7)


# ===================================================================== cue sheet
def scene_starts():
    out, t = {}, 0.0
    for key, _, dur, _, _ in scenes.SCENES:
        out[key] = t
        t += dur
    return out


def caption_spans():
    """Record each scene's caption call (lines, t0, cps) by stubbing caption()."""
    spans = {}
    real = scenes.caption
    for key, _, dur, fn, _ in scenes.SCENES:
        rec = []

        def fake(cv, lines, t, t0=0.5, cps=48, **kw):
            rec.append((lines, t0, cps))
        scenes.caption = fake
        fn(scenes.Canvas(), 0.0, still=False)
        if rec:
            lines, t0, cps = rec[0]
            spans[key] = (t0, sum(len(x) for x in lines) / cps)
    scenes.caption = real
    return spans


def cues():
    """List of (time_s, kind, gain)."""
    S = scene_starts()
    c = []

    def add(scene, t, kind, g=1.0):
        c.append((S[scene] + t, kind, g))

    for key in S:
        if S[key] > 0:
            add(key, 0.0, "whoosh", 0.55)
    # typing of captions
    for key, (t0, d) in caption_spans().items():
        c.append((S[key] + t0, "type", d))

    # 01 title
    add("title", 0.05, "startup", 0.9)
    c.append((S["title"] + 1.0, "type", 49 / 40))
    add("title", 2.4, "pop", 0.5)
    for i in range(4):
        add("title", 3.0 + i * 0.06, "blip", 0.35)
    # 02 why
    add("why", 1.65, "lid", 1.0)
    for i in range(4):
        add("why", 2.0 + i * 0.5, "deny", 0.45)
        add("why", 2.2 + i * 0.5, "ok", 0.35)
    add("why", 4.3, "pop", 0.6)
    # 03 home
    for t in (0.3, 0.6, 1.2, 1.8, 2.6, 3.4):
        add("home", t, "pop", 0.6)
    c.append((S["home"] + 0.2, "hum", 8.6))
    # 04 sessions: the log pulses as events append
    for et, kind in scenes._EVS:
        if 0.0 <= et < 10.0:
            add("sessions", et, "tick", 0.25 if kind not in "CSL" else 0.0)
    add("sessions", 2.5, "merge", 0.6)
    add("sessions", 4.6, "lock", 0.8)
    add("sessions", 7.0, "merge", 0.6)
    # 05 orchestrate
    for i in range(6):
        add("orchestrate", 1.0 + i * 0.3, "pop", 0.45)
    # 06 databases
    for i in range(9):
        add("databases", 0.2 + i * 0.15, "tick", 0.5)
    for name, tl in scenes._JOBS:
        for at, col in tl[1:]:
            add("databases", at, "ok" if col == "done" else "blip", 0.45 if col == "done" else 0.4)
    # 07 graph
    for k, v in scenes._G_NODES.items():
        for at, st, _ in v[4][1:]:
            kind = {"done": "ok", "merging": "merge"}.get(st, "blip")
            add("graph", at, kind, 0.5 if kind != "merge" else 0.7)
    # 08 loop
    for at, ev in scenes._LOOP:
        if ev.startswith("fail"):
            add("loop", at, "fail", 0.9)
        elif ev == "escalate":
            add("loop", at, "whoosh", 0.5)
        elif ev == "inbox":
            add("loop", at, "inbox", 1.0)
        else:
            add("loop", at, "blip", 0.5)
    c.append((S["loop"] + 11.0, "type", 360 / 150))
    # 09 long horizon
    for at, stamp, s, col in scenes._FEED:
        kind = {"red": "glitch", "amber": "inbox"}.get(col, "pop")
        add("long-horizon", at, kind, 0.8 if kind == "glitch" else (0.6 if kind == "inbox" else 0.5))
    add("long-horizon", 7.65, "ok", 0.6)
    # 10 secrets
    for i in range(10):
        add("secrets", 0.5 + i / 8, "key", 0.5)
    add("secrets", 0.3, "lock", 0.9)
    add("secrets", 1.6, "ok", 0.6)
    for i in range(4):
        add("secrets", 2.2 + i * 0.5, "pop", 0.45)
    add("secrets", 4.4, "deny", 0.35)
    # 11 delivery
    prev = None
    for f in range(int(9 * 30)):
        t = f / 30
        a = int(t * 1.6) % 5
        if a != prev and a != 0:
            add("delivery", t, "blip", 0.3)
        prev = a
    for i in range(7):
        add("delivery", i / 1.3, "pop", 0.45)
    add("delivery", 6 / 1.3 + 0.05, "merge", 0.6)
    # 12 overview, 13 outro
    add("overview", 0.5, "pop", 0.4)
    add("outro", 0.15, "outro", 1.0)
    return sorted(c)


# ===================================================================== synth
def tt(d):
    return np.arange(int(d * SR)) / SR


def env(n, a=0.002, r=0.08, d=None):
    """Attack / exponential release envelope over n samples."""
    t = np.arange(n) / SR
    e = np.minimum(1.0, t / max(a, 1e-4)) * np.exp(-t / max(r, 1e-4))
    return e


def sq(f, t, duty=0.5):
    ph = np.cumsum(np.broadcast_to(f, t.shape) / SR) % 1.0
    return np.where(ph < duty, 1.0, -1.0)


def tri(f, t):
    ph = np.cumsum(np.broadcast_to(f, t.shape) / SR) % 1.0
    return 4 * np.abs(ph - 0.5) - 1


def lowpass(x, fc):
    X = np.fft.rfft(x)
    f = np.fft.rfftfreq(len(x), 1 / SR)
    X *= 1 / np.sqrt(1 + (f / fc) ** 4)
    return np.fft.irfft(X, len(x))


def bandpass_sweep(x, f0, f1):
    """Crude sweeping band-pass: blend of a few static band-passes."""
    out = np.zeros_like(x)
    n = len(x)
    segs = 8
    for i in range(segs):
        fc = f0 * (f1 / f0) ** (i / (segs - 1))
        X = np.fft.rfft(x)
        f = np.fft.rfftfreq(n, 1 / SR)
        X *= np.exp(-((np.log(f + 1) - np.log(fc)) ** 2) / 0.3)
        y = np.fft.irfft(X, n)
        w = np.zeros(n)
        a, b = int(n * i / segs), int(n * (i + 1) / segs)
        w[max(0, a - n // 16):min(n, b + n // 16)] = 1
        out += y * lowpass(w, 30)
    return out


def note(m):
    return 440.0 * 2 ** ((m - 69) / 12)


def sfx_synth(kind, g=1.0):
    if kind == "pop":
        t = tt(0.07)
        f = 700 + 900 * (t / t[-1])
        y = np.sin(2 * np.pi * np.cumsum(f) / SR) * env(len(t), 0.001, 0.025)
        return 0.5 * y
    if kind == "blip":
        t = tt(0.09)
        m = RNG.choice([76, 79, 81, 84])
        y = sq(note(m), t, 0.25) * env(len(t), 0.001, 0.03)
        return 0.18 * lowpass(y, 6000)
    if kind == "tick":
        t = tt(0.02)
        y = RNG.standard_normal(len(t)) * env(len(t), 0.0005, 0.004)
        y += 0.5 * sq(2400, t) * env(len(t), 0.0005, 0.003)
        return 0.25 * y
    if kind == "key":
        t = tt(0.05)
        y = RNG.standard_normal(len(t)) * env(len(t), 0.0005, 0.006)
        y = lowpass(y, 5000) + 0.3 * np.sin(2 * np.pi * 180 * t) * env(len(t), 0.001, 0.01)
        return 0.5 * y
    if kind == "ok":
        out = []
        for m in (83, 88):
            t = tt(0.07)
            out.append(sq(note(m), t, 0.25) * env(len(t), 0.001, 0.05))
        return 0.2 * lowpass(np.concatenate(out), 7000)
    if kind == "deny":
        t = tt(0.22)
        f = 220 - 80 * (t / t[-1])
        y = sq(f, t, 0.5) * env(len(t), 0.002, 0.09)
        return 0.16 * lowpass(y, 2500)
    if kind == "fail":
        out = []
        for f0 in (196, 147):
            t = tt(0.16)
            y = 0.5 * (sq(f0, t) + sq(f0 * 1.01, t)) * env(len(t), 0.002, 0.12)
            out.append(y)
        return 0.22 * lowpass(np.concatenate(out), 2200)
    if kind == "whoosh":
        t = tt(0.6)
        y = bandpass_sweep(RNG.standard_normal(len(t)), 400, 5000)
        e = np.sin(np.pi * np.minimum(1, t / t[-1])) ** 2
        return 0.35 * y * e / (np.abs(y).max() + 1e-9)
    if kind == "lid":
        t = tt(0.35)
        thud = np.sin(2 * np.pi * (60 + 60 * np.exp(-t * 30)) * t) * env(len(t), 0.001, 0.08)
        click = RNG.standard_normal(len(t)) * env(len(t), 0.0005, 0.006)
        return 0.6 * thud + 0.15 * lowpass(click, 4000)
    if kind in ("inbox", "outro", "startup", "merge"):
        seq = {"inbox": [(79, 0.0), (84, 0.16)],
               "outro": [(72, 0.0), (76, 0.09), (79, 0.18), (84, 0.27), (88, 0.36)],
               "startup": [(60, 0.0), (64, 0.07), (67, 0.14), (72, 0.21), (76, 0.28), (79, 0.35)],
               "merge": [(72, 0.0), (76, 0.05), (79, 0.10), (84, 0.15)]}[kind]
        tail = {"inbox": 0.9, "outro": 2.2, "startup": 1.0, "merge": 0.5}[kind]
        n = int((seq[-1][1] + tail) * SR)
        y = np.zeros(n)
        for m, at in seq:
            t = tt(tail)
            f = note(m)
            bell = (np.sin(2 * np.pi * f * t) + 0.35 * np.sin(2 * np.pi * 2 * f * t)
                    + 0.12 * np.sin(2 * np.pi * 3.01 * f * t))
            if kind in ("startup", "merge"):
                bell = 0.6 * bell + 0.4 * sq(f, t, 0.25)
            bell *= env(len(t), 0.002, tail / 3)
            i = int(at * SR)
            y[i:i + len(t)] += bell[:n - i]
        if kind == "merge":
            sh = RNG.standard_normal(n) * env(n, 0.01, 0.2)
            y += 0.4 * bandpass_sweep(sh, 3000, 9000)
        return 0.22 * lowpass(y, 9000)
    if kind == "lock":
        y = np.zeros(int(0.25 * SR))
        for at in (0.0, 0.07):
            t = tt(0.12)
            c = RNG.standard_normal(len(t)) * env(len(t), 0.0003, 0.005)
            c += 0.6 * np.sin(2 * np.pi * 2900 * t) * env(len(t), 0.0005, 0.03)
            i = int(at * SR)
            y[i:i + len(t)] += c[:len(y) - i]
        return 0.35 * y
    if kind == "glitch":
        n = int(1.2 * SR)
        y = np.zeros(n)
        t = tt(0.45)
        noise = RNG.standard_normal(len(t))
        gate = (np.floor(t * 40) % 3 != 0).astype(float)
        crushed = np.round(noise * 3) / 3 * gate * env(len(t), 0.001, 0.25)
        y[:len(t)] += 0.4 * lowpass(crushed, 5000)
        up = sfx_synth("startup")
        i = int(0.55 * SR)
        y[i:i + len(up)] += up[:n - i]
        return y
    if kind == "hum":
        t = tt(g)
        y = 0.6 * np.sin(2 * np.pi * 55 * t) + 0.3 * np.sin(2 * np.pi * 110 * t)
        y += 0.5 * lowpass(RNG.standard_normal(len(t)), 600)
        fade = np.minimum(1, np.minimum(t / 0.8, (t[-1] - t) / 0.8))
        return 0.05 * y * fade
    raise KeyError(kind)


def typing_synth(dur):
    n = int(dur * SR) + SR // 10
    y = np.zeros(n)
    t = 0.0
    while t < dur:
        k = sfx_synth("key") * (0.35 + 0.25 * RNG.random())
        i = int(t * SR)
        y[i:i + len(k)] += k[:n - i]
        t += 0.045 + 0.04 * RNG.random()
    return y


# ===================================================================== music
CHORDS = {  # root midi, chord tones
    "C": (48, [60, 64, 67, 72]), "Am": (45, [57, 60, 64, 69]),
    "F": (41, [57, 60, 65, 69]), "G": (43, [59, 62, 67, 71]),
    "Dm": (50, [62, 65, 69, 74]), "Em": (52, [59, 64, 67, 71]), "E": (40, [56, 59, 64, 68]),
}


# Lead motif per chord: (beat, midi, length in beats). It comes back in every
# "lead" scene, so the film has one tune people can remember.
MOTIF = {
    "C": [(0, 76, 1), (1, 79, 0.5), (1.5, 81, 0.5), (2, 79, 1), (3, 76, 1)],
    "Am": [(0, 72, 1), (1, 76, 1), (2, 74, 0.5), (2.5, 72, 1.5)],
    "F": [(0, 69, 1), (1, 72, 0.5), (1.5, 74, 0.5), (2, 77, 1.5), (3.5, 76, 0.5)],
    "G": [(0, 74, 1.5), (1.5, 71, 0.5), (2, 74, 1), (3, 79, 1)],
    # tense variant for the verify/fix loop
    "Dm": [(0, 74, 1), (1, 77, 1), (2, 81, 2)],
    "E": [(0, 68, 1), (1, 71, 1), (2, 76, 2)],
}
MOTIF_TENSE = {"Am": [(0, 69, 1), (1, 72, 1), (2, 76, 2)], "F": [(0, 65, 1), (1, 69, 1), (2, 72, 2)]}

# Per-scene arrangement: arp density (16ths step), bass, drums, lead level, pad level
ARR = {
    "title":        dict(arp=2, bass=False, drums=0, lead=0.0, pad=0.10),
    "why":          dict(arp=2, bass=True, drums=1, lead=0.0, pad=0.10),
    "home":         dict(arp=1, bass=True, drums=2, lead=0.055, pad=0.09),
    "sessions":     dict(arp=1, bass=True, drums=2, lead=0.0, pad=0.09),
    "orchestrate":  dict(arp=1, bass=True, drums=2, lead=0.05, pad=0.09),
    "databases":    dict(arp=1, bass=True, drums=2, lead=0.045, pad=0.09),
    "graph":        dict(arp=1, bass=True, drums=2, lead=0.0, pad=0.09),
    "loop":         dict(arp=2, bass=True, drums=2, lead=0.035, pad=0.10, tense=True),
    "long-horizon": dict(arp=1, bass=True, drums=2, lead=0.055, pad=0.09),
    "secrets":      dict(arp=2, bass=True, drums=1, lead=0.0, pad=0.10),
    "delivery":     dict(arp=1, bass=True, drums=2, lead=0.045, pad=0.09),
    "overview":     dict(arp=1, bass=True, drums=3, lead=0.07, pad=0.12, chorus=True),
    "outro":        dict(arp=0, bass=False, drums=0, lead=0.0, pad=0.0, ending=True),
}


def lead_voice(f, d):
    t = tt(d)
    vib = 1 + 0.006 * np.sin(2 * np.pi * 5.5 * t) * np.clip((t - 0.12) * 6, 0, 1)
    y = 0.6 * sq(f * vib, t, 0.5) + 0.6 * tri(f * vib, t)
    e = np.minimum(1, t / 0.01) * np.clip((d - t) / 0.04, 0, 1) * (0.75 + 0.25 * np.exp(-t / 0.15))
    return y * e


def kick():
    tk = tt(0.18)
    return np.sin(2 * np.pi * (50 + 90 * np.exp(-tk * 35)) * tk) * env(len(tk), 0.001, 0.07)


def snare(g=1.0):
    ts_ = tt(0.14)
    tone = np.sin(2 * np.pi * 190 * ts_) * env(len(ts_), 0.001, 0.03)
    return g * (lowpass(RNG.standard_normal(len(ts_)), 5000) * env(len(ts_), 0.001, 0.045) + 0.4 * tone)


def hat():
    th = tt(0.04)
    h = RNG.standard_normal(len(th)) * env(len(th), 0.0005, 0.01)
    return h - lowpass(h, 7000)


def crash():
    tc = tt(1.6)
    c = RNG.standard_normal(len(tc))
    c = c - lowpass(c, 4000)
    return c * env(len(tc), 0.001, 0.45)


def music_synth(total):
    n = int(total * SR)
    mix = np.zeros(n + SR * 4)

    def put(x, t, g):
        i = int(t * SR)
        if 0 <= i < len(mix):
            mix[i:i + len(x)] += g * x[:len(mix) - i]

    def fill(t_end, beat):
        """A one-beat snare/tom fill that lands on t_end."""
        for k in range(4):
            t = t_end - beat + k * beat / 4
            tm = tt(0.12)
            tom = np.sin(2 * np.pi * (220 - 30 * k) * tm) * env(len(tm), 0.001, 0.06)
            put(snare(), t, 0.05 + 0.025 * k)
            put(tom, t, 0.10)

    t_scene = 0.0
    prog_main = ["C", "Am", "F", "G"]
    prog_tense = ["Am", "F", "Dm", "E"]
    for key, _, dur, _, _ in scenes.SCENES:
        a = ARR[key]
        beats = max(4, int(round(dur / 0.6)))
        beat = dur / beats          # every scene holds a whole number of beats
        prog = prog_tense if a.get("tense") else prog_main
        if a.get("ending"):
            tl = tt(total - t_scene + 0.5)
            pad = sum(tri(note(m), tl) for m in CHORDS["C"][1]) / 4 * np.exp(-tl / 2.4)
            put(pad, t_scene, 0.17)
            put(sum(tri(note(m), tl) for m in (36, 48)) / 2 * np.exp(-tl / 2.0), t_scene, 0.14)
            for i, m in enumerate((84, 79, 76, 72, 67, 64, 60)):   # descending sparkle
                td = tt(0.5)
                put(sq(note(m), td, 0.125) * env(len(td), 0.001, 0.12), t_scene + 0.25 + i * 0.3, 0.035)
            put(crash(), t_scene, 0.08)
            put(kick(), t_scene, 0.3)
            break
        if key != "title":
            put(crash(), t_scene, 0.05 if not a.get("chorus") else 0.09)
        b = 0
        while b * 4 < beats:
            t0 = t_scene + b * 4 * beat
            nb = min(4, beats - b * 4)          # last bar may be cut short by the scene change
            ch = prog[b % 4]
            root, tones = CHORDS[ch]
            # pad
            tb = tt(nb * beat)
            pad = sum(tri(note(m), tb) for m in tones[:3]) / 3
            pad *= np.minimum(1, np.minimum(tb / 0.15, (tb[-1] - tb) / 0.05))
            put(pad, t0, a["pad"])
            # arpeggio
            if a["arp"]:
                for s in range(0, nb * 4, a["arp"]):
                    m = tones[[0, 1, 2, 3, 2, 1, 2, 3][s % 8]] + 12
                    ta = tt(beat / 4)
                    put(sq(note(m), ta, 0.125 if not a.get("tense") else 0.25) * env(len(ta), 0.001, 0.05),
                        t0 + s * beat / 4, 0.042)
                    if a.get("chorus"):
                        put(sq(note(m + 12), ta, 0.125) * env(len(ta), 0.001, 0.04), t0 + s * beat / 4, 0.022)
            # bass
            if a["bass"]:
                for e in range(nb * 2):
                    m = root + (0 if e % 4 != 3 else 7)
                    tb8 = tt(beat / 2)
                    put(tri(note(m), tb8) * env(len(tb8), 0.003, 0.18), t0 + e * beat / 2, 0.16)
            # drums: 1 = kick + hats, 2 = full, 3 = full + extra kick
            if a["drums"]:
                for q in range(nb):
                    tq = t0 + q * beat
                    if q in (0, 2) or (a["drums"] == 3 and q == 3):
                        put(kick(), tq if q != 3 else tq + beat / 2, 0.30)
                    if a["drums"] >= 2 and q in (1, 3):
                        put(snare(), tq, 0.07)
                    put(hat(), tq + beat / 2, 0.05)
                    if a["drums"] == 3:
                        put(hat(), tq, 0.03)
            # lead
            if a["lead"]:
                mot = (MOTIF_TENSE.get(ch) or MOTIF[ch]) if a.get("tense") else MOTIF[ch]
                for bt, m, ln in mot:
                    if bt >= nb:
                        continue
                    d = min(ln, nb - bt) * beat * 0.92
                    put(lead_voice(note(m), d), t0 + bt * beat, a["lead"])
                    if a.get("chorus"):
                        put(lead_voice(note(m - 12), d), t0 + bt * beat, a["lead"] * 0.45)
            b += 1
        t_scene += dur

    # lifts: a fill into the inbox moment and into the final chorus
    S = scene_starts()
    lb = 16.0 / int(round(16.0 / 0.6))
    inbox_t = S["loop"] + 10.6
    fill(inbox_t, lb)
    put(crash(), inbox_t, 0.06)
    db = 9.0 / int(round(9.0 / 0.6))
    fill(S["overview"], db)

    mix = mix[:n]
    fade = np.ones(n)
    fi = int(0.6 * SR)
    fade[:fi] = np.linspace(0, 1, fi)
    return lowpass(mix, 10000) * fade


# ===================================================================== io + mix
def read_wav(path):
    """Read a 16/24/32-bit PCM or float WAV via ffmpeg into mono float @ SR."""
    raw = subprocess.run(["ffmpeg", "-loglevel", "error", "-i", path, "-f", "f32le", "-ac", "1",
                          "-ar", str(SR), "-"], capture_output=True, check=True).stdout
    return np.frombuffer(raw, dtype=np.float32).astype(np.float64)


def trim(x, thr=0.02):
    idx = np.nonzero(np.abs(x) > thr * np.abs(x).max())[0]
    if len(idx) == 0:
        return x
    a = max(0, idx[0] - int(0.003 * SR))
    return x[a:idx[-1] + int(0.05 * SR)]


def load_mcp(d):
    names = ["whoosh", "pop", "lid", "ok", "fail", "inbox", "typing", "lock", "glitch", "merge", "hum", "outro"]
    out = {}
    for nm in names:
        x = read_wav(os.path.join(d, nm + ".wav"))
        out[nm] = x if nm in ("hum", "typing") else trim(x)
    return out


def place(buf, x, t, g):
    i = int(t * SR)
    if i >= len(buf):
        return
    x = x[:len(buf) - i]
    buf[i:i + len(x)] += g * x


def speed(x, r):
    idx = np.arange(0, len(x) - 1, r)
    return np.interp(idx, np.arange(len(x)), x)


def build_sfx(cue_list, mode, mcp=None):
    n = int(TOTAL * SR) + SR
    buf = np.zeros(n)
    # which kinds come from the model library in each mode
    model_kinds = {
        "A": set(),
        "B": {"whoosh", "pop", "blip", "tick", "lid", "ok", "deny", "fail", "inbox", "type", "key",
              "lock", "glitch", "merge", "hum", "outro", "startup"},
        "C": {"whoosh", "lid", "fail", "inbox", "lock", "glitch", "merge", "hum", "outro"},
    }[mode]
    lib_map = {"blip": "pop", "tick": "pop", "deny": "fail", "key": "typing", "startup": "outro"}
    lib_gain = {"pop": 0.35, "blip": 0.22, "tick": 0.10, "deny": 0.3, "whoosh": 0.45, "ok": 0.4,
                "fail": 0.55, "inbox": 0.7, "lid": 0.7, "lock": 0.6, "glitch": 0.6, "merge": 0.5,
                "hum": 0.18, "outro": 0.7, "startup": 0.5, "key": 0.3}
    for t, kind, g in cue_list:
        if kind == "type":
            dur = g
            if "type" in model_kinds:
                ty = mcp["typing"]
                seg = ty[:int(min(dur, len(ty) / SR) * SR)].copy()
                k = min(len(seg), int(0.05 * SR))
                seg[-k:] *= np.linspace(1, 0, k)
                place(buf, seg, t, 0.28)
            else:
                place(buf, typing_synth(dur), t, 0.55)
            continue
        if kind == "tick" and g == 0:
            continue
        if kind in model_kinds and mcp is not None:
            src = mcp[lib_map.get(kind, kind)]
            if kind == "hum":
                L = int(g * SR)
                reps = int(np.ceil(L / len(src)))
                x = np.tile(src, reps)[:L]
                fade = np.minimum(1, np.minimum(np.arange(L) / (0.8 * SR), (L - np.arange(L)) / (0.8 * SR)))
                place(buf, x * fade, t, lib_gain["hum"])
                continue
            x = src
            if kind == "key":
                x = src[int(RNG.random() * (len(src) - 0.1 * SR)):][:int(0.08 * SR)]
            if kind in ("blip", "tick"):
                x = speed(src, RNG.choice([1.0, 1.12, 1.26]))[:int(0.25 * SR)]
            place(buf, x, t, lib_gain.get(kind, 0.5) * (0.6 + 0.4 * g))
        else:
            x = sfx_synth(kind, g) if kind != "hum" else sfx_synth("hum", g)
            place(buf, x, t, 1.0 if kind == "hum" else g)
    return buf


def duck(music, sfx, depth=0.45):
    """Lower the bed under effects (simple sidechain on the effects envelope)."""
    e = np.abs(sfx)
    win = int(0.08 * SR)
    k = np.ones(win) / win
    e = np.convolve(e, k, mode="same")
    e = e / (e.max() + 1e-9)
    g = 1 - depth * np.clip(e * 4, 0, 1)
    g = np.convolve(g, np.ones(int(0.15 * SR)) / int(0.15 * SR), mode="same")
    return music * g[:len(music)]


def fit(x, n):
    if len(x) >= n:
        return x[:n]
    return np.concatenate([x, np.zeros(n - len(x))])


def rms_norm(x, target_db):
    r = np.sqrt(np.mean(x ** 2)) + 1e-12
    return x * (10 ** (target_db / 20) / r)


def write_wav(path, x):
    x = np.tanh(x * 1.1) / np.tanh(1.1)  # gentle soft-clip
    pcm = (np.clip(x, -1, 1) * 32767).astype(np.int16)
    st = np.repeat(pcm[:, None], 2, axis=1)
    with wave.open(path, "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(st.tobytes())


def master(music, sfx, music_db, sfx_db):
    n = int(TOTAL * SR)
    m = rms_norm(fit(music, n), music_db)
    s = fit(sfx, n)
    s = s / (np.abs(s).max() + 1e-9) * 10 ** (sfx_db / 20)
    m = duck(m, s)
    end = int(1.5 * SR)
    fade = np.ones(n)
    fade[-end:] = np.linspace(1, 0, end) ** 2
    return (m + s) * fade


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--mcp")
    ap.add_argument("--only", choices=["A", "B", "C"])
    ap.add_argument("--mux", nargs=2, metavar=("VIDEO", "OUT"),
                    help="mux the last rendered soundtrack onto VIDEO at -14 LUFS")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    cl = cues()
    print(f"{len(cl)} cues over {TOTAL:.1f} s")
    todo = [a.only] if a.only else (["A", "B", "C"] if a.mcp else ["A"])
    mcp = load_mcp(a.mcp) if a.mcp and set(todo) & {"B", "C"} else None
    for v in todo:
        if v == "A":
            music = music_synth(TOTAL)
            mix = master(music, build_sfx(cl, "A"), -29, -5)
        elif v == "B":
            music = read_wav(os.path.join(a.mcp, "music_mureka9.wav"))
            mix = master(music, build_sfx(cl, "B", mcp), -23, -8)
        else:
            music = read_wav(os.path.join(a.mcp, "music_eleven.wav"))
            mix = master(music, build_sfx(cl, "C", mcp), -27, -5)
        p = os.path.join(a.out, f"sound-{v}.wav")
        write_wav(p, mix)
        print(p)
    if a.mux:
        video, out = a.mux
        subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-i", video, "-i", p, "-map", "0:v", "-map", "1:a",
                        "-c:v", "copy", "-af", "loudnorm=I=-14:TP=-1.5:LRA=11", "-ar", str(SR),
                        "-c:a", "aac", "-b:a", "192k", "-shortest", "-movflags", "+faststart", out], check=True)
        print(out)


if __name__ == "__main__":
    main()

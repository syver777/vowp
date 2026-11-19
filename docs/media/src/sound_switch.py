"""Code-only soundtrack for the harness-switch clip (switch_scenes.py).

Same synth and effects as version A of the explainer (sound.py): a chiptune bed
at ~100 BPM where every scene holds a whole number of beats, plus effects cued
from the scene timelines.

    python3 -I sound_switch.py OUT.wav [--mux VIDEO OUT.mp4]
"""
import argparse
import os
import subprocess
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import switch_scenes as sc  # noqa: E402
from sound import (SR, CHORDS, MOTIF, MOTIF_TENSE, sfx_synth, typing_synth, lead_voice, kick,  # noqa: E402
                   snare, hat, crash, tri, sq, tt, env, note, lowpass, place, duck, fit, rms_norm,
                   write_wav)

TOTAL = sum(s[2] for s in sc.SCENES)


def starts():
    out, t = {}, 0.0
    for key, _, dur, _, _ in sc.SCENES:
        out[key] = t
        t += dur
    return out


def caption_spans():
    spans = {}
    real = sc.caption
    for key, _, dur, fn, _ in sc.SCENES:
        rec = []

        def fake(cv, lines, t, t0=0.5, cps=48, **kw):
            rec.append((lines, t0, cps))
        sc.caption = fake
        fn(sc.Canvas(), 0.0, still=False)
        if rec:
            lines, t0, cps = rec[0]
            spans[key] = (t0, sum(len(x) for x in lines) / cps)
    sc.caption = real
    return spans


def cues():
    S = starts()
    c = []

    def add(scene, t, kind, g=1.0):
        c.append((S[scene] + t, kind, g))

    for key in S:
        if S[key] > 0:
            add(key, 0.0, "whoosh", 0.55)
    for key, (t0, d) in caption_spans().items():
        c.append((S[key] + t0, "type", d))

    # 01 news
    add("news", 0.05, "startup", 0.9)
    for t in (0.0, 0.4, 0.8):
        add("news", t, "blip", 0.45)
    c.append((S["news"], "type", 13 / 16))
    add("news", 0.8, "pop", 0.6)
    add("news", 0.9, "pop", 0.5)
    for i in range(6):
        add("news", 1.2 + i * 0.2, "tick", 0.35)
    add("news", 2.6, "ok", 0.8)
    add("news", 2.62, "pop", 0.5)
    # 02 the lock-in
    add("trap", 0.1, "pop", 0.5)
    add("trap", 2.0, "blip", 0.5)
    add("trap", 3.0, "glitch", 0.9)
    add("trap", 3.05, "fail", 0.8)
    add("trap", 3.4, "deny", 0.5)
    c.append((S["trap"] + 4.0, "type", 33 / 30))
    for t in (4.2, 4.6, 5.0):
        add("trap", t, "deny", 0.4)
    # 03 the switch
    c.append((S["switch"] + 0.8, "type", 41 / 28))
    add("switch", 2.6, "blip", 0.6)              # enter
    for et, kind, src in sc._SEVS:
        if 0.0 <= et < 13.2 and kind not in "CS":
            add("switch", et, "tick", 0.22)
    for t0, s, col in sc._TERM:
        add("switch", t0, "ok" if col == "light" else "pop", 0.45)
    add("switch", 4.4, "pop", 0.6)               # capsule leaves the log
    add("switch", sc.SW, "merge", 0.9)           # the switch lands
    for i in range(5):
        add("switch", 7.0 + i * 0.4, "ok", 0.35)
    # 04 the ladder
    n = sum(len(s) for s, _ in sc._TOML)
    c.append((S["ladder"] + 0.3, "type", n / 60))
    add("ladder", 1.6, "pop", 0.45)
    add("ladder", 2.8, "fail", 0.8)
    add("ladder", 2.95, "whoosh", 0.4)
    add("ladder", 3.1, "merge", 0.6)
    add("ladder", 5.8, "ok", 0.8)
    # 05 outro
    add("outro", 0.1, "outro", 1.0)
    return sorted(c)


# per scene: before / after an optional lift (time in the scene)
ARR = {
    "news":   dict(arp=2, bass=True, drums=1, lead=0.0, pad=0.10),
    "trap":   dict(arp=2, bass=True, drums=1, lead=0.03, pad=0.10, tense=True),
    "switch": dict(arp=2, bass=True, drums=1, lead=0.0, pad=0.09,
                   lift=sc.SW, after=dict(arp=1, bass=True, drums=2, lead=0.05, pad=0.09)),
    "ladder": dict(arp=1, bass=True, drums=3, lead=0.06, pad=0.11, chorus=True),
    "outro":  dict(ending=True),
}


def music():
    n = int(TOTAL * SR)
    mix = np.zeros(n + SR * 4)

    def put(x, t, g):
        i = int(t * SR)
        if 0 <= i < len(mix):
            mix[i:i + len(x)] += g * x[:len(mix) - i]

    def fill(t_end, beat):
        for k in range(4):
            t = t_end - beat + k * beat / 4
            tm = tt(0.12)
            tom = np.sin(2 * np.pi * (220 - 30 * k) * tm) * env(len(tm), 0.001, 0.06)
            put(snare(), t, 0.05 + 0.025 * k)
            put(tom, t, 0.10)

    t_scene = 0.0
    for key, _, dur, _, _ in sc.SCENES:
        a0 = ARR[key]
        if a0.get("ending"):
            tl = tt(TOTAL - t_scene + 0.5)
            put(sum(tri(note(m), tl) for m in CHORDS["C"][1]) / 4 * np.exp(-tl / 2.4), t_scene, 0.17)
            put(sum(tri(note(m), tl) for m in (36, 48)) / 2 * np.exp(-tl / 2.0), t_scene, 0.14)
            for i, m in enumerate((84, 79, 76, 72, 67, 64, 60)):
                td = tt(0.5)
                put(sq(note(m), td, 0.125) * env(len(td), 0.001, 0.12), t_scene + 0.25 + i * 0.3, 0.035)
            put(crash(), t_scene, 0.08)
            put(kick(), t_scene, 0.3)
            break
        beats = max(4, int(round(dur / 0.6)))
        beat = dur / beats
        prog = ["Am", "F", "Dm", "E"] if a0.get("tense") else ["C", "Am", "F", "G"]
        if t_scene > 0:
            put(crash(), t_scene, 0.05 if not a0.get("chorus") else 0.09)
        lift = a0.get("lift")
        b = 0
        while b * 4 < beats:
            t0 = t_scene + b * 4 * beat
            nb = min(4, beats - b * 4)
            ch = prog[b % 4]
            root, tones = CHORDS[ch]
            for q in range(nb * 4):           # step through 16ths so a lift can land mid-bar
                ts = t0 + q * beat / 4
                a = a0["after"] if lift is not None and ts - t_scene >= lift else a0
                beat_q, sub = divmod(q, 4)
                if q == 0 or (lift is not None and abs(ts - t_scene - lift) < beat / 8):
                    # pad for the rest of this bar
                    steps = nb * 4 - q
                    if lift is not None and q == 0:
                        lq = (t_scene + lift - ts) / (beat / 4)
                        if 0 < lq < steps:
                            steps = int(round(lq))
                    tb = tt(steps * beat / 4)
                    pad = sum(tri(note(m), tb) for m in tones[:3]) / 3
                    pad *= np.minimum(1, np.minimum(tb / 0.15, (tb[-1] - tb) / 0.05))
                    put(pad, ts, a["pad"])
                if a["arp"] and q % a["arp"] == 0:
                    m = tones[[0, 1, 2, 3, 2, 1, 2, 3][q % 8]] + 12
                    ta = tt(beat / 4)
                    put(sq(note(m), ta, 0.125 if not a.get("tense") else 0.25) * env(len(ta), 0.001, 0.05),
                        ts, 0.042)
                    if a.get("chorus"):
                        put(sq(note(m + 12), ta, 0.125) * env(len(ta), 0.001, 0.04), ts, 0.022)
                if a["bass"] and sub % 2 == 0:
                    e = q // 2
                    m = root + (0 if e % 4 != 3 else 7)
                    tb8 = tt(beat / 2)
                    put(tri(note(m), tb8) * env(len(tb8), 0.003, 0.18), ts, 0.16)
                if a["drums"] and sub == 0:
                    if beat_q in (0, 2) or (a["drums"] == 3 and beat_q == 3):
                        put(kick(), ts if beat_q != 3 else ts + beat / 2, 0.30)
                    if a["drums"] >= 2 and beat_q in (1, 3):
                        put(snare(), ts, 0.07)
                    put(hat(), ts + beat / 2, 0.05)
                    if a["drums"] == 3:
                        put(hat(), ts, 0.03)
            # lead: motif per bar, from whichever arrangement owns the bar's start
            for bt, m, ln in (MOTIF_TENSE.get(ch) or MOTIF[ch]) if a0.get("tense") else MOTIF[ch]:
                if bt >= nb:
                    continue
                ts = t0 + bt * beat
                a = a0["after"] if lift is not None and ts - t_scene >= lift else a0
                if not a["lead"]:
                    continue
                d = min(ln, nb - bt) * beat * 0.92
                put(lead_voice(note(m), d), ts, a["lead"])
                if a.get("chorus"):
                    put(lead_voice(note(m - 12), d), ts, a["lead"] * 0.45)
            b += 1
        if lift is not None:
            fill(t_scene + lift, beat)
            put(crash(), t_scene + lift, 0.07)
        t_scene += dur

    S = starts()
    fill(S["ladder"], 13.2 / 22)
    mix = mix[:n]
    fade = np.ones(n)
    fi = int(0.4 * SR)
    fade[:fi] = np.linspace(0, 1, fi)
    return lowpass(mix, 10000) * fade


def build_sfx(cue_list):
    buf = np.zeros(int(TOTAL * SR) + SR)
    for t, kind, g in cue_list:
        if kind == "type":
            place(buf, typing_synth(g), t, 0.55)
        else:
            place(buf, sfx_synth(kind, g), t, g)
    return buf


def master(m, s, music_db=-29, sfx_db=-5):
    n = int(TOTAL * SR)
    m = rms_norm(fit(m, n), music_db)
    s = fit(s, n)
    s = s / (np.abs(s).max() + 1e-9) * 10 ** (sfx_db / 20)
    m = duck(m, s)
    end = int(1.5 * SR)
    fade = np.ones(n)
    fade[-end:] = np.linspace(1, 0, end) ** 2
    return (m + s) * fade


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--mux", nargs=2, metavar=("VIDEO", "OUT"))
    a = ap.parse_args()
    cl = cues()
    print(f"{len(cl)} cues over {TOTAL:.1f} s")
    write_wav(a.out, master(music(), build_sfx(cl)))
    print(a.out)
    if a.mux:
        video, out = a.mux
        subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-i", video, "-i", a.out, "-map", "0:v",
                        "-map", "1:a", "-c:v", "copy", "-af", "loudnorm=I=-14:TP=-1.5:LRA=11",
                        "-ar", str(SR), "-c:a", "aac", "-b:a", "192k", "-shortest",
                        "-movflags", "+faststart", out], check=True)
        print(out)


if __name__ == "__main__":
    main()

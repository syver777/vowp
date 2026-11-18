"""Render the VOWP explainer video (and a short GIF of one scene) with ffmpeg.

usage: python3 -I render_video.py OUT.mp4 [--fps 30] [--scale 3] [--gif SCENE OUT.gif]
"""
import argparse
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import numpy as np  # noqa: E402
from PIL import Image  # noqa: E402

from pixkit import BAYER4  # noqa: E402
from scenes import SCENES, draw_scene, W, H  # noqa: E402

FADE = 0.4  # seconds of dither dissolve at the start of each scene


def bayer_mask(k, block=2):
    """True where the new frame shows, for dissolve progress k in [0, 1]."""
    b = np.array(BAYER4)
    tile = np.kron(b, np.ones((block, block), dtype=int))
    reps = (H // tile.shape[0] + 1, W // tile.shape[1] + 1)
    m = np.tile(tile, reps)[:H, :W]
    return m < int(k * 16 + 0.5)


def frames(fps, only=None):
    total = sum(s[2] for s in SCENES)
    idxs = [i for i, s in enumerate(SCENES) if only is None or s[0] == only]
    elapsed = sum(SCENES[i][2] for i in range(idxs[0]))
    prev_last = None
    for i in idxs:
        key, title, dur, fn, _ = SCENES[i]
        n = int(round(dur * fps))
        last = None
        for f in range(n):
            t = f / fps
            prog = (elapsed + t) / total
            img = np.asarray(draw_scene(i, t, prog=prog).img)
            if prev_last is not None and t < FADE:
                m = bayer_mask(t / FADE)[..., None]
                img = np.where(m, img, prev_last)
            last = img
            yield img
        prev_last = last
        elapsed += dur


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--fps", type=int, default=30)
    ap.add_argument("--scale", type=int, default=3)
    ap.add_argument("--gif", nargs=2, metavar=("SCENE", "OUT"))
    a = ap.parse_args()

    sw, sh = W * a.scale, H * a.scale
    cmd = ["ffmpeg", "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgb24",
           "-s", f"{sw}x{sh}", "-r", str(a.fps), "-i", "-",
           "-c:v", "libx264", "-preset", "slow", "-crf", "16", "-tune", "animation",
           "-pix_fmt", "yuv420p", "-movflags", "+faststart", a.out]
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    count = 0
    for img in frames(a.fps):
        big = Image.fromarray(img).resize((sw, sh), Image.NEAREST)
        proc.stdin.write(big.tobytes())
        count += 1
    proc.stdin.close()
    proc.wait()
    print(f"{a.out}: {count} frames, {count / a.fps:.1f} s")

    if a.gif:
        scene, out = a.gif
        gfps, gscale = 12, 2
        gw, gh = W * gscale, H * gscale
        tmp = out + ".tmp.mp4"
        cmd = ["ffmpeg", "-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgb24",
               "-s", f"{gw}x{gh}", "-r", str(gfps), "-i", "-", "-c:v", "libx264", "-crf", "0",
               "-pix_fmt", "yuv444p", tmp]
        proc = subprocess.Popen(cmd, stdin=subprocess.PIPE)
        for img in frames(gfps, only=scene):
            proc.stdin.write(Image.fromarray(img).resize((gw, gh), Image.NEAREST).tobytes())
        proc.stdin.close()
        proc.wait()
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", tmp, "-vf",
                        "split[a][b];[a]palettegen=max_colors=48:stats_mode=full[p];"
                        "[b][p]paletteuse=dither=none", "-loop", "0", out], check=True)
        os.remove(tmp)
        print(out)


if __name__ == "__main__":
    main()

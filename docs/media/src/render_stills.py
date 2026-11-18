"""Render one still per scene into docs/images/ (and a contact sheet for review)."""
import os
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from scenes import SCENES, draw_scene

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "..", "images")
os.makedirs(OUT, exist_ok=True)
for i, (key, title, dur, fn, still_t) in enumerate(SCENES):
    if key in ("title", "outro"):
        continue
    cv = draw_scene(i, still_t, still=True)
    path = os.path.join(OUT, f"vowp-{key}.png")
    cv.save(path, scale=2)
    print(path)

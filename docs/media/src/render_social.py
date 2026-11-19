"""Draw the link-preview card (GitHub social preview, X, Slack...): 1280x640.

    python3 -I docs/media/src/render_social.py docs/images/vowp-social.png

Same kit and palette as the explainer's outro. Content stays inside a centred
safe area so a 1.91:1 crop (X, LinkedIn) loses nothing.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from pixkit import Canvas, chip, server, laptop, phone, arrow, text_w  # noqa: E402

W, H = 640, 320


def draw():
    cv = Canvas(W, H)
    for y in range(8, H, 12):
        for x in range(6, W, 12):
            cv.px(x, y, "panel2")
    for i, (x, y) in enumerate(((40, 36), (590, 60), (70, 270), (560, 250), (300, 20), (130, 140),
                                (510, 150), (220, 292), (430, 290))):
        cv.rect(x, y, 3, 3, "deep")

    # mark + logotype
    sc = 8
    word = "VOWP"
    tw = text_w(word, sc)
    mark_w = 30
    x0 = W // 2 - (tw + mark_w + 12) // 2
    mx = x0
    for dx, dy, h, col in ((0, 18, 38, "main"), (11, 28, 28, "light"), (22, 8, 48, "pale")):
        cv.rect(mx + dx, 46 + dy, 8, h, col)
        cv.rect(mx + dx + 1, 46 + dy + h, 8, 1, "black")
    lx = x0 + mark_w + 12
    cv.text(lx, 50, word, "light", scale=sc, shadow="deep")

    tag = "A VPS framework for AI agents"
    cv.text(W // 2, 128, tag, "white", scale=2, align="center")
    cv.text(W // 2, 152, "your agents need a home that never sleeps", "pale", align="center")

    # devices -> home
    laptop(cv, 150, 186, lid=1.0, t=0.3, w=48)
    phone(cv, 214, 192, t=0.3)
    arrow(cv, 240, 210, 290, 210, "amber", dash=3)
    server(cv, 300, 178, units=4, t=0.3, w=44)
    arrow(cv, 352, 210, 402, 210, "amber", dash=3)
    for i, (lab, tone) in enumerate((("agent", "blue"), ("agent", "violet"), ("agent", "green"))):
        chip(cv, 410, 186 + i * 15, lab, tone)
    cv.text(470, 190, "work for days", "dim", shadow=None)
    cv.text(470, 205, "verify / fix loop", "dim", shadow=None)
    cv.text(470, 220, "inbox for you", "dim", shadow=None)

    items = (("universal sessions", "green"), ("any harness", "blue"),
             ("job graphs", "violet"), ("open source · MIT", "amber"))
    total = sum(text_w(lab) + 8 for lab, _ in items) + 8 * (len(items) - 1)
    x = W // 2 - total // 2
    for lab, tone in items:
        x += chip(cv, x, 262, lab, tone) + 8
    cv.rect(0, H - 4, W, 4, "main")
    return cv


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "vowp-social.png"
    draw().save(out, scale=2)
    print(out)

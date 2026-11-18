"""Pixel-art drawing kit for the VOWP explainer images and video.

Everything is drawn on a small canvas (640x360 by default) with hard pixels and
a hand-made 5x7 bitmap font, then scaled up with nearest-neighbour. The palette
follows the StackSpirit "Meadow" sheet: greens on a near-black ground, with
amber, red and blue reserved for job states.
"""

from PIL import Image, ImageDraw

# ---------------------------------------------------------------- palette
PAL = {
    "bg": (13, 18, 15),        # #0D120F
    "bg2": (17, 24, 20),       # #111814
    "panel": (21, 29, 24),     # #151D18
    "panel2": (26, 46, 33),    # #1A2E21
    "line": (52, 68, 58),      # #34443A
    "outline": (11, 55, 32),   # #0B3720
    "deep": (26, 91, 56),      # #1A5B38
    "shadow": (42, 124, 68),   # #2A7C44
    "main": (67, 168, 90),     # #43A85A
    "light": (124, 203, 107),  # #7CCB6B
    "pale": (205, 237, 176),   # #CDEDB0
    "white": (245, 247, 240),  # #F5F7F0
    "dim": (141, 155, 145),    # #8D9B91
    "dim2": (90, 104, 95),
    "amber": (240, 184, 96),   # #F0B860
    "amber_d": (120, 86, 36),
    "red": (240, 138, 128),    # #F08A80
    "red_d": (110, 44, 40),
    "blue": (127, 178, 234),   # #7FB2EA
    "blue_d": (32, 60, 92),
    "violet": (186, 160, 240),
    "violet_d": (64, 50, 104),
    "black": (8, 12, 11),
}

# ---------------------------------------------------------------- font
# 5 columns, 7 rows above the baseline, plus up to 2 descender rows.
_G = {
    "A": ".###. #...# #...# ##### #...# #...# #...#",
    "B": "####. #...# #...# ####. #...# #...# ####.",
    "C": ".###. #...# #.... #.... #.... #...# .###.",
    "D": "####. #...# #...# #...# #...# #...# ####.",
    "E": "##### #.... #.... ####. #.... #.... #####",
    "F": "##### #.... #.... ####. #.... #.... #....",
    "G": ".###. #...# #.... #.### #...# #...# .####",
    "H": "#...# #...# #...# ##### #...# #...# #...#",
    "I": ".###. ..#.. ..#.. ..#.. ..#.. ..#.. .###.",
    "J": "..### ...#. ...#. ...#. ...#. #..#. .##..",
    "K": "#...# #..#. #.#.. ##... #.#.. #..#. #...#",
    "L": "#.... #.... #.... #.... #.... #.... #####",
    "M": "#...# ##.## #.#.# #.#.# #...# #...# #...#",
    "N": "#...# #...# ##..# #.#.# #..## #...# #...#",
    "O": ".###. #...# #...# #...# #...# #...# .###.",
    "P": "####. #...# #...# ####. #.... #.... #....",
    "Q": ".###. #...# #...# #...# #.#.# #..#. .##.#",
    "R": "####. #...# #...# ####. #.#.. #..#. #...#",
    "S": ".#### #.... #.... .###. ....# ....# ####.",
    "T": "##### ..#.. ..#.. ..#.. ..#.. ..#.. ..#..",
    "U": "#...# #...# #...# #...# #...# #...# .###.",
    "V": "#...# #...# #...# #...# #...# .#.#. ..#..",
    "W": "#...# #...# #...# #.#.# #.#.# #.#.# .#.#.",
    "X": "#...# #...# .#.#. ..#.. .#.#. #...# #...#",
    "Y": "#...# #...# .#.#. ..#.. ..#.. ..#.. ..#..",
    "Z": "##### ....# ...#. ..#.. .#... #.... #####",
    "a": "..... ..... .###. ....# .#### #...# .####",
    "b": "#.... #.... ####. #...# #...# #...# ####.",
    "c": "..... ..... .###. #.... #.... #...# .###.",
    "d": "....# ....# .#### #...# #...# #...# .####",
    "e": "..... ..... .###. #...# ##### #.... .###.",
    "f": "..##. .#..# .#... ###.. .#... .#... .#...",
    "g": "..... ..... .#### #...# #...# #...# .#### ....# .###.",
    "h": "#.... #.... #.##. ##..# #...# #...# #...#",
    "i": "..#.. ..... .##.. ..#.. ..#.. ..#.. .###.",
    "j": "...#. ..... ..##. ...#. ...#. ...#. ...#. #..#. .##..",
    "k": "#.... #.... #..#. #.#.. ##... #.#.. #..#.",
    "l": ".##.. ..#.. ..#.. ..#.. ..#.. ..#.. .###.",
    "m": "..... ..... ##.#. #.#.# #.#.# #.#.# #.#.#",
    "n": "..... ..... #.##. ##..# #...# #...# #...#",
    "o": "..... ..... .###. #...# #...# #...# .###.",
    "p": "..... ..... ####. #...# #...# #...# ####. #.... #....",
    "q": "..... ..... .#### #...# #...# #...# .#### ....# ....#",
    "r": "..... ..... #.##. ##..# #.... #.... #....",
    "s": "..... ..... .###. #.... .###. ....# ####.",
    "t": ".#... .#... ###.. .#... .#... .#..# ..##.",
    "u": "..... ..... #...# #...# #...# #..## .##.#",
    "v": "..... ..... #...# #...# #...# .#.#. ..#..",
    "w": "..... ..... #...# #...# #.#.# #.#.# .#.#.",
    "x": "..... ..... #...# .#.#. ..#.. .#.#. #...#",
    "y": "..... ..... #...# #...# #...# #...# .#### ....# .###.",
    "z": "..... ..... ##### ...#. ..#.. .#... #####",
    "0": ".###. #...# #..## #.#.# ##..# #...# .###.",
    "1": "..#.. .##.. ..#.. ..#.. ..#.. ..#.. .###.",
    "2": ".###. #...# ....# ...#. ..#.. .#... #####",
    "3": "####. ....# ....# .###. ....# ....# ####.",
    "4": "...#. ..##. .#.#. #..#. ##### ...#. ...#.",
    "5": "##### #.... ####. ....# ....# #...# .###.",
    "6": "..##. .#... #.... ####. #...# #...# .###.",
    "7": "##### ....# ...#. ..#.. .#... .#... .#...",
    "8": ".###. #...# #...# .###. #...# #...# .###.",
    "9": ".###. #...# #...# .#### ....# ...#. .##..",
    " ": "..... ..... ..... ..... ..... ..... .....",
    ".": "..... ..... ..... ..... ..... .##.. .##..",
    ",": "..... ..... ..... ..... ..... .##.. .##.. ..#.. .#...",
    ":": "..... .##.. .##.. ..... .##.. .##.. .....",
    ";": "..... .##.. .##.. ..... .##.. .##.. ..#.. .#...",
    "!": "..#.. ..#.. ..#.. ..#.. ..#.. ..... ..#..",
    "?": ".###. #...# ....# ...#. ..#.. ..... ..#..",
    "-": "..... ..... ..... .###. ..... ..... .....",
    "_": "..... ..... ..... ..... ..... ..... #####",
    "/": "....# ....# ...#. ..#.. .#... #.... #....",
    "\\": "#.... #.... .#... ..#.. ...#. ....# ....#",
    "(": "...#. ..#.. .#... .#... .#... ..#.. ...#.",
    ")": ".#... ..#.. ...#. ...#. ...#. ..#.. .#...",
    "[": ".###. .#... .#... .#... .#... .#... .###.",
    "]": ".###. ...#. ...#. ...#. ...#. ...#. .###.",
    "{": "...## ..#.. ..#.. .#... ..#.. ..#.. ...##",
    "}": "##... ..#.. ..#.. ...#. ..#.. ..#.. ##...",
    "<": "...#. ..#.. .#... #.... .#... ..#.. ...#.",
    ">": ".#... ..#.. ...#. ....# ...#. ..#.. .#...",
    "=": "..... ..... ##### ..... ##### ..... .....",
    "+": "..... ..#.. ..#.. ##### ..#.. ..#.. .....",
    "*": "..... #.#.# .###. ##### .###. #.#.# .....",
    "#": ".#.#. .#.#. ##### .#.#. ##### .#.#. .#.#.",
    "%": "##..# ##..# ...#. ..#.. .#... #..## #..##",
    "'": "..#.. ..#.. .#... ..... ..... ..... .....",
    '"': ".#.#. .#.#. ..... ..... ..... ..... .....",
    "@": ".###. #...# #.### #.#.# #.### #.... .###.",
    "&": ".##.. #..#. #.#.. .#... #.#.# #..#. .##.#",
    "|": "..#.. ..#.. ..#.. ..#.. ..#.. ..#.. ..#..",
    "~": "..... ..... .#... #.#.# ...#. ..... .....",
    "$": "..#.. .#### #.#.. .###. ..#.# ####. ..#..",
    "^": "..#.. .#.#. #...# ..... ..... ..... .....",
    "`": ".#... ..#.. ..... ..... ..... ..... .....",
    "·": "..... ..... ..... ..#.. ..... ..... .....",  # middle dot
    "…": "..... ..... ..... ..... ..... ..... #.#.#",  # ellipsis
    "→": "..... ..#.. ...#. ##### ...#. ..#.. .....",  # right arrow
    "←": "..... ..#.. .#... ##### .#... ..#.. .....",  # left arrow
    "↑": "..#.. .###. #.#.# ..#.. ..#.. ..#.. .....",  # up arrow
    "↓": "..... ..#.. ..#.. ..#.. #.#.# .###. ..#..",  # down arrow
    "✓": "..... ....# ...## #.##. ###.. .#... .....",  # check
    "✗": "..... #...# .#.#. ..#.. .#.#. #...# .....",  # cross
    "—": "..... ..... ..... ##### ..... ..... .....",  # em dash
    "–": "..... ..... ..... ####. ..... ..... .....",  # en dash
    "•": "..... ..... .###. .###. .###. ..... .....",  # bullet
}
GLYPHS = {}
for _ch, _rows in _G.items():
    rows = _rows.split()
    GLYPHS[_ch] = [[c == "#" for c in r] for r in rows]

ADV = 6        # advance per glyph at scale 1
LINE_H = 10    # line height at scale 1


def text_w(s, scale=1):
    return len(s) * ADV * scale - scale if s else 0


class Canvas:
    def __init__(self, w=640, h=360, bg="bg"):
        self.w, self.h = w, h
        self.img = Image.new("RGB", (w, h), c(bg))
        self.d = ImageDraw.Draw(self.img)

    # -- primitives (inclusive pixel boxes) --------------------------------
    def rect(self, x, y, w, h, col):
        if w <= 0 or h <= 0:
            return
        self.d.rectangle([int(x), int(y), int(x + w - 1), int(y + h - 1)], fill=c(col))

    def frame(self, x, y, w, h, col, t=1):
        self.rect(x, y, w, t, col)
        self.rect(x, y + h - t, w, t, col)
        self.rect(x, y, t, h, col)
        self.rect(x + w - t, y, t, h, col)

    def px(self, x, y, col):
        if 0 <= x < self.w and 0 <= y < self.h:
            self.img.putpixel((int(x), int(y)), c(col))

    def hline(self, x0, x1, y, col, dash=0, phase=0):
        if x1 < x0:
            x0, x1 = x1, x0
        if not dash:
            self.rect(x0, y, x1 - x0 + 1, 1, col)
            return
        for x in range(int(x0), int(x1) + 1):
            if ((x + phase) // dash) % 2 == 0:
                self.px(x, y, col)

    def vline(self, x, y0, y1, col, dash=0, phase=0):
        if y1 < y0:
            y0, y1 = y1, y0
        if not dash:
            self.rect(x, y0, 1, y1 - y0 + 1, col)
            return
        for y in range(int(y0), int(y1) + 1):
            if ((y + phase) // dash) % 2 == 0:
                self.px(x, y, col)

    def dither(self, x, y, w, h, col, density=0.5):
        """Ordered (Bayer 4x4) dither fill - the shading style of the brand sheet."""
        th = int(density * 16)
        for yy in range(int(y), int(y + h)):
            for xx in range(int(x), int(x + w)):
                if BAYER4[yy & 3][xx & 3] < th:
                    self.px(xx, yy, col)

    # -- text -------------------------------------------------------------
    def text(self, x, y, s, col="white", scale=1, shadow="black", align="left"):
        s = str(s)
        if align == "center":
            x = x - text_w(s, scale) // 2
        elif align == "right":
            x = x - text_w(s, scale)
        x, y = int(x), int(y)
        if shadow:
            self._text(x + scale, y + scale, s, shadow, scale)
        self._text(x, y, s, col, scale)
        return x + len(s) * ADV * scale

    def _text(self, x, y, s, col, scale):
        colv = c(col)
        for i, ch in enumerate(s):
            g = GLYPHS.get(ch) or GLYPHS["?"]
            gx = x + i * ADV * scale
            for r, row in enumerate(g):
                for k, on in enumerate(row):
                    if on:
                        self.d.rectangle(
                            [gx + k * scale, y + r * scale,
                             gx + k * scale + scale - 1, y + r * scale + scale - 1],
                            fill=colv)

    def typed(self, x, y, s, n, col="white", scale=1, cursor=True, **kw):
        """Typewriter: draw the first n characters and a block cursor."""
        n = max(0, min(len(s), int(n)))
        end = self.text(x, y, s[:n], col, scale, **kw)
        if cursor and n < len(s):
            self.rect(end, y, 5 * scale, 7 * scale, col)

    def save(self, path, scale=2):
        out = self.img.resize((self.w * scale, self.h * scale), Image.NEAREST)
        out.save(path, optimize=True)

    def scaled(self, scale):
        return self.img.resize((self.w * scale, self.h * scale), Image.NEAREST)


BAYER4 = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]]


def c(col):
    return PAL[col] if isinstance(col, str) else col


def mix(a, b, t):
    a, b = c(a), c(b)
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def ease(t):
    t = max(0.0, min(1.0, t))
    return t * t * (3 - 2 * t)


def clamp01(t):
    return max(0.0, min(1.0, t))


# ---------------------------------------------------------------- components
def panel(cv, x, y, w, h, title=None, tone="green", fill="panel", title_col=None):
    """Bevelled pixel panel: dark outline, lit top-left edge, shaded bottom."""
    edge = {"green": ("outline", "shadow", "deep"),
            "amber": ("amber_d", "amber", "amber_d"),
            "red": ("red_d", "red", "red_d"),
            "blue": ("blue_d", "blue", "blue_d"),
            "violet": ("violet_d", "violet", "violet_d"),
            "grey": ("black", "line", "black")}[tone]
    cv.rect(x, y, w, h, fill)
    cv.frame(x, y, w, h, edge[0])
    cv.rect(x + 1, y + 1, w - 2, 1, edge[1])
    cv.rect(x + 1, y + 1, 1, h - 2, edge[1])
    cv.rect(x + 1, y + h - 2, w - 2, 1, edge[2])
    cv.rect(x + w - 2, y + 2, 1, h - 3, edge[2])
    if title:
        tw = text_w(title) + 8
        cv.rect(x + 6, y - 4, tw, 9, fill)
        cv.frame(x + 6, y - 4, tw, 9, edge[0])
        cv.text(x + 10, y - 3, title, title_col or edge[1], shadow=None)


def chip(cv, x, y, label, tone="green", fill=None, w=None, col=None):
    """Small label chip; returns its width."""
    w = w or text_w(label) + 8
    tones = {"green": ("deep", "light"), "amber": ("amber_d", "amber"),
             "red": ("red_d", "red"), "blue": ("blue_d", "blue"),
             "grey": ("line", "dim"), "violet": ("violet_d", "violet"),
             "pale": ("shadow", "white")}
    bgc, fg = tones[tone]
    cv.rect(x, y, w, 11, fill or bgc)
    cv.rect(x + 1, y + 10, w - 1, 1, "black")
    cv.text(x + (w - text_w(label)) // 2, y + 2, label, col or fg, shadow=None)
    return w


def db_cyl(cv, x, y, w=28, h=30, tone="main", label=None):
    """Pixel database cylinder."""
    body = {"main": ("deep", "main", "light", "outline"),
            "amber": ("amber_d", "amber", "white", "black"),
            "blue": ("blue_d", "blue", "white", "black"),
            "violet": ("violet_d", "violet", "white", "black")}[tone]
    dk, md, lt, ol = body
    cv.rect(x + 2, y + 3, w - 4, h - 5, md)
    cv.rect(x, y + 4, w, h - 7, md)
    cv.rect(x, y + 4, 1, h - 7, ol)
    cv.rect(x + w - 1, y + 4, 1, h - 7, ol)
    cv.rect(x + 2, y + h - 2, w - 4, 1, ol)
    cv.px(x + 1, y + h - 3, ol)
    cv.px(x + w - 2, y + h - 3, ol)
    cv.dither(x + w - 9, y + 6, 7, h - 9, dk, 0.5)
    cv.rect(x + 3, y + 7, 2, h - 12, lt)
    # top ellipse
    cv.rect(x + 2, y, w - 4, 1, ol)
    cv.rect(x + 1, y + 1, w - 2, 5, lt)
    cv.rect(x + 3, y + 2, w - 6, 3, dk)
    cv.px(x, y + 2, ol)
    cv.px(x, y + 3, ol)
    cv.px(x + w - 1, y + 2, ol)
    cv.px(x + w - 1, y + 3, ol)
    # band lines
    for by in (y + h // 2, y + h - 8):
        cv.rect(x + 1, by, w - 2, 1, dk)
    if label:
        cv.text(x + w // 2, y + h + 3, label, "pale", align="center")


def server(cv, x, y, units=4, t=0.0, w=56, label=None, busy=1.0):
    """Rack server with blinking LEDs."""
    uh = 14
    h = units * uh + 6
    panel(cv, x, y, w, h, tone="green", fill="panel2")
    for i in range(units):
        uy = y + 3 + i * uh
        cv.rect(x + 4, uy + 1, w - 8, uh - 3, "bg2")
        cv.rect(x + 4, uy + uh - 3, w - 8, 1, "outline")
        for k in range(0, w - 26, 3):
            cv.rect(x + 7 + k, uy + 4, 2, 5, "panel2")
        ph = (t * 3.3 + i * 0.37)
        on1 = (int(ph * 2) % 3) != 0 or busy >= 1.5
        on2 = int(ph * 5 + i) % 2 == 0 and busy > 0
        cv.rect(x + w - 16, uy + 5, 3, 3, "light" if on1 else "deep")
        cv.rect(x + w - 10, uy + 5, 3, 3, "amber" if on2 else "amber_d")
    if label:
        cv.text(x + w // 2, y + h + 4, label, "pale", align="center")
    return h


def laptop(cv, x, y, lid=1.0, screen="main", t=0.0, w=56, label=None):
    """Laptop; lid 1.0 = open, 0.0 = closed."""
    sh = int(34 * lid)
    if sh > 1:
        top = y + 34 - sh
        cv.rect(x + 4, top, w - 8, sh, "line")
        cv.frame(x + 4, top, w - 8, sh, "black")
        if sh > 6:
            cv.rect(x + 7, top + 3, w - 14, sh - 6, "bg2")
            # little lines of "terminal" text
            for i in range(max(0, (sh - 8) // 5)):
                ln = 6 + int(((i * 37 + int(t * 6)) % 23))
                cv.rect(x + 9, top + 5 + i * 5, min(ln, w - 20), 2, screen)
    cv.rect(x, y + 34, w, 5, "dim2")
    cv.rect(x, y + 34, w, 1, "dim")
    cv.rect(x + 1, y + 39, w - 2, 1, "black")
    cv.rect(x + w // 2 - 6, y + 35, 12, 1, "line")
    if label:
        cv.text(x + w // 2, y + 44, label, "pale", align="center")


def desktop(cv, x, y, t=0.0, w=56, label=None):
    cv.rect(x, y, w, 36, "line")
    cv.frame(x, y, w, 36, "black")
    cv.rect(x + 3, y + 3, w - 6, 30, "bg2")
    for i in range(5):
        ln = 8 + ((i * 29 + int(t * 5)) % 25)
        cv.rect(x + 6, y + 6 + i * 5, min(ln, w - 12), 2, "main")
    cv.rect(x + w // 2 - 3, y + 36, 6, 5, "dim2")
    cv.rect(x + w // 2 - 12, y + 41, 24, 2, "dim")
    if label:
        cv.text(x + w // 2, y + 47, label, "pale", align="center")


def phone(cv, x, y, t=0.0, label=None, lit=True):
    cv.rect(x, y, 22, 40, "line")
    cv.frame(x, y, 22, 40, "black")
    cv.rect(x + 2, y + 4, 18, 30, "bg2" if lit else "black")
    if lit:
        for i in range(5):
            ln = 4 + ((i * 7 + int(t * 4)) % 11)
            cv.rect(x + 4, y + 7 + i * 5, ln, 2, "light")
    cv.rect(x + 8, y + 36, 6, 1, "dim")
    if label:
        cv.text(x + 11, y + 44, label, "pale", align="center")


def lock(cv, x, y, col="amber"):
    cv.rect(x + 2, y, 5, 1, col)
    cv.rect(x + 1, y + 1, 1, 4, col)
    cv.rect(x + 7, y + 1, 1, 4, col)
    cv.rect(x, y + 4, 9, 7, col)
    cv.rect(x + 4, y + 6, 1, 3, "black")


def key_icon(cv, x, y, col="amber"):
    cv.rect(x, y, 5, 5, col)
    cv.rect(x + 1, y + 1, 3, 3, "black")
    cv.rect(x + 2, y + 2, 1, 1, col)
    cv.rect(x + 5, y + 2, 8, 1, col)
    cv.rect(x + 10, y + 3, 1, 2, col)
    cv.rect(x + 12, y + 3, 1, 2, col)


def envelope(cv, x, y, col="amber", w=16, h=11):
    cv.rect(x, y, w, h, col)
    cv.frame(x, y, w, h, "black")
    for i in range(w // 2):
        cv.px(x + i, y + min(i, h - 2) * h // (w // 2 + 1) + 1, "black")
        cv.px(x + w - 1 - i, y + min(i, h - 2) * h // (w // 2 + 1) + 1, "black")


def doc_icon(cv, x, y, col="pale", w=12, h=15, lines=4):
    cv.rect(x, y, w, h, col)
    cv.frame(x, y, w, h, "black")
    cv.rect(x + w - 4, y, 4, 4, "bg")
    cv.rect(x + w - 4, y + 3, 4, 1, "black")
    cv.rect(x + w - 4, y, 1, 4, "black")
    for i in range(lines):
        cv.rect(x + 2, y + 5 + i * 2 + (1 if i else 0), w - 5 - (i % 2) * 2, 1, "dim2")


def spinner(cv, x, y, t, col="light", r=5):
    """8-step pixel spinner (the L1 tick)."""
    pts = [(0, -r), (r - 1, -r + 1), (r, 0), (r - 1, r - 1), (0, r), (-r + 1, r - 1), (-r, 0), (-r + 1, -r + 1)]
    head = int(t * 8) % 8
    for i, (dx, dy) in enumerate(pts):
        age = (head - i) % 8
        colv = mix(col, "bg", min(1.0, age / 5))
        cv.rect(x + dx - 1, y + dy - 1, 2, 2, colv)


def arrow(cv, x0, y0, x1, y1, col="dim", dash=0, phase=0, head=True):
    """Orthogonal arrow: horizontal then vertical (or straight)."""
    if y0 == y1:
        cv.hline(x0, x1, y0, col, dash, phase)
        if head:
            d = 1 if x1 > x0 else -1
            for i in range(3):
                cv.vline(x1 - d * i, y1 - i, y1 + i, col)
    elif x0 == x1:
        cv.vline(x0, y0, y1, col, dash, phase)
        if head:
            d = 1 if y1 > y0 else -1
            for i in range(3):
                cv.hline(x1 - i, x1 + i, y1 - d * i, col)
    else:
        cv.hline(x0, x1, y0, col, dash, phase)
        arrow(cv, x1, y0, x1, y1, col, dash, phase, head)


def packet(cv, x0, y0, x1, y1, t, col="light", size=3):
    """A square that travels along a straight segment at progress t."""
    t = clamp01(t)
    x = x0 + (x1 - x0) * t
    y = y0 + (y1 - y0) * t
    cv.rect(int(x) - size // 2, int(y) - size // 2, size, size, col)
    cv.rect(int(x) - size // 2 + 1, int(y) - size // 2 + size, size, 1, "black")


def worker_chip(cv, x, y, label, tone="blue", t=0.0):
    """A worker: a tiny terminal glyph plus the harness name."""
    cv.rect(x, y, 10, 9, "black")
    cv.frame(x, y, 10, 9, tone)
    if int(t * 3) % 2 == 0:
        cv.rect(x + 2, y + 3, 3, 3, tone)
    cv.rect(x + 6, y + 5, 2, 1, tone)
    cv.text(x + 13, y + 1, label, tone, shadow=None)


STATE_TONE = {
    "pending": ("grey", "dim", "line"),
    "ready": ("amber", "amber", "amber_d"),
    "running": ("blue", "blue", "blue_d"),
    "gating": ("violet", "violet", "violet_d"),
    "reviewing": ("violet", "violet", "violet_d"),
    "fixing": ("red", "red", "red_d"),
    "failed": ("red", "red", "red_d"),
    "merging": ("green", "light", "deep"),
    "done": ("green", "light", "deep"),
    "parked": ("amber", "amber", "amber_d"),
    "decision": ("amber", "amber", "amber_d"),
}


def job_node(cv, x, y, kind, name, state, w=96, t=0.0, worker=None, sub=None, badge=None):
    """A job node in the graph: kind tag, slug, state bar."""
    tone, fg, dk = STATE_TONE[state]
    h = 34 if (worker or sub) else 26
    fill = "panel" if state != "pending" else "bg2"
    panel(cv, x, y, w, h, tone=tone if tone != "grey" else "grey", fill=fill)
    cv.rect(x + 3, y + 3, text_w(kind) + 4, 9, dk)
    cv.text(x + 5, y + 4, kind, fg, shadow=None)
    cv.text(x + 4, y + 15, name, "white" if state != "pending" else "dim", shadow=None)
    # state badge (right)
    lab = badge or state
    if state == "done" and not badge:
        lab = "done ✓"
    cv.text(x + w - 4, y + 4, lab, fg, shadow=None, align="right")
    if state == "running":
        # progress shimmer
        p = int((t * 30) % (w - 8))
        cv.rect(x + 4, y + h - 4, w - 8, 1, "blue_d")
        cv.rect(x + 4 + p, y + h - 4, 6, 1, "blue")
    if worker:
        worker_chip(cv, x + 4, y + 23, worker, "blue" if state == "running" else
                    ("violet" if state in ("reviewing", "gating") else
                     ("red" if state == "fixing" else "dim")), t)
    elif sub:
        cv.text(x + 4, y + 24, sub, "dim", shadow=None)
    return h

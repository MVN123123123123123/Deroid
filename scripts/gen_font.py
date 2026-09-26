#!/usr/bin/env python3
"""Generate the Rust glyph table for the UTLC vector stroke typography engine.

Design space: 1000 units/em, y-up, baseline y = 0, cap 700, x-height 520,
ascender 740, descender -200.  Every glyph is a set of sub-paths and every
sub-path is a chain of quadratic beziers (centreline skeleton).  font.rs
strokes the skeleton with a weight dependent pen radius and anti-aliases by
analytic point-to-curve distance, so type stays smooth at any size.
"""
import math

CAP = 700.0
XH = 520.0
ASC = 740.0
DESC = -200.0

G = {}


def f(v):
    s = "%.1f" % v
    return "0.0" if s == "-0.0" else s


class P:
    def __init__(self, x, y):
        self.sx = float(x)
        self.sy = float(y)
        self.x = float(x)
        self.y = float(y)
        self.segs = []

    def q(self, cx, cy, x, y):
        self.segs.append((float(cx), float(cy), float(x), float(y)))
        self.x, self.y = float(x), float(y)
        return self

    def l(self, x, y):
        x, y = float(x), float(y)
        return self.q((self.x + x) * 0.5, (self.y + y) * 0.5, x, y)

    def arc(self, cx, cy, rx, ry, a0, a1):
        span = a1 - a0
        n = max(1, int(math.ceil(abs(span) / 90.0 - 1e-9)))
        step = span / n
        k = 4.0 / 3.0 * math.tan(math.radians(step) / 4.0)
        r = math.radians(a0)
        sx = cx + rx * math.cos(r)
        sy = cy + ry * math.sin(r)
        if abs(sx - self.x) > 1.0 or abs(sy - self.y) > 1.0:
            self.l(sx, sy)
        for i in range(n):
            a = math.radians(a0 + step * i)
            b = math.radians(a0 + step * (i + 1))
            m = math.radians(a0 + step * (i + 0.5))
            x1, y1 = cx + rx * math.cos(a), cy + ry * math.sin(a)
            x2, y2 = cx + rx * math.cos(b), cy + ry * math.sin(b)
            xm, ym = cx + rx * math.cos(m), cy + ry * math.sin(m)
            # exact quadratic that interpolates the arc midpoint
            self.q(2.0 * xm - (x1 + x2) * 0.5, 2.0 * ym - (y1 + y2) * 0.5, x2, y2)
        return self

    def ring(self, cx, cy, rx, ry):
        return self.arc(cx, cy, rx, ry, 90, 450)

    def dot(self, x, y):
        return self.q(x, y, x, y)


def glyph(ch, adv, *paths):
    G[ch] = (adv, list(paths))


# ============================================================ punctuation
glyph(" ", 280)
glyph("!", 300, P(150, CAP).l(150, 200), P(150, 40).dot(150, 40))
glyph('"', 430, P(110, CAP).l(110, 430), P(310, CAP).l(310, 430))
glyph("#", 680, P(230, 0).l(300, CAP), P(120, 0).l(190, CAP),
      P(90, 250).l(410, 250), P(110, 470).l(430, 470))
glyph("$", 620, P(520, 545).q(400, 700, 300, 700).q(120, 700, 120, 540)
      .q(120, 380, 300, 330).q(480, 280, 480, 160).q(480, 0, 300, 0)
      .q(100, 0, 85, 150), P(300, 660).l(300, -80))
glyph("%", 780, P(120, 20).l(520, 700), P(200, 700).ring(200, 560, 130, 140),
      P(440, 285).ring(440, 145, 130, 140))
glyph("&", 780, P(345, 695).ring(345, 545, 170, 150),
      P(320, 395).ring(320, 200, 215, 195),
      P(105, 395).l(610, 700))
glyph("'", 240, P(120, CAP).l(120, 420))
glyph("(", 360, P(300, ASC).arc(300, 270, 180, 470, 90, 270))
glyph(")", 360, P(60, ASC).arc(60, 270, 180, 470, 90, -90))
glyph("*", 540, P(270, 630).l(270, 250), P(130, 550).l(410, 330), P(410, 550).l(130, 330))
glyph("+", 640, P(320, 520).l(320, 120), P(120, 320).l(520, 320))
glyph(",", 300, P(170, 190).l(70, -130))
glyph("-", 420, P(90, 300).l(330, 300))
glyph(".", 300, P(150, 0).dot(150, 0))
glyph("/", 480, P(40, -70).l(440, 770))
glyph(":", 300, P(150, 380).dot(150, 380), P(150, 60).dot(150, 60))
glyph(";", 300, P(160, 380).dot(160, 380), P(170, 190).l(70, -130))
glyph("<", 560, P(450, 530).l(120, 300).l(450, 70))
glyph("=", 640, P(110, 400).l(530, 400), P(110, 200).l(530, 200))
glyph(">", 560, P(110, 530).l(440, 300).l(110, 70))
glyph("?", 560, P(110, 540).arc(300, 548, 190, 152, 185, -40).l(300, 230), P(300, 40).dot(300, 40))
glyph("@", 840, P(611, 259).arc(400, 350, 225, 265, -20, 285),
      P(410, 460).ring(410, 340, 120, 120), P(530, 340).l(530, 200), P(530, 200).l(620, 200))

# ============================================================ digits
glyph("0", 620, P(310, CAP).ring(310, 350, 215, 350))
glyph("1", 620, P(120, 545).l(300, CAP).l(300, 0))
glyph("2", 620, P(111, 537).arc(310, 550, 200, 150, 185, -30).l(110, 0).l(540, 0))
glyph("3", 620, P(113, 564).arc(300, 535, 190, 165, 170, -70).arc(300, 180, 215, 180, 70, -170))
glyph("4", 620, P(430, 0).l(430, CAP).l(70, 205).l(560, 205))
glyph("5", 620, P(520, CAP).l(150, CAP).l(130, 430).q(150, 470, 300, 470)
      .arc(300, 245, 240, 225, 90, -140))
glyph("6", 620, P(310, 500).ring(310, 250, 215, 250), P(110, 520).arc(310, 520, 200, 180, 180, 45))
glyph("7", 620, P(95, CAP).l(545, CAP).l(250, 0))
glyph("8", 620, P(310, 700).ring(310, 528, 185, 172), P(310, 356).ring(310, 178, 220, 178))
glyph("9", 620, P(310, 700).ring(310, 450, 215, 250), P(525, 380).q(530, 120, 320, 60))

# ============================================================ uppercase
glyph("A", 640, P(60, 0).l(320, CAP).l(580, 0), P(152, 200).l(488, 200))
glyph("B", 620, P(120, 0).l(120, CAP).l(330, CAP).arc(330, 540, 160, 160, 90, -90).l(120, 380)
      .l(350, 380).arc(350, 190, 190, 190, 90, -90).l(120, 0))
glyph("C", 640, P(492, 575).arc(320, 350, 225, 350, 40, 320))
glyph("D", 660, P(120, 0).l(120, CAP).l(300, CAP).arc(300, 350, 260, 350, 90, -90).l(120, 0))
glyph("E", 580, P(520, CAP).l(120, CAP).l(120, 0).l(520, 0), P(120, 355).l(455, 355))
glyph("F", 540, P(520, CAP).l(120, CAP).l(120, 0), P(120, 355).l(450, 355))
glyph("G", 680, P(479, 597).arc(320, 350, 225, 350, 45, 360).l(400, 350))
glyph("H", 640, P(120, CAP).l(120, 0), P(520, CAP).l(520, 0), P(120, 355).l(520, 355))
glyph("I", 300, P(150, CAP).l(150, 0))
glyph("J", 500, P(430, CAP).l(430, 170).arc(250, 170, 180, 170, 0, -180))
glyph("K", 620, P(120, CAP).l(120, 0), P(545, CAP).l(120, 300), P(255, 410).l(565, 0))
glyph("L", 560, P(120, CAP).l(120, 0).l(520, 0))
glyph("M", 760, P(110, 0).l(110, CAP).l(380, 200).l(650, CAP).l(650, 0))
glyph("N", 640, P(120, 0).l(120, CAP).l(520, 170).l(520, CAP))
glyph("O", 680, P(340, CAP).ring(340, 350, 240, 350))
glyph("P", 600, P(120, 0).l(120, CAP).l(330, CAP).arc(330, 520, 185, 180, 90, -90).l(120, 340))
glyph("Q", 700, P(340, CAP).ring(340, 350, 240, 350), P(430, 170).l(600, -40))
glyph("R", 630, P(120, 0).l(120, CAP).l(330, CAP).arc(330, 520, 185, 180, 90, -90).l(120, 340)
      .l(380, 340).l(560, 0))
glyph("S", 600, P(520, 545).q(400, 700, 300, 700).q(120, 700, 120, 540)
      .q(120, 380, 300, 330).q(480, 280, 480, 160).q(480, 0, 300, 0)
      .q(100, 0, 85, 150))
glyph("T", 600, P(60, CAP).l(540, CAP), P(300, CAP).l(300, 0))
glyph("U", 640, P(110, CAP).l(110, 210).arc(320, 210, 210, 210, 180, 360).l(530, CAP))
glyph("V", 640, P(60, CAP).l(320, 0).l(580, CAP))
glyph("W", 860, P(60, CAP).l(255, 0).l(420, 480).l(585, 0).l(780, CAP))
glyph("X", 630, P(80, CAP).l(550, 0), P(550, CAP).l(80, 0))
glyph("Y", 630, P(70, CAP).l(315, 350).l(560, CAP), P(315, 350).l(315, 0))
glyph("Z", 600, P(90, CAP).l(520, CAP).l(90, 0).l(520, 0))

# ============================================================ lowercase
glyph("a", 580, P(470, XH).l(470, 0).arc(275, 258, 195, 270, -90, -320))
glyph("b", 580, P(110, ASC).l(110, 0), P(110, 258).arc(275, 258, 175, 270, 180, 540))
glyph("c", 540, P(408, 449).arc(270, 258, 195, 270, 45, 315))
glyph("d", 580, P(450, ASC).l(450, 0), P(450, 258).arc(285, 258, 175, 270, 0, -360))
glyph("e", 560, P(85, 245).l(465, 245).arc(275, 258, 195, 270, 0, 315))
glyph("f", 400, P(130, 0).l(130, 600).arc(300, 600, 170, 140, 180, 80), P(60, XH).l(330, XH))
glyph("g", 580, P(450, XH).l(450, -200).arc(300, -200, 150, 140, 0, 90),
      P(450, 258).arc(275, 258, 175, 270, 0, -360))
glyph("h", 580, P(110, ASC).l(110, 0), P(110, 400).arc(280, 400, 170, 120, 180, 0).l(450, 0))
glyph("i", 300, P(150, XH).l(150, 0), P(150, 700).dot(150, 700))
glyph("j", 340, P(210, XH).l(210, -200).arc(75, -200, 135, 140, 0, 90), P(210, 700).dot(210, 700))
glyph("k", 540, P(110, ASC).l(110, 0), P(450, XH).l(110, 200), P(230, 300).l(470, 0))
glyph("l", 300, P(150, ASC).l(150, 0))
glyph("m", 860, P(110, XH).l(110, 0), P(110, 400).arc(265, 400, 155, 120, 180, 0).l(420, 0),
      P(420, 400).arc(575, 400, 155, 120, 180, 0).l(730, 0))
glyph("n", 580, P(110, XH).l(110, 0), P(110, 400).arc(280, 400, 170, 120, 180, 0).l(450, 0))
glyph("o", 560, P(280, XH).ring(280, 258, 205, 270))
glyph("p", 580, P(110, XH).l(110, DESC), P(110, 258).arc(275, 258, 175, 270, 180, 540))
glyph("q", 580, P(450, XH).l(450, DESC), P(450, 258).arc(285, 258, 175, 270, 0, -360))
glyph("r", 440, P(110, XH).l(110, 0), P(110, 400).arc(270, 400, 160, 120, 180, 40))
glyph("s", 480, P(405, 425).q(310, 545, 240, 545).q(105, 545, 105, 420)
      .q(105, 300, 240, 265).q(385, 230, 385, 140).q(385, 0, 240, 0)
      .q(105, 0, 80, 130))
glyph("t", 420, P(215, 660).l(215, 120).arc(330, 120, 115, 120, 180, 275), P(65, XH).l(360, XH))
glyph("u", 580, P(110, XH).l(110, 130).arc(280, 130, 170, 130, 180, 360).l(450, XH).l(450, 0))
glyph("v", 540, P(60, XH).l(270, 0).l(480, XH))
glyph("w", 760, P(50, XH).l(225, 0).l(370, 390).l(515, 0).l(690, XH))
glyph("x", 540, P(70, XH).l(470, 0), P(470, XH).l(70, 0))
glyph("y", 540, P(60, XH).l(280, 165), P(60, XH).l(500, -190))
glyph("z", 520, P(80, XH).l(440, XH).l(80, 0).l(440, 0))

# ============================================================ brackets
glyph("[", 360, P(300, ASC).l(120, ASC).l(120, DESC).l(300, DESC))
glyph("\\", 480, P(40, 770).l(440, -70))
glyph("]", 360, P(60, ASC).l(240, ASC).l(240, DESC).l(60, DESC))
glyph("^", 560, P(110, 420).l(280, CAP).l(450, 420))
glyph("_", 560, P(40, -110).l(520, -110))
glyph("`", 280, P(80, CAP).l(210, 620))
glyph("{", 400, P(330, ASC).l(190, ASC).l(190, 400).l(90, 300).l(190, 200).l(190, DESC).l(330, DESC))
glyph("|", 300, P(150, ASC).l(150, DESC))
glyph("}", 400, P(70, ASC).l(210, ASC).l(210, 400).l(310, 300).l(210, 200).l(210, DESC).l(70, DESC))
glyph("~", 620, P(70, 300).arc(200, 300, 130, 70, 180, 360).arc(460, 300, 130, 70, 180, 0))
glyph("\x7f", 600, P(140, 140).l(460, 560), P(460, 140).l(140, 560))

# ============================================================ emit rust
order = [chr(c) for c in range(0x20, 0x80)]
missing = [c for c in order if c not in G]
assert not missing, missing
lines = [
    "// @generated by scripts/gen_font.py - 1000 units/em, y-up, baseline at y = 0.",
    "static GLYPHS: [Glyph; 96] = [",
]
for ch in order:
    adv, paths = G[ch]
    disp = {" ": "space", "\x7f": "DEL", "\\": "backslash"}.get(ch, ch)
    lines.append("    // %s" % disp)
    if not paths:
        lines.append("    Glyph { a: %s, sub: &[] }," % f(adv))
        continue
    parts = []
    for p in paths:
        body = ", ".join("Q(%s, %s, %s, %s)" % (f(a), f(b), f(c), f(d)) for a, b, c, d in p.segs)
        parts.append("Sub { s: (%s, %s), p: &[%s] }" % (f(p.sx), f(p.sy), body))
    lines.append("    Glyph { a: %s, sub: &[%s] }," % (f(adv), ", ".join(parts)))
lines.append("];")
open("/tmp/opencode/glyphs.rs", "w").write("\n".join(lines) + "\n")
print("glyphs:", len(order), "bytes:", sum(len(l) for l in lines))

"""生成 app 图标（多尺寸 .ico + 各尺寸 png），并支持出候选图。

为什么用脚本生成：
  1. 可复现 —— 任何时候运行都得到同样的图标；
  2. 没有版权顾虑（自己画的）；
  3. 尺寸齐全 —— Windows 在 16/20/24/32/40/48/64/128/256 不同位置用不同尺寸，
     只给一张大图让系统缩，任务栏和标题栏上会糊。

设计语言（按"拼色"方向迭代）：
  - 圆角方块底（近似 Apple squircle）
  - 图形用两种颜色拼：暖色（琥珀 / 橙）负责"实况照片"的同心圆记号，
    冷色（青 / 蓝）负责"会播放"的三角
  - 底色分四套：蓝青渐变、深墨蓝、纯白、对角双色

变体（--variant）：
  badge      蓝青渐变底 + 白色三角 + 角上白色同心圆（第一版正式方案）
  ring       蓝青渐变底 + 圆环套三角
  frame      蓝青渐变底 + 相框内三角
  live       蓝青渐变底 + 同心圆为主体 + 角上小三角
  duo-dark   深墨蓝底 + 琥珀同心圆 + 青色三角（拼色）
  duo-light  白底 + 琥珀同心圆 + 青色三角（拼色，iOS 明亮风格）
  duo-split  对角双色底（琥珀 / 青）+ 白色三角与圆环（拼色）
  duo-tile   蓝青渐变底 + 琥珀三角 + 青色同心圆（拼色）

用法：
  python tools/gen-icon.py                              # 写出正式图标（默认变体）
  python tools/gen-icon.py --variant duo-dark            # 换变体
  python tools/gen-icon.py --sheet out.png               # 出候选对比图
"""
from __future__ import annotations

import argparse
import math
import sys
from pathlib import Path

try:
    import numpy as np
    from PIL import Image, ImageChops, ImageDraw, ImageFilter
except ImportError:  # pragma: no cover
    print("需要 Pillow 与 numpy：pip install pillow numpy", file=sys.stderr)
    raise

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "app" / "src-tauri" / "icons"

S = 1024                 # 逻辑画布
SS = 2                   # 超采样倍数
C = S * SS

BLUE = (46, 124, 246)    # #2E7CF6
CYAN = (20, 194, 206)    # #14C2CE
AMBER = (255, 176, 48)   # #FFB030
ORANGE = (255, 138, 51)  # #FF8A33
INK = (18, 26, 44)       # #121A2C
TEAL = (34, 211, 238)    # #22D3EE
WHITE = (255, 255, 255)
RADIUS = int(0.235 * C)

# 每种变体：底(background) + 各层图形(带颜色)
SPECS: dict[str, dict] = {
    "badge": dict(bg="gradient", layers=[("badge", WHITE), ("tri", WHITE)]),
    "ring": dict(bg="gradient", layers=[("ring", WHITE), ("tri", WHITE)]),
    "frame": dict(bg="gradient", layers=[("frame", WHITE), ("tri_in_frame", WHITE)]),
    "live": dict(bg="gradient", layers=[("live_mark", WHITE), ("tri_corner", WHITE)]),
    # ---- 拼色方案 ----
    "duo-dark": dict(bg="ink", layers=[("badge", AMBER), ("tri", TEAL)]),
    "duo-light": dict(bg="white", layers=[("badge", ORANGE), ("tri", BLUE)]),
    "duo-split": dict(bg="split", layers=[("ring", WHITE), ("tri", WHITE)]),
    "duo-tile": dict(bg="gradient", layers=[("badge", TEAL), ("tri", AMBER)]),
}
DEFAULT_VARIANT = "duo-dark"


# ---------------------------------------------------------------- 底色
def _lerp(a: tuple[int, int, int], b: tuple[int, int, int], t: np.ndarray) -> Image.Image:
    t = t[:, :, None]
    va = np.array(a, dtype=np.float32)[None, None, :]
    vb = np.array(b, dtype=np.float32)[None, None, :]
    return Image.fromarray((va * (1.0 - t) + vb * t).round().astype(np.uint8), "RGB")


def background(kind: str) -> Image.Image:
    y, x = np.mgrid[0:C, 0:C].astype(np.float32)
    diag = (x + y) / (2.0 * (C - 1))
    if kind == "gradient":
        bg = _lerp(BLUE, CYAN, diag)
    elif kind == "ink":
        bg = _lerp((26, 36, 58), INK, diag)
    elif kind == "white":
        bg = _lerp((255, 255, 255), (238, 242, 248), diag)
    elif kind == "split":
        # 对角双色：左上暖色带 -> 右下冷色带，中间一条硬分界
        left = _lerp(ORANGE, AMBER, diag)
        right = _lerp(TEAL, BLUE, diag)
        step = (np.clip((diag - 0.46) / 0.08, 0.0, 1.0))[:, :, None]
        arr = np.array(left, dtype=np.float32) * (1 - step) + np.array(right, dtype=np.float32) * step
        bg = Image.fromarray(arr.round().astype(np.uint8), "RGB")
    else:
        raise ValueError(kind)

    # 顶部柔光：浅色底不加，深色/彩色底加一点，避免整块太平
    if kind != "white":
        glow = Image.new("L", (C, C), 0)
        ImageDraw.Draw(glow).ellipse([-0.25 * C, -0.70 * C, 1.25 * C, 0.50 * C], fill=255)
        glow = glow.filter(ImageFilter.GaussianBlur(0.12 * C)).point(lambda v: v // 6)
        bg = Image.composite(Image.new("RGB", (C, C), WHITE), bg, glow)
    return bg


def squircle_mask() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, C - 1, C - 1], radius=RADIUS, fill=255)
    return m


# ---------------------------------------------------------------- 图形
def blur_round(mask: Image.Image, sigma: float) -> Image.Image:
    """把尖锐边角磨圆：模糊后阈值化。"""
    return mask.filter(ImageFilter.GaussianBlur(sigma)).point(lambda v: 255 if v >= 128 else 0)


def triangle(cx: int, cy: int, w: float, h: float) -> Image.Image:
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    w, h = int(w * C), int(h * C)
    left, top = cx - w // 2, cy - h // 2
    grow = int(0.012 * C)
    d.polygon(
        [(left - grow, top - grow), (left + w + grow, cy), (left - grow, top + h + grow)], fill=255
    )
    return blur_round(layer, 0.022 * C)


def rings(cx: int, cy: int, radii: list[tuple[float, float]], dot: float = 0.0) -> Image.Image:
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    for r, w in radii:
        r, w = int(r * C), int(w * C)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=255, width=w)
    if dot > 0:
        rr = int(dot * C)
        d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=255)
    return layer


def rounded_rect(cx: int, cy: int, w: float, h: float, stroke: float, radius: float) -> Image.Image:
    layer = Image.new("L", (C, C), 0)
    w, h, stroke, radius = int(w * C), int(h * C), int(stroke * C), int(radius * C)
    ImageDraw.Draw(layer).rounded_rectangle(
        [cx - w // 2, cy - h // 2, cx + w // 2, cy + h // 2], radius=radius, outline=255, width=stroke
    )
    return layer


# 各层图形（返回 L 掩膜，颜色在合成时给）
def layer_mask(name: str) -> Image.Image:
    cx, cy = C // 2, C // 2
    if name == "tri":
        return triangle(cx + int(0.020 * C), cy, 0.40, 0.46)
    if name == "tri_in_frame":
        return triangle(cx + int(0.012 * C), cy, 0.22, 0.26)
    if name == "tri_corner":
        return triangle(int(0.70 * C), int(0.72 * C), 0.16, 0.19)
    if name == "badge":
        return rings(int(0.255 * C), int(0.255 * C), [(0.088, 0.020), (0.050, 0.020)], dot=0.018)
    if name == "live_mark":
        return rings(cx, cy, [(0.30, 0.034), (0.19, 0.034)], dot=0.085)
    if name == "frame":
        return rounded_rect(cx, cy, 0.62, 0.50, 0.045, 0.09)
    if name == "ring":
        ring = rings(cx, cy, [(0.385, 0.028)])
        cut = Image.new("L", (C, C), 0)
        ImageDraw.Draw(cut).ellipse(
            [int(0.30 * C), int(-0.18 * C), int(0.78 * C), int(0.22 * C)], fill=255
        )
        return ImageChops.subtract(ring, cut)
    raise ValueError(name)


# ---------------------------------------------------------------- 复古方案
# 说明：这批图标不走"渐变底 + 图形"的现代写法，而是各自是一整套视觉语言
# （宝丽来相纸、35mm 胶片、CRT 电视、老镜头、8-bit 像素、合成波落日、包豪斯）。
# 它们自己画完整张图，再由 _finish() 统一切圆角。

CREAM = (247, 243, 234)
SEPIA = (226, 120, 60)
AMBERB = (255, 190, 90)
INKBLACK = (20, 18, 17)


def _finish(body: Image.Image) -> Image.Image:
    """把一张 RGB 画面按 squircle 裁切，输出带透明通道的图标。"""
    out = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    out.paste(body, (0, 0), squircle_mask())
    return out


def _vgrad(box: tuple[int, int, int, int], a, b) -> Image.Image:
    """竖直渐变块。"""
    l, t, r, bo = box
    h = max(1, bo - t)
    t_arr = (np.arange(h, dtype=np.float32) / max(1, h - 1))[:, None].repeat(r - l, axis=1)
    img = _lerp(a, b, t_arr)
    return img


def _tri_points(cx: int, cy: int, w: float, h: float) -> list[tuple[int, int]]:
    w, h = int(w * C), int(h * C)
    return [(cx - w // 2, cy - h // 2), (cx + w // 2, cy), (cx - w // 2, cy + h // 2)]


def retro_polaroid() -> Image.Image:
    """宝丽来：相纸白边、暖色影像、播放三角嵌在画面里。"""
    body = Image.new("RGB", (C, C), CREAM)
    d = ImageDraw.Draw(body)
    l, t, r, b = int(0.10 * C), int(0.10 * C), int(0.90 * C), int(0.74 * C)
    photo = _vgrad((l, t, r, b), (255, 176, 48), (236, 72, 153))
    body.paste(photo, (l, t))
    d.rectangle([l, t, r, b], outline=(205, 195, 178), width=int(0.005 * C))
    # 相纸下缘略暗，像老纸
    d.rectangle([int(0.10 * C), int(0.74 * C), int(0.90 * C), int(0.76 * C)], fill=(232, 224, 210))
    d.polygon(_tri_points(C // 2 + int(0.01 * C), (t + b) // 2, 0.26, 0.30), fill=WHITE)
    return _finish(body)


def retro_film() -> Image.Image:
    """35mm 胶片：齿孔、暖褐影像、奶油色播放三角。"""
    body = Image.new("RGB", (C, C), INKBLACK)
    d = ImageDraw.Draw(body)
    l, t, r, b = int(0.20 * C), int(0.16 * C), int(0.80 * C), int(0.84 * C)
    frame = _vgrad((l, t, r, b), AMBERB, SEPIA)
    body.paste(frame, (l, t))
    d.rectangle([l, t, r, b], outline=(120, 96, 70), width=int(0.006 * C))
    for i in range(5):
        y = t + (b - t) * (i + 0.5) / 5
        for x0 in (int(0.045 * C), int(0.865 * C)):
            d.rounded_rectangle(
                [x0, int(y - 0.035 * C), x0 + int(0.09 * C), int(y + 0.035 * C)],
                radius=int(0.012 * C),
                fill=(238, 230, 214),
            )
    d.polygon(_tri_points(C // 2 + int(0.01 * C), C // 2, 0.22, 0.26), fill=(255, 250, 240))
    return _finish(body)


def retro_crt() -> Image.Image:
    """CRT 电视：米色机身、墨绿屏幕、扫描线、右侧旋钮。"""
    body = Image.new("RGB", (C, C), (216, 199, 168))
    d = ImageDraw.Draw(body)
    d.rounded_rectangle(
        [int(0.06 * C), int(0.06 * C), int(0.94 * C), int(0.94 * C)],
        radius=int(0.12 * C),
        fill=(206, 188, 155),
        outline=(150, 130, 104),
        width=int(0.012 * C),
    )
    sl, st, sr, sb = int(0.12 * C), int(0.14 * C), int(0.86 * C), int(0.78 * C)
    screen = _vgrad((sl, st, sr, sb), (16, 46, 34), (10, 24, 20))
    body.paste(screen, (sl, st))
    # 扫描线
    for y in range(st, sb, int(0.022 * C)):
        d.line([(sl, y), (sr, y)], fill=(28, 70, 52), width=max(1, int(0.004 * C)))
    d.rounded_rectangle(
        [sl, st, sr, sb], radius=int(0.03 * C), outline=(58, 48, 38), width=int(0.010 * C)
    )
    d.polygon(_tri_points(C // 2 + int(0.01 * C), (st + sb) // 2, 0.22, 0.26), fill=(198, 255, 176))
    for i, cy in enumerate((0.86, 0.90)):
        d.ellipse(
            [int(0.74 * C), int(cy * C), int(0.82 * C), int((cy + 0.035) * C)],
            fill=(140, 122, 96),
        )
    return _finish(body)


def retro_lens() -> Image.Image:
    """老镜头：奶油上盖、皮革机身、多层镀膜镜头（镜头中心就是播放三角）。"""
    body = Image.new("RGB", (C, C), (42, 35, 32))
    d = ImageDraw.Draw(body)
    d.rectangle([0, 0, C, int(0.30 * C)], fill=(232, 224, 206))
    d.rectangle([0, int(0.28 * C), C, int(0.32 * C)], fill=(196, 186, 166))
    cx, cy = C // 2, int(0.56 * C)
    for rr, col in (
        (0.30, (232, 224, 206)),
        (0.26, (28, 24, 22)),
        (0.21, (58, 48, 44)),
        (0.16, (18, 16, 15)),
    ):
        r = int(rr * C)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=col)
    glass = Image.new("RGB", (C, C), (0, 0, 0))
    gd = ImageDraw.Draw(glass)
    r = int(0.14 * C)
    gd.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(40, 90, 150))
    glass = glass.filter(ImageFilter.GaussianBlur(int(0.02 * C)))
    body = Image.composite(glass, body, Image.new("L", (C, C), 0).point(lambda v: 0))
    bm = Image.new("L", (C, C), 0)
    ImageDraw.Draw(bm).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    body = Image.composite(glass, body, bm)
    d = ImageDraw.Draw(body)
    d.ellipse(
        [cx - int(0.05 * C), cy - int(0.09 * C), cx + int(0.03 * C), cy - int(0.01 * C)],
        fill=(150, 200, 240),
    )
    d.polygon(_tri_points(cx + int(0.01 * C), cy, 0.14, 0.17), fill=(255, 255, 255))
    return _finish(body)


def retro_pixel() -> Image.Image:
    """8-bit：16×16 网格上逐行填出像素三角，最近邻放大，硬边、无抗锯齿。"""
    grid = 16
    pix = Image.new("RGB", (grid, grid), (12, 12, 16))
    d = ImageDraw.Draw(pix)
    # 像素画框
    d.rectangle([1, 1, 14, 14], outline=(238, 234, 222))
    # 挖掉四角，强化方块感
    for cx, cy in ((1, 1), (14, 1), (1, 14), (14, 14)):
        pix.putpixel((cx, cy), (12, 12, 16))
    # 右向三角：顶点 (12,7)，底边 x=4，y 从 2 到 12
    for y in range(2, 13):
        depth = 1.0 - abs(y - 7) / 5.0
        x_max = 4 + int(round(8 * depth))
        for x in range(4, x_max + 1):
            pix.putpixel((x, y), (255, 148, 56))
    big = pix.resize((C, C), Image.NEAREST)
    return _finish(big)


def retro_sunset() -> Image.Image:
    """合成波：紫粉天空、条纹落日、青色网格地面。"""
    body = Image.new("RGB", (C, C), (40, 10, 70))
    t_arr = (np.arange(C, dtype=np.float32) / (C - 1))[:, None].repeat(C, axis=1)
    body = _lerp((42, 12, 78), (255, 110, 150), t_arr)
    d = ImageDraw.Draw(body)
    cx, cy, r = C // 2, int(0.52 * C), int(0.24 * C)
    sun = Image.new("L", (C, C), 0)
    ImageDraw.Draw(sun).ellipse([cx - r, cy - r, cx + r, cy + r], fill=255)
    band = Image.new("L", (C, C), 255)
    bd = ImageDraw.Draw(band)
    for i in range(6):
        y = cy + int((0.02 + i * 0.05) * C)
        bd.rectangle([cx - r, y, cx + r, y + int(0.012 * C)], fill=0)
    body = Image.composite(Image.new("RGB", (C, C), (255, 205, 110)), body, ImageChops.multiply(sun, band))
    d = ImageDraw.Draw(body)
    # 地面网格
    horizon = int(0.70 * C)
    d.rectangle([0, horizon, C, C], fill=(20, 8, 38))
    for i in range(-6, 7):
        x_bottom = C // 2 + i * int(0.22 * C)
        d.line([(C // 2, horizon), (x_bottom, C)], fill=(34, 211, 238), width=int(0.006 * C))
    for k in range(1, 5):
        y = horizon + int((C - horizon) * (k / 5.0) ** 1.8)
        d.line([(0, y), (C, y)], fill=(34, 211, 238), width=int(0.006 * C))
    d.polygon(_tri_points(cx + int(0.01 * C), cy, 0.16, 0.19), fill=(26, 8, 40))
    return _finish(body)


def retro_bauhaus() -> Image.Image:
    """包豪斯：奶油底、红圆、蓝三角、黑色横条，纯几何。"""
    body = Image.new("RGB", (C, C), (240, 233, 216))
    d = ImageDraw.Draw(body)
    r = int(0.26 * C)
    d.ellipse([int(0.10 * C), int(0.10 * C), int(0.10 * C) + 2 * r, int(0.10 * C) + 2 * r], fill=(214, 60, 46))
    d.polygon(
        [(int(0.94 * C), int(0.18 * C)), (int(0.94 * C), int(0.74 * C)), (int(0.44 * C), int(0.46 * C))],
        fill=(28, 62, 120),
    )
    d.rectangle([int(0.08 * C), int(0.82 * C), int(0.92 * C), int(0.88 * C)], fill=(24, 22, 20))
    return _finish(body)


# ---------------------------------------------------------------- 包豪斯 / 美式
# 包豪斯的关键不是"奶油底 + 圆 + 三角"，而是三原色（红黄蓝）加黑白、
# 只用基本几何体、构图靠对角与重心；这里四款分别试了：三原色组合、
# 黑底对角色带、三同心圆、网格构成。
# 美式大胆配色走的是另一条线：Saul Bass 的剪纸色块、星条旗抽象、
# 1950s 餐馆（薄荷绿 + 红 + 黑白格）、波普艺术（黄底 + 黑描边 + Ben-Day 网点）。

BH_CREAM = (243, 236, 222)
BH_RED = (214, 60, 46)
BH_YELLOW = (242, 184, 38)
BH_BLUE = (28, 62, 120)
BH_BLACK = (26, 23, 20)
US_CREAM = (250, 244, 230)
US_BLUE = (26, 54, 122)
US_RED = (206, 44, 44)
US_TEAL = (46, 160, 152)
POP_YELLOW = (255, 208, 40)


def bh_primitives() -> Image.Image:
    """包豪斯一：三原色各占一个基本形 —— 红圆、黄三角、蓝横条。"""
    body = Image.new("RGB", (C, C), BH_CREAM)
    d = ImageDraw.Draw(body)
    r = int(0.23 * C)
    d.ellipse([int(0.12 * C), int(0.14 * C), int(0.12 * C) + 2 * r, int(0.14 * C) + 2 * r], fill=BH_RED)
    d.polygon(_tri_points(int(0.70 * C), int(0.42 * C), 0.34, 0.42), fill=BH_YELLOW)
    d.rectangle([int(0.10 * C), int(0.76 * C), int(0.90 * C), int(0.88 * C)], fill=BH_BLUE)
    return _finish(body)


def bh_diagonal() -> Image.Image:
    """包豪斯二：黑底 + 三原色对角色带 + 白色播放三角。"""
    body = Image.new("RGB", (C, C), BH_BLACK)
    d = ImageDraw.Draw(body)
    # 三条 45° 色带，用平行四边形模拟
    for i, col in enumerate((BH_RED, BH_YELLOW, BH_BLUE)):
        off = int((0.06 + i * 0.12) * C)
        w = int(0.085 * C)
        d.polygon(
            [(off, C), (off + w, C), (off + w + int(0.55 * C), int(0.30 * C)), (off + int(0.55 * C), int(0.30 * C))],
            fill=col,
        )
    d.polygon(_tri_points(int(0.60 * C), int(0.40 * C), 0.30, 0.36), fill=(250, 246, 236))
    return _finish(body)


def bh_rings() -> Image.Image:
    """包豪斯三：红/黄/蓝三同心圆，最内层放黑色播放三角。"""
    body = Image.new("RGB", (C, C), BH_CREAM)
    d = ImageDraw.Draw(body)
    cx, cy = C // 2, C // 2
    for rr, col in ((0.38, BH_RED), (0.28, BH_YELLOW), (0.18, BH_BLUE)):
        r = int(rr * C)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=col)
    d.polygon(_tri_points(cx + int(0.01 * C), cy, 0.15, 0.18), fill=BH_BLACK)
    return _finish(body)


def bh_grid() -> Image.Image:
    """包豪斯四：网格构成 —— 红方块、蓝圆、黄三角，压黑细线。"""
    body = Image.new("RGB", (C, C), BH_CREAM)
    d = ImageDraw.Draw(body)
    step = int(0.125 * C)
    for i in range(1, 8):
        d.line([(i * step, 0), (i * step, C)], fill=(226, 218, 202), width=max(1, int(0.004 * C)))
        d.line([(0, i * step), (C, i * step)], fill=(226, 218, 202), width=max(1, int(0.004 * C)))
    d.rectangle([int(0.10 * C), int(0.10 * C), int(0.44 * C), int(0.44 * C)], fill=BH_RED)
    r = int(0.17 * C)
    d.ellipse([int(0.62 * C) - r, int(0.70 * C) - r, int(0.62 * C) + r, int(0.70 * C) + r], fill=BH_BLUE)
    d.polygon(_tri_points(int(0.68 * C), int(0.30 * C), 0.26, 0.32), fill=BH_YELLOW)
    return _finish(body)


def am_bass() -> Image.Image:
    """美式一：Saul Bass 剪纸感 —— 奶油底、橙色大圆、黑色剪刀三角、上下黑条。"""
    body = Image.new("RGB", (C, C), US_CREAM)
    d = ImageDraw.Draw(body)
    r = int(0.30 * C)
    d.ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=(232, 106, 46))
    d.polygon(_tri_points(C // 2 + int(0.03 * C), C // 2, 0.24, 0.28), fill=(24, 22, 20))
    d.rectangle([0, 0, C, int(0.10 * C)], fill=(24, 22, 20))
    d.rectangle([0, int(0.90 * C), C, C], fill=(24, 22, 20))
    return _finish(body)


def am_flag() -> Image.Image:
    """美式二：星条旗抽象 —— 深蓝底、白色三角、底部红白条纹。"""
    body = Image.new("RGB", (C, C), US_BLUE)
    d = ImageDraw.Draw(body)
    for i in range(3):
        y = int((0.76 + i * 0.08) * C)
        d.rectangle([0, y, C, y + int(0.045 * C)], fill=US_RED)
    d.polygon(_tri_points(int(0.50 * C), int(0.44 * C), 0.36, 0.42), fill=US_CREAM)
    return _finish(body)


def am_diner() -> Image.Image:
    """美式三：1950s 餐馆 —— 薄荷绿配红、底部黑白棋盘格。"""
    body = Image.new("RGB", (C, C), (238, 246, 238))
    d = ImageDraw.Draw(body)
    d.rectangle([0, 0, int(0.62 * C), C], fill=US_TEAL)
    r = int(0.20 * C)
    d.ellipse([int(0.40 * C) - r, int(0.34 * C) - r, int(0.40 * C) + r, int(0.34 * C) + r], fill=US_RED)
    d.polygon(_tri_points(int(0.40 * C), int(0.34 * C), 0.16, 0.19), fill=(255, 255, 255))
    sq = int(0.10 * C)
    for row in range(2):
        for i in range(10):
            if (i + row) % 2 == 0:
                d.rectangle([i * sq, int(0.80 * C) + row * sq, (i + 1) * sq, int(0.80 * C) + (row + 1) * sq],
                            fill=(26, 24, 22))
    return _finish(body)


def am_pop() -> Image.Image:
    """美式四：波普 —— 亮黄底、粗黑描边三角（红蓝各半）、Ben-Day 网点。"""
    body = Image.new("RGB", (C, C), POP_YELLOW)
    d = ImageDraw.Draw(body)
    # 网点（右下角一块）
    dot = int(0.022 * C)
    for y in range(int(0.55 * C), C, int(0.075 * C)):
        for x in range(int(0.55 * C), C, int(0.075 * C)):
            d.ellipse([x, y, x + dot, y + dot], fill=(255, 150, 40))
    # 三角：左半红、右半蓝，靠画的顺序拼色
    pts = _tri_points(C // 2 + int(0.01 * C), C // 2, 0.44, 0.50)
    d.polygon(pts, fill=US_RED)
    mid = pts[0][0] + (pts[1][0] - pts[0][0]) // 2
    d.polygon([(mid, pts[0][1]), pts[1], (mid, pts[2][1])], fill=US_BLUE)
    # 粗描边
    d.line(pts + [pts[0]], fill=(22, 20, 18), width=int(0.030 * C), joint="curve")
    return _finish(body)


# ---------------------------------------------------------------- 苹果照片风格（径向 / 花瓣）
# 用户反馈"三角形太像播放器"。苹果「照片」的图标从来不用播放三角，
# 它的语言是：白底 + 一组半透明彩色花瓣径向排布，重叠处自然加深 ——
# 视觉上像一朵花/一片光晕，语义上是"一堆照片叠在一起"。
# 这里沿这个方向做，但加上本产品的记号（同心圆 = 实况照片）。
APPLE_COLORS = [
    (255, 59, 48),    # red
    (255, 149, 0),    # orange
    (255, 204, 0),    # yellow
    (52, 199, 89),    # green
    (0, 199, 190),    # teal
    (0, 122, 255),    # blue
    (175, 82, 222),   # purple
    (255, 45, 85),    # pink
]


def _petal(angle: float, color, alpha: int, w: float, h: float, dist: float) -> Image.Image:
    """一片花瓣：位于中心上方 dist 处的椭圆，再整体绕中心旋转 angle 度。"""
    lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    d = ImageDraw.Draw(lay)
    pw, ph = int(w * C), int(h * C)
    cx, cy = C // 2, C // 2 - int(dist * C)
    d.ellipse([cx - pw // 2, cy - ph // 2, cx + pw // 2, cy + ph // 2], fill=color + (alpha,))
    return lay.rotate(angle, center=(C // 2, C // 2), resample=Image.BICUBIC)


def _flower(alpha: int = 205, petals: int = 8, w: float = 0.185, h: float = 0.44, dist: float = 0.185):
    """径向花瓣叠加；半透明所以重叠处颜色自然变深，和苹果照片的处理一致。"""
    out = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    for i in range(petals):
        color = APPLE_COLORS[i % len(APPLE_COLORS)]
        out = Image.alpha_composite(out, _petal(i * (360.0 / petals), color, alpha, w, h, dist))
    return out


def _white_plate(body: Image.Image) -> Image.Image:
    """白底圆角方块（照片图标的基础底色），带一点点边缘阴影层次。"""
    plate = _lerp((255, 255, 255), (243, 246, 250), (np.mgrid[0:C, 0:C][0] / (C - 1)).astype(np.float32))
    body.paste(plate, (0, 0))
    return body


def ap_petals() -> Image.Image:
    """花瓣（纯苹果照片语言）：白底 + 8 片半透明彩色花瓣。"""
    body = Image.new("RGB", (C, C), (255, 255, 255))
    _white_plate(body)
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(body, (0, 0))
    icon = Image.alpha_composite(icon, _flower(alpha=205))
    # 中心留白小圆：让花瓣有个"花心"
    m = Image.new("L", (C, C), 0)
    r = int(0.052 * C)
    ImageDraw.Draw(m).ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=255)
    white = Image.new("RGBA", (C, C), (255, 255, 255, 0))
    white.putalpha(m)
    icon = Image.alpha_composite(icon, white)
    return _finish(icon.convert("RGB"))


def ap_petals_live() -> Image.Image:
    """花瓣 + 实况照片记号：花心换成两个同心圆环，明确"会动的照片"。"""
    body = Image.new("RGB", (C, C), (255, 255, 255))
    _white_plate(body)
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(body, (0, 0))
    icon = Image.alpha_composite(icon, _flower(alpha=200))
    mark = rings(C // 2, C // 2, [(0.075, 0.016), (0.034, 0.014)], dot=0.014)
    ink = Image.new("RGBA", (C, C), (255, 255, 255, 0))
    ink.putalpha(mark)
    icon = Image.alpha_composite(icon, ink)
    return _finish(icon.convert("RGB"))


def ap_bloom() -> Image.Image:
    """深色花瓣变体：墨蓝底 + 彩色花瓣，重叠处发光，适合深色任务栏。"""
    body = Image.new("RGB", (C, C), (16, 20, 34))
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(body, (0, 0))
    icon = Image.alpha_composite(icon, _flower(alpha=215, w=0.165, h=0.40))
    m = Image.new("L", (C, C), 0)
    r = int(0.045 * C)
    ImageDraw.Draw(m).ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=255)
    ink = Image.new("RGBA", (C, C), (255, 255, 255, 0))
    ink.putalpha(m)
    icon = Image.alpha_composite(icon, ink)
    return _finish(icon.convert("RGB"))


def ap_ripple() -> Image.Image:
    """涟漪：一圈圈彩色同心环，越往外越淡，中心一个实心点（= 实况照片的波纹感）。"""
    body = Image.new("RGB", (C, C), (255, 255, 255))
    _white_plate(body)
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(body, (0, 0))
    n = 6
    for i in range(n):
        r = int((0.085 + i * 0.055) * C)
        color = APPLE_COLORS[(i * 2) % len(APPLE_COLORS)]
        alpha = 230 - i * 26
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        ImageDraw.Draw(lay).ellipse(
            [C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r],
            outline=color + (alpha,),
            width=int(0.042 * C),
        )
        icon = Image.alpha_composite(icon, lay)
    dot = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    r = int(0.055 * C)
    ImageDraw.Draw(dot).ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=(28, 28, 32, 255))
    icon = Image.alpha_composite(icon, dot)
    return _finish(icon.convert("RGB"))


def ap_stack() -> Image.Image:
    """照片叠放：三张错位相片，白边 + 彩色描边，最上面一张带实况记号。"""
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(Image.new("RGB", (C, C), (246, 247, 249)), (0, 0))
    cards = [(-9, (0, 122, 255)), (0, (0, 199, 190)), (9, (255, 149, 0))]
    for angle, stroke in cards:
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        d = ImageDraw.Draw(lay)
        w, h, rad = int(0.52 * C), int(0.62 * C), int(0.06 * C)
        d.rounded_rectangle(
            [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
            radius=rad,
            fill=stroke + (170,),
            outline=stroke + (255,),
            width=int(0.020 * C),
        )
        inset = int(0.055 * C)
        d.rounded_rectangle(
            [C // 2 - w // 2 + inset, C // 2 - h // 2 + inset,
             C // 2 + w // 2 - inset, C // 2 + h // 2 - inset],
            radius=int(rad * 0.6),
            fill=(255, 255, 255, 235),
        )
        icon = Image.alpha_composite(icon, lay.rotate(angle, center=(C // 2, C // 2), resample=Image.BICUBIC))
    mark = rings(C // 2, C // 2, [(0.070, 0.015), (0.032, 0.013)], dot=0.013)
    ink = Image.new("RGBA", (C, C), (28, 28, 32, 0))
    ink.putalpha(mark)
    icon = Image.alpha_composite(icon, ink)
    return _finish(icon.convert("RGB"))


def ap_shutter() -> Image.Image:
    """光圈：六片叶片绕中心旋转，颜色沿圈渐变 —— 像相机的眼睛。"""
    body = Image.new("RGB", (C, C), (18, 18, 22))
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(body, (0, 0))
    n = 6
    for i in range(n):
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        d = ImageDraw.Draw(lay)
        r = int(0.40 * C)
        a = i * (360.0 / n)
        col = APPLE_COLORS[i % len(APPLE_COLORS)]
        # 一片"叶片"：从中心扇形切出
        d.pieslice(
            [C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r],
            start=a, end=a + 360.0 / n, fill=col + (235,),
        )
        # 挖掉一块形成错位
        d.pieslice(
            [C // 2 - int(0.22 * C), C // 2 - int(0.22 * C), C // 2 + int(0.22 * C), C // 2 + int(0.22 * C)],
            start=a, end=a + 360.0 / n, fill=(0, 0, 0, 0),
        )
        icon = Image.alpha_composite(icon, lay)
    hole = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    r = int(0.115 * C)
    ImageDraw.Draw(hole).ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=(18, 18, 22, 255))
    icon = Image.alpha_composite(icon, hole)
    return _finish(icon.convert("RGB"))


# ---------------------------------------------------------------- 复古调性 × 径向构图
# 用户要求"风格复古但要像苹果照片（不要播放三角）"，于是保留花瓣/同心圆这套
# 照片语言，把配色、纸感、颗粒、网点换成七十年代印刷与胶片的路子。

PAL_70S = [
    (196, 85, 44), (206, 138, 52), (201, 162, 39), (122, 139, 74),
    (107, 74, 47), (168, 91, 58), (214, 178, 110), (144, 66, 40),
]
PAL_KODAK = [
    (199, 62, 42), (224, 120, 38), (232, 168, 44), (196, 140, 60),
    (150, 60, 40), (210, 90, 50), (230, 190, 120), (170, 80, 50),
]
PAL_DECO = [
    (198, 160, 90), (224, 190, 120), (176, 136, 64), (206, 170, 104),
    (160, 120, 56), (232, 204, 150), (188, 150, 80), (214, 180, 112),
]


def _flower_pal(pal, alpha: int = 210, petals: int = 8, w: float = 0.185,
                h: float = 0.44, dist: float = 0.185) -> Image.Image:
    """按给定调色板铺花瓣，其余同 _flower。"""
    out = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    for i in range(petals):
        out = Image.alpha_composite(out, _petal(i * (360.0 / petals), pal[i % len(pal)], alpha, w, h, dist))
    return out


def _paper(color=(240, 230, 212), drop: int = 12) -> Image.Image:
    """纸张底：轻微竖向压暗，模拟旧纸。"""
    t = (np.mgrid[0:C, 0:C][0] / (C - 1)).astype(np.float32)
    return _lerp(color, tuple(max(0, c - drop) for c in color), t)


def _grain(img: Image.Image, amount: float = 6.0, seed: int = 7) -> Image.Image:
    """颗粒：给 RGB 加一点高斯噪声，复古印刷的质感。"""
    rng = np.random.default_rng(seed)
    arr = np.asarray(img.convert("RGB")).astype(np.float32)
    arr += rng.normal(0, amount, arr.shape[:2])[:, :, None]
    return Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))


def _vignette(img: Image.Image, strength: float = 0.20) -> Image.Image:
    yy, xx = np.mgrid[0:C, 0:C].astype(np.float32)
    d = np.sqrt((xx - C / 2) ** 2 + (yy - C / 2) ** 2) / (C * 0.72)
    mask = np.clip(1 - strength * np.clip(d, 0, 1) ** 2.2, 0, 1)[:, :, None]
    arr = np.asarray(img.convert("RGB")).astype(np.float32) * mask
    return Image.fromarray(np.clip(arr, 0, 255).astype(np.uint8))


def _rings_ink(mark_color=(58, 40, 28)) -> Image.Image:
    mark = rings(C // 2, C // 2, [(0.078, 0.016), (0.036, 0.014)], dot=0.014)
    ink = Image.new("RGBA", (C, C), mark_color + (0,))
    ink.putalpha(mark)
    return ink


def _centered(icon: Image.Image) -> Image.Image:
    """把 RGBA 图标按 squircle 裁切，输出 RGBA。"""
    out = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    out.paste(icon.convert("RGB"), (0, 0), squircle_mask())
    return out


def rf_70s() -> Image.Image:
    """七十年代印刷：牛皮米底、芥末/砖红/橄榄花瓣、深棕同心环。"""
    icon = _paper((242, 232, 212)).convert("RGBA")
    icon = Image.alpha_composite(icon, _flower_pal(PAL_70S, alpha=210))
    icon = Image.alpha_composite(icon, _rings_ink((62, 44, 30)))
    body = _grain(_vignette(icon.convert("RGB"), 0.18), 6.0)
    return _centered(body)


def rf_kodak() -> Image.Image:
    """柯达暖调：更红更橙，纸底略黄，颗粒更明显。"""
    icon = _paper((248, 236, 214)).convert("RGBA")
    icon = Image.alpha_composite(icon, _flower_pal(PAL_KODAK, alpha=215))
    icon = Image.alpha_composite(icon, _rings_ink((70, 34, 24)))
    body = _grain(_vignette(icon.convert("RGB"), 0.16), 7.5)
    return _centered(body)


def rf_deco() -> Image.Image:
    """装饰艺术：炭黑底、金铜色花瓣、金色同心弧 —— 像老剧院海报。"""
    icon = Image.new("RGBA", (C, C), (26, 24, 28, 255))
    icon = Image.alpha_composite(icon, _flower_pal(PAL_DECO, alpha=235))
    # 两道细金环
    for rr, wdt in ((0.415, 0.008), (0.445, 0.005)):
        r = int(rr * C)
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        ImageDraw.Draw(lay).ellipse(
            [C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r],
            outline=(214, 180, 112, 235),
            width=int(wdt * C),
        )
        icon = Image.alpha_composite(icon, lay)
    icon = Image.alpha_composite(icon, _rings_ink((232, 204, 150)))
    body = _grain(_vignette(icon.convert("RGB"), 0.24), 4.0)
    return _centered(body)


def rf_halftone() -> Image.Image:
    """旧印刷网点：花瓣用网点表现，三色油墨错位，像老画册封面。"""
    mask = _flower_pal([(255, 255, 255)], alpha=255)
    mask_l = mask.getchannel("A")
    body = _paper((246, 238, 222)).convert("RGB")
    d = ImageDraw.Draw(body)
    step = int(0.026 * C)
    r = int(0.013 * C)
    inks = [(196, 56, 44), (228, 148, 36), (36, 124, 134)]
    px = mask_l.load()
    for y in range(0, C, step):
        for x in range(0, C, step):
            if px[min(C - 1, x), min(C - 1, y)] > 90:
                ink = inks[(x // step + y // step) % len(inks)]
                d.ellipse([x - r, y - r, x + r, y + r], fill=ink)
    # 中心同心环也做成实心，压住网点
    icon = Image.alpha_composite(body.convert("RGBA"), _rings_ink((52, 40, 30)))
    out = _grain(icon.convert("RGB"), 3.0)
    return _centered(out)


def rf_glow() -> Image.Image:
    """老镜头光晕：棕褐径向渐变 + 奶油色同心环 + 明显暗角。"""
    cx, cy = C // 2, C // 2
    yy, xx = np.mgrid[0:C, 0:C].astype(np.float32)
    dist = np.sqrt((xx - cx) ** 2 + (yy - cy) ** 2) / (C * 0.62)
    dist = np.clip(dist, 0, 1)
    body = _lerp((206, 168, 96), (44, 30, 22), dist)
    icon = body.convert("RGBA")
    for i, (rr, wdt) in enumerate(((0.30, 0.012), (0.20, 0.010), (0.11, 0.009))):
        r = int(rr * C)
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        ImageDraw.Draw(lay).ellipse(
            [cx - r, cy - r, cx + r, cy + r],
            outline=(248, 236, 208, 190 - i * 30),
            width=int(wdt * C),
        )
        icon = Image.alpha_composite(icon, lay)
    dot = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    r = int(0.035 * C)
    ImageDraw.Draw(dot).ellipse([cx - r, cy - r, cx + r, cy + r], fill=(248, 236, 208, 235))
    icon = Image.alpha_composite(icon, dot)
    out = _grain(_vignette(icon.convert("RGB"), 0.30), 8.0)
    return _centered(out)


def rf_stack() -> Image.Image:
    """棕褐相片叠放：三张老相片错位，纸边发黄，中心同心环。"""
    icon = _paper((238, 226, 204)).convert("RGBA")
    cards = [(-9, (150, 96, 60)), (0, (110, 74, 46)), (9, (176, 132, 78))]
    for angle, tone in cards:
        lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
        d = ImageDraw.Draw(lay)
        w, h, rad = int(0.52 * C), int(0.62 * C), int(0.05 * C)
        d.rounded_rectangle(
            [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
            radius=rad,
            fill=(250, 244, 230, 255),
            outline=tone + (255,),
            width=int(0.020 * C),
        )
        inset = int(0.060 * C)
        d.rounded_rectangle(
            [C // 2 - w // 2 + inset, C // 2 - h // 2 + inset,
             C // 2 + w // 2 - inset, C // 2 + h // 2 - inset],
            radius=int(rad * 0.5),
            fill=tone + (215,),
        )
        icon = Image.alpha_composite(icon, lay.rotate(angle, center=(C // 2, C // 2), resample=Image.BICUBIC))
    icon = Image.alpha_composite(icon, _rings_ink((250, 242, 226)))
    out = _grain(_vignette(icon.convert("RGB"), 0.22), 6.0)
    return _centered(out)


RETRO_FLOWER = {
    "rf-70s": rf_70s,
    "rf-kodak": rf_kodak,
    "rf-deco": rf_deco,
    "rf-halftone": rf_halftone,
    "rf-glow": rf_glow,
    "rf-stack": rf_stack,
}

# ---------------------------------------------------------------- 网点渲染器与造型骨架
INK_CMYK = [(0, 122, 150), (206, 44, 96), (232, 176, 40), (52, 52, 58)]
INK_DUO = [(190, 58, 48), (32, 120, 140)]
INK_WARM = [(196, 56, 44), (228, 148, 36), (120, 74, 44)]
INK_GOLD = [(198, 160, 90), (232, 204, 150), (150, 110, 52)]


def _mask_flower() -> Image.Image:
    return _flower_pal([(255, 255, 255)], alpha=255).getchannel("A")


def _mask_rings() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    for i in range(4):
        r = int((0.10 + i * 0.072) * C)
        d.ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], outline=255, width=int(0.050 * C))
    return m


def _mask_card() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    w, h = int(0.60 * C), int(0.70 * C)
    ImageDraw.Draw(m).rounded_rectangle(
        [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
        radius=int(0.05 * C), fill=255,
    )
    # 相纸白边：把内圈挖掉，只留边框的网点
    inner = Image.new("L", (C, C), 0)
    iw, ih = int(0.44 * C), int(0.46 * C)
    ImageDraw.Draw(inner).rounded_rectangle(
        [C // 2 - iw // 2, C // 2 - ih // 2 - int(0.03 * C), C // 2 + iw // 2, C // 2 + ih // 2 - int(0.03 * C)],
        radius=int(0.02 * C), fill=255,
    )
    frame = ImageChops.subtract(m, inner)
    # 画面区用更稀疏的网点：这里先只返回边框，"画面"由调用方补
    return frame


def _mask_stack() -> Image.Image:
    out = Image.new("L", (C, C), 0)
    for angle in (-10, 0, 10):
        lay = Image.new("L", (C, C), 0)
        w, h = int(0.50 * C), int(0.60 * C)
        ImageDraw.Draw(lay).rounded_rectangle(
            [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
            radius=int(0.05 * C), fill=255,
        )
        lay = lay.rotate(angle, center=(C // 2, C // 2), resample=Image.BICUBIC)
        out = ImageChops.lighter(out, lay)
    return out


def _mask_waves() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    for i in range(5):
        y0 = int((0.24 + i * 0.13) * C)
        pts = []
        for x in range(0, C + 1, int(0.02 * C)):
            t = x / C
            y = y0 + int(math.sin(t * math.pi * 2 + i * 0.7) * 0.045 * C)
            pts.append((x, y))
        d.line(pts, fill=255, width=int(0.070 * C), joint="curve")
    return m


def _mask_aperture() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    r_out, r_in = int(0.42 * C), int(0.14 * C)
    n = 6
    for i in range(n):
        a0 = i * (360.0 / n)
        d.pieslice([C // 2 - r_out, C // 2 - r_out, C // 2 + r_out, C // 2 + r_out],
                   start=a0, end=a0 + 360.0 / n, fill=255)
    d.ellipse([C // 2 - r_in, C // 2 - r_in, C // 2 + r_in, C // 2 + r_in], fill=0)
    return m


def _mask_film() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    w, h = int(0.52 * C), int(0.86 * C)
    ImageDraw.Draw(m).rounded_rectangle(
        [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
        radius=int(0.03 * C), fill=255,
    )
    # 齿孔
    hole = Image.new("L", (C, C), 0)
    hd = ImageDraw.Draw(hole)
    for row in range(2):
        y = C // 2 - h // 2 + int(0.03 * C) + row * (h - int(0.09 * C))
        for i in range(5):
            x = C // 2 - w // 2 + int(0.04 * C) + i * int(0.09 * C)
            hd.rounded_rectangle([x, y, x + int(0.05 * C), y + int(0.045 * C)], radius=int(0.01 * C), fill=255)
    return ImageChops.subtract(m, hole)


def _mask_live() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    for rr, wdt in ((0.40, 0.075), (0.24, 0.070)):
        r = int(rr * C)
        d.ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], outline=255, width=int(wdt * C))
    r = int(0.10 * C)
    d.ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=255)
    return m


def _halftone(mask: Image.Image, inks, paper=(246, 238, 222), step_f: float = 0.026,
              r_f: float = 0.013, thresh: int = 90, mode: str = "angle",
              misreg: float = 0.40, grain: float = 3.0, vig: float = 0.18) -> Image.Image:
    """把掩膜渲染成网点：每个网格点画一个圆，油墨按角度/半径/棋盘分配，并叠一层错位套印。"""
    step = max(2, int(step_f * C))
    r = max(1, int(r_f * C))
    lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    d = ImageDraw.Draw(lay)
    px = mask.load()
    cx = cy = C // 2
    for y in range(step // 2, C, step):
        for x in range(step // 2, C, step):
            if px[x, y] <= thresh:
                continue
            if mode == "angle":
                a = (math.degrees(math.atan2(y - cy, x - cx)) + 360) % 360
                ink = inks[int(a / 360 * len(inks)) % len(inks)]
            elif mode == "radial":
                dist = math.hypot(x - cx, y - cy) / (C * 0.5)
                ink = inks[min(len(inks) - 1, int(dist * len(inks)))]
            else:
                ink = inks[(x // step + y // step) % len(inks)]
            d.ellipse([x - r, y - r, x + r, y + r], fill=ink + (255,))
            if misreg > 0:
                ox, oy = int(step * 0.30), int(step * 0.20)
                ink2 = inks[(inks.index(ink) + 1) % len(inks)]
                d.ellipse([x + ox - r * 0.8, y + oy - r * 0.8, x + ox + r * 0.8, y + oy + r * 0.8],
                          fill=ink2 + (int(255 * misreg),))
    body = _paper(paper).convert("RGBA")
    body = Image.alpha_composite(body, lay)
    return _centered(_grain(_vignette(body.convert("RGB"), vig), grain))


def hf_flower() -> Image.Image:
    """网点 · 花瓣"""
    return _halftone(_mask_flower(), INK_WARM, mode="angle")


def hf_rings() -> Image.Image:
    """网点 · 同心涟漪"""
    return _halftone(_mask_rings(), INK_DUO, mode="radial")


def hf_card() -> Image.Image:
    """网点 · 相片卡片：边框用网点，画面留白成相纸"""
    frame = _halftone(_mask_card(), INK_CMYK, mode="checker", misreg=0.5)
    body = _paper((246, 238, 222)).convert("RGBA")
    body = Image.alpha_composite(body, frame)
    # 画面区：暖棕网点（稀疏）
    inner = Image.new("L", (C, C), 0)
    iw, ih = int(0.44 * C), int(0.46 * C)
    ImageDraw.Draw(inner).rounded_rectangle(
        [C // 2 - iw // 2, C // 2 - ih // 2 - int(0.03 * C),
         C // 2 + iw // 2, C // 2 + ih // 2 - int(0.03 * C)],
        radius=int(0.02 * C), fill=255,
    )
    pic = _halftone(inner, INK_WARM, step_f=0.020, r_f=0.009, mode="angle", misreg=0.25, vig=0.0, grain=0.0)
    body = Image.alpha_composite(body, pic)
    return _centered(_grain(_vignette(body.convert("RGB"), 0.16), 3.0))


def hf_stack() -> Image.Image:
    """网点 · 叠放相片"""
    return _halftone(_mask_stack(), INK_DUO, mode="checker", step_f=0.024, r_f=0.012)


def hf_waves() -> Image.Image:
    """网点 · 波带（连续帧的感觉）"""
    return _halftone(_mask_waves(), INK_WARM, mode="radial", step_f=0.024, r_f=0.012)


def hf_aperture() -> Image.Image:
    """网点 · 光圈"""
    return _halftone(_mask_aperture(), INK_CMYK, mode="angle", step_f=0.024, r_f=0.012)


def hf_film() -> Image.Image:
    """网点 · 胶片条"""
    return _halftone(_mask_film(), INK_WARM, mode="checker", step_f=0.022, r_f=0.011)


def hf_live() -> Image.Image:
    """网点 · 实况照片记号（双环 + 点）作主体"""
    return _halftone(_mask_live(), INK_GOLD, paper=(26, 24, 28), mode="angle",
                     step_f=0.024, r_f=0.012, vig=0.22, grain=3.5)


HALFTONE = {
    "hf-flower": hf_flower,
    "hf-rings": hf_rings,
    "hf-card": hf_card,
    "hf-stack": hf_stack,
    "hf-waves": hf_waves,
    "hf-aperture": hf_aperture,
    "hf-film": hf_film,
    "hf-live": hf_live,
}

# ---------------------------------------------------------------- 三色密集网屏
INK_RED = (222, 56, 52)
INK_YEL = (244, 192, 54)
INK_TEA = (36, 156, 168)
INK_MAG = (206, 62, 132)
INK_CYA = (44, 166, 206)
INK_GOLD = (206, 168, 92)

TRIAD_WARM = [INK_RED, INK_YEL, INK_TEA]
TRIAD_CMYK = [INK_MAG, INK_YEL, INK_CYA]
TRIAD_GOLD = [INK_GOLD, (232, 204, 150), (150, 108, 48)]

SCREEN_ANGLES = (15.0, 45.0, 75.0)


_SCREEN_CACHE: dict = {}


def _screen_dots(step: int, r: int, angle_deg: float) -> Image.Image:
    """一张网屏的点阵掩膜（按参数缓存，多个方案共用同一张网屏）。"""
    key = (step, r, round(angle_deg, 2))
    if key in _SCREEN_CACHE:
        return _SCREEN_CACHE[key]
    lay = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(lay)
    a = math.radians(angle_deg)
    ca, sa = math.cos(a), math.sin(a)
    half = C // 2
    span = int(C * 0.78)
    for iy in range(-span, span + step, step):
        for ix in range(-span, span + step, step):
            x = ca * ix - sa * iy + half
            y = sa * ix + ca * iy + half
            if 0 <= x < C and 0 <= y < C:
                d.ellipse([x - r, y - r, x + r, y + r], fill=255)
    _SCREEN_CACHE[key] = lay
    return lay


def _lighten(ink, k: float = 0.10):
    """油墨半透明：向白色混合一点，避免 multiply 叠印把重叠处压成纯黑。"""
    return tuple(int(255 - (255 - c) * (1.0 - k)) for c in ink)


def _screens(mask: Image.Image, inks, paper=(248, 241, 228), step_f: float = 0.0125,
             r_f: float = 0.0056, angles=SCREEN_ANGLES,
             grain: float = 2.0, vig: float = 0.12) -> Image.Image:
    """三色密集套印：每色一张网屏，裁进掩膜后用 multiply 叠印（模拟半透明油墨）。"""
    step = max(3, int(step_f * C))
    r = max(1, int(r_f * C))
    base = _paper(paper).convert("RGB")
    for ink, ang in zip(inks, angles):
        dots = ImageChops.multiply(_screen_dots(step, r, ang), mask)
        layer = Image.new("RGB", (C, C), (255, 255, 255))
        layer.paste(_lighten(ink), (0, 0), dots)
        base = ImageChops.multiply(base, layer)
    return _centered(_grain(_vignette(base, vig), grain))


def hx_flower() -> Image.Image:
    """三色网屏 · 花瓣（暖三色：红/黄/青）"""
    return _screens(_mask_flower(), TRIAD_WARM)


def hx_rings() -> Image.Image:
    """三色网屏 · 同心涟漪（印刷三色：品红/黄/青）"""
    return _screens(_mask_rings(), TRIAD_CMYK)


def hx_card() -> Image.Image:
    """三色网屏 · 相片卡片：边框与画面分两张网屏，密度不同"""
    inner = Image.new("L", (C, C), 0)
    iw, ih = int(0.44 * C), int(0.46 * C)
    ImageDraw.Draw(inner).rounded_rectangle(
        [C // 2 - iw // 2, C // 2 - ih // 2 - int(0.03 * C),
         C // 2 + iw // 2, C // 2 + ih // 2 - int(0.03 * C)],
        radius=int(0.02 * C), fill=255,
    )
    m = ImageChops.lighter(_mask_card(), inner)
    return _screens(m, TRIAD_CMYK)


def hx_stack() -> Image.Image:
    """三色网屏 · 叠放相片（暖三色）"""
    return _screens(_mask_stack(), TRIAD_WARM)


def hx_waves() -> Image.Image:
    """三色网屏 · 波带"""
    return _screens(_mask_waves(), TRIAD_WARM)


def hx_live_screen() -> Image.Image:
    """三色网屏 · 实况照片双环（金色三重在浅色纸上，套印后偏暖）"""
    return _screens(_mask_live(), TRIAD_GOLD, paper=(250, 244, 232), vig=0.18, grain=2.5)


HALFTONE3 = {
    "hx-flower": hx_flower,
    "hx-rings": hx_rings,
    "hx-card": hx_card,
    "hx-stack": hx_stack,
    "hx-waves": hx_waves,
    "hx-live": hx_live_screen,
}

# ---------------------------------------------------------------- 彩色网点像素（用户选定的风格）
# 一颗颗分开的圆点，颜色按角度/半径分区，纸底透出来 —— 就是最初那张 F 的样子。
INK_HP = [
    (198, 58, 46),    # 砖红
    (226, 120, 40),   # 橙
    (234, 178, 52),   # 芥末黄
    (38, 132, 140),   # 青
    (52, 88, 120),    # 蓝
]


def _pattern_index(i: int, j: int, n: int, pattern: str) -> int:
    """给格子 (i, j) 选一种油墨 —— 关键是让相邻格子落在不同颜色上。

    n=5 时 (i + 2j) mod 5 是拉丁方：水平差 1、垂直差 2、两条对角线差 3/1，
    都不为 0 —— 所以四邻与两个对角都不会同色，同色点被均匀撒开，
    既没有色块成片，也没有斜条纹。
    """
    if pattern == "checker":
        return (i + j) % n
    if pattern == "brick":
        return (i + 2 * j) % n
    if pattern == "latin":
        return (2 * i + 3 * j) % n
    if pattern == "hash":
        return (i * 3 + j * 5 + (i * j) * 7) % n
    # "rand"：稳定的伪随机，避免任何可见的条纹
    h = (i * 73856093) ^ (j * 19349663)
    return (h ^ (h >> 13)) % n


def _hp(mask: Image.Image, inks=None, paper=(246, 238, 222), step_f: float = 0.0245,
        size_f: float = 0.0186, pattern: str = "brick", shape: str = "square",
        rings_ink_col=(58, 40, 28), grain: float = 3.0, vig: float = 0.14) -> Image.Image:
    """把掩膜铺成**交错配色**的像素块：每格一色，相邻不同，纸底透出缝隙。"""
    inks = inks or INK_HP
    step = max(2, int(step_f * C))
    half = max(1, int(size_f * C / 2))
    body = _paper(paper).convert("RGBA")
    lay = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    d = ImageDraw.Draw(lay)
    px = mask.load()
    n = len(inks)
    for j, y in enumerate(range(step // 2, C, step)):
        for i, x in enumerate(range(step // 2, C, step)):
            if px[x, y] <= 100:
                continue
            ink = inks[_pattern_index(i, j, n, pattern)]
            box = [x - half, y - half, x + half, y + half]
            if shape == "square":
                d.rectangle(box, fill=ink + (255,))
            else:
                d.ellipse(box, fill=ink + (255,))
    lay.putalpha(ImageChops.multiply(lay.getchannel("A"), mask))
    body = Image.alpha_composite(body, lay)
    if rings_ink_col is not None:
        body = Image.alpha_composite(body, _rings_ink(rings_ink_col))
    return _centered(_grain(_vignette(body.convert("RGB"), vig), grain))


# ---- 造型骨架 ----
def _mask_star6() -> Image.Image:
    """六角星：两个相反方向的三角形叠一起。"""
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    R = int(0.46 * C)
    cx, cy = C // 2, C // 2
    for k in range(6):
        a0 = math.radians(k * 60 - 90)
        a1 = math.radians(k * 60 - 30)
        d.polygon([(cx, cy), (cx + R * math.cos(a0), cy + R * math.sin(a0)),
                   (cx + R * math.cos(a1), cy + R * math.sin(a1))], fill=255)
    return m


def _mask_grid4() -> Image.Image:
    """2×2 小照片：四块圆角方块，中间留缝。"""
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    gap = int(0.045 * C)
    sz = int(0.34 * C)
    pad = int(0.11 * C)
    for i in range(2):
        for j in range(2):
            x = pad + j * (sz + gap)
            y = pad + i * (sz + gap)
            d.rounded_rectangle([x, y, x + sz, y + sz], radius=int(0.05 * C), fill=255)
    return m


def _mask_reel() -> Image.Image:
    """胶片盘：外圈 + 五个孔 + 中心轴。"""
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    R = int(0.44 * C)
    d.ellipse([C // 2 - R, C // 2 - R, C // 2 + R, C // 2 + R], fill=255)
    hole = Image.new("L", (C, C), 0)
    hd = ImageDraw.Draw(hole)
    r = int(0.10 * C)
    for k in range(5):
        a = math.radians(k * 72 - 90)
        hx = C // 2 + int(0.24 * C * math.cos(a))
        hy = C // 2 + int(0.24 * C * math.sin(a))
        hd.ellipse([hx - r, hy - r, hx + r, hy + r], fill=255)
    r2 = int(0.075 * C)
    hd.ellipse([C // 2 - r2, C // 2 - r2, C // 2 + r2, C // 2 + r2], fill=255)
    return ImageChops.subtract(m, hole)


def _mask_camera() -> Image.Image:
    """相机机身 + 镜头（镜头是个空心圆，中心留出同心环的位置）。"""
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    # 机身
    d.rounded_rectangle([int(0.10 * C), int(0.26 * C), int(0.90 * C), int(0.80 * C)],
                        radius=int(0.07 * C), fill=255)
    # 取景器
    d.rounded_rectangle([int(0.30 * C), int(0.18 * C), int(0.52 * C), int(0.28 * C)],
                        radius=int(0.03 * C), fill=255)
    # 镜头挖空
    r = int(0.20 * C)
    d.ellipse([C // 2 - r, int(0.53 * C) - r, C // 2 + r, int(0.53 * C) + r], fill=0)
    return m


def _mask_pinwheel() -> Image.Image:
    """风车：八片扇形，中间留空放同心环。"""
    m = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(m)
    R = int(0.46 * C)
    r = int(0.14 * C)
    for k in range(8):
        a0 = k * 45
        d.pieslice([C // 2 - R, C // 2 - R, C // 2 + R, C // 2 + R], start=a0 + 6, end=a0 + 39, fill=255)
    d.ellipse([C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r], fill=0)
    return m


def hp_star6() -> Image.Image:
    """彩色网点 · 六角星"""
    return _hp(_mask_star6())


def hp_rings() -> Image.Image:
    """彩色网点 · 同心涟漪"""
    return _hp(_mask_rings())


def hp_card() -> Image.Image:
    """彩色网点 · 相片卡片（边框 + 画面）"""
    inner = Image.new("L", (C, C), 0)
    iw, ih = int(0.44 * C), int(0.46 * C)
    ImageDraw.Draw(inner).rounded_rectangle(
        [C // 2 - iw // 2, C // 2 - ih // 2 - int(0.03 * C),
         C // 2 + iw // 2, C // 2 + ih // 2 - int(0.03 * C)],
        radius=int(0.02 * C), fill=255,
    )
    m = ImageChops.lighter(_mask_card(), inner)
    return _hp(m)


def hp_stack() -> Image.Image:
    """彩色网点 · 叠放相片"""
    return _hp(_mask_stack())


def hp_grid4() -> Image.Image:
    """彩色网点 · 四宫格"""
    return _hp(_mask_grid4(), pattern="checker")


def hp_reel() -> Image.Image:
    """彩色网点 · 胶片盘"""
    return _hp(_mask_reel())


def hp_camera() -> Image.Image:
    """彩色网点 · 相机"""
    return _hp(_mask_camera())


def hp_pinwheel() -> Image.Image:
    """彩色网点 · 风车（八扇，中间同心环）"""
    return _hp(_mask_pinwheel())


HP = {
    "hp-star6": hp_star6,
    "hp-rings": hp_rings,
    "hp-card": hp_card,
    "hp-stack": hp_stack,
    "hp-grid4": hp_grid4,
    "hp-reel": hp_reel,
    "hp-camera": hp_camera,
    "hp-pinwheel": hp_pinwheel,
}

APPLE = {
    "ap-petals": ap_petals,
    "ap-petals-live": ap_petals_live,
    "ap-bloom": ap_bloom,
    "ap-ripple": ap_ripple,
    "ap-stack": ap_stack,
    "ap-shutter": ap_shutter,
}

EXTRA = {
    "bh-primitives": bh_primitives,
    "bh-diagonal": bh_diagonal,
    "bh-rings": bh_rings,
    "bh-grid": bh_grid,
    "am-bass": am_bass,
    "am-flag": am_flag,
    "am-diner": am_diner,
    "am-pop": am_pop,
}

RETRO = {
    "retro-polaroid": retro_polaroid,
    "retro-film": retro_film,
    "retro-crt": retro_crt,
    "retro-lens": retro_lens,
    "retro-pixel": retro_pixel,
    "retro-sunset": retro_sunset,
    "retro-bauhaus": retro_bauhaus,
    **EXTRA,
    **APPLE,
    **RETRO_FLOWER,
    **HALFTONE,
    **HALFTONE3,
    **HP,
}

VARIANTS = list(SPECS) + list(RETRO)


# ---------------------------------------------------------------- 合成与输出
def compose(variant: str = DEFAULT_VARIANT) -> Image.Image:
    if variant in RETRO:
        return RETRO[variant]()

    spec = SPECS[variant]
    mask = squircle_mask()

    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(background(spec["bg"]), (0, 0), mask)

    for name, color in spec["layers"]:
        layer = ImageChops.multiply(layer_mask(name), mask)
        ink = Image.new("RGBA", (C, C), color + (0,))
        ink.putalpha(layer)
        icon.alpha_composite(ink)
    return icon


def write_icons(variant: str) -> None:
    big = compose(variant)
    ICONS.mkdir(parents=True, exist_ok=True)
    for px, name in {1024: "icon.png", 256: "128x128@2x.png", 128: "128x128.png", 32: "32x32.png"}.items():
        big.resize((px, px), Image.LANCZOS).save(ICONS / name)
        print(f"  {name:20s} {px}x{px}")
    ico_sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    big.save(ICONS / "icon.ico", format="ICO", sizes=[(n, n) for n in ico_sizes])
    print(f"  icon.ico             含 {len(ico_sizes)} 个尺寸 (变体 {variant})")


def write_sheet(out: Path, stock: Path | None, variants: list[str]) -> None:
    cols: list[tuple[str, Image.Image]] = []
    if stock and stock.exists():
        cols.append(("A-stock", Image.open(stock).convert("RGBA")))
    letters = "BCDEFGHIJ"
    for i, v in enumerate(variants):
        cols.append((f"{letters[i]}-{v}", compose(v)))

    big_px, pad = 176, 30
    col_w = big_px + pad
    width = pad + len(cols) * col_w
    row_h = big_px + 104
    height = 2 * row_h + 24
    sheet = Image.new("RGB", (width, height), (255, 255, 255))
    d = ImageDraw.Draw(sheet)
    d.rectangle([0, row_h + 24, width, height], fill=(24, 24, 27))

    for row, (y0, fg) in enumerate(((12, (40, 40, 46)), (row_h + 36, (230, 230, 235)))):
        for i, (label, img) in enumerate(cols):
            x = pad + i * col_w
            big = img.resize((big_px, big_px), Image.LANCZOS)
            sheet.paste(big, (x, y0 + 6), big)
            d.text((x, y0 + 6 + big_px + 4), label, fill=fg)
            sx = x
            for n in (48, 32, 16):
                small = img.resize((n, n), Image.LANCZOS)
                sheet.paste(small, (sx, y0 + 6 + big_px + 24), small)
                sx += n + 8
    sheet.save(out)
    print(f"候选对比图：{out}（{len(cols)} 列）")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--variant", default=DEFAULT_VARIANT, choices=VARIANTS)
    ap.add_argument("--sheet", type=Path, default=None, help="只输出候选对比图")
    ap.add_argument(
        "--sheet-variants", default="badge,duo-dark,duo-light,duo-split,duo-tile",
        help="对比图里要列出的变体，逗号分隔",
    )
    ap.add_argument("--stock", type=Path, default=None, help="对比图里附带的原始图标 png")
    args = ap.parse_args()

    if args.sheet:
        picked = [v.strip() for v in args.sheet_variants.split(",") if v.strip()]
        write_sheet(args.sheet, args.stock, picked)
    else:
        write_icons(args.variant)


if __name__ == "__main__":
    main()

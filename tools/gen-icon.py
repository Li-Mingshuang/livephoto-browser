"""生成 app 图标（多尺寸 .ico + 各尺寸 png）。

为什么要用脚本生成而不是找个图标：
  1. 可复现 —— 任何时候 `python tools/gen-icon.py` 都得到同样的图标；
  2. 没有版权顾虑（自己画的）；
  3. 尺寸齐全 —— Windows 在 16/20/24/32/40/48/64/128/256 不同位置用不同尺寸，
     只给一张大图让系统缩，任务栏和标题栏上会糊。

设计（1024 画布，超采样后下采样，边缘自然抗锯齿）：
  - 圆角方块（近似 Apple squircle）+ 蓝→青对角渐变 + 顶部柔光
  - 白色播放三角（圆角，用"模糊后阈值化"得到圆角，而不是画粗折线）
  - 一圈约 300° 的白色圆环（暗示 Live / 回放），缺口朝右上
理由：16px 下能看清的只有"蓝底 + 白三角"，所以主体必须是这两样；
圆环只在大尺寸提供"会动的照片"的暗示，小尺寸糊掉也不影响识别。

用法：python tools/gen-icon.py
"""
from __future__ import annotations

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

TOP = (46, 124, 246)     # #2E7CF6
BOT = (20, 194, 206)     # #14C2CE
RADIUS = int(0.235 * C)


def gradient() -> Image.Image:
    y, x = np.mgrid[0:C, 0:C].astype(np.float32)
    t = ((x + y) / (2.0 * (C - 1)))[:, :, None]
    top = np.array(TOP, dtype=np.float32)[None, None, :]
    bot = np.array(BOT, dtype=np.float32)[None, None, :]
    rgb = top * (1.0 - t) + bot * t
    return Image.fromarray(rgb.round().astype(np.uint8), "RGB")


def squircle_mask() -> Image.Image:
    m = Image.new("L", (C, C), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, C - 1, C - 1], radius=RADIUS, fill=255)
    return m


def blur_round(mask: Image.Image, sigma: float) -> Image.Image:
    """把尖锐的多边形边角磨圆：模糊后阈值化（形态学开运算的近似）。"""
    return mask.filter(ImageFilter.GaussianBlur(sigma)).point(lambda v: 255 if v >= 128 else 0)


def play_triangle() -> Image.Image:
    """白色实心三角，圆角。"""
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    cx, cy = C // 2 + int(0.020 * C), C // 2     # 右移 2% 做视觉居中
    w, h = int(0.40 * C), int(0.46 * C)
    left, top = cx - w // 2, cy - h // 2
    # 先画得比目标略大一点，模糊阈值会收缩边缘
    grow = int(0.012 * C)
    d.polygon(
        [(left - grow, top - grow), (left + w + grow, cy), (left - grow, top + h + grow)],
        fill=255,
    )
    return blur_round(layer, 0.022 * C)


def live_ring() -> Image.Image:
    """300° 白色圆环，缺口朝右上。"""
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    r = int(0.385 * C)
    box = [C // 2 - r, C // 2 - r, C // 2 + r, C // 2 + r]
    width = int(0.028 * C)
    d.arc(box, start=-58, end=242, fill=255, width=width)
    # 圆头：在弧的两个端点各补一个小圆
    for deg in (-58, 242):
        a = math.radians(deg)
        px, py = C // 2 + r * math.cos(a), C // 2 + r * math.sin(a)
        rr = width // 2
        d.ellipse([px - rr, py - rr, px + rr, py + rr], fill=255)
    return layer


def compose() -> Image.Image:
    bg = gradient()

    # 顶部柔光：白色椭圆 + 高斯模糊，压低强度做成"柔光"
    glow = Image.new("L", (C, C), 0)
    ImageDraw.Draw(glow).ellipse([-0.25 * C, -0.70 * C, 1.25 * C, 0.50 * C], fill=255)
    glow = glow.filter(ImageFilter.GaussianBlur(0.12 * C)).point(lambda v: v // 5)
    bg = Image.composite(Image.new("RGB", (C, C), (255, 255, 255)), bg, glow)

    mask = squircle_mask()
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(bg, (0, 0), mask)

    glyph = ImageChops.lighter(live_ring(), play_triangle())
    glyph = ImageChops.multiply(glyph, mask)          # 不超出圆角
    white = Image.new("RGBA", (C, C), (255, 255, 255, 0))
    white.putalpha(glyph)
    icon.alpha_composite(white)
    return icon


def main() -> None:
    big = compose()
    ICONS.mkdir(parents=True, exist_ok=True)

    for px, name in {1024: "icon.png", 256: "128x128@2x.png", 128: "128x128.png", 32: "32x32.png"}.items():
        big.resize((px, px), Image.LANCZOS).save(ICONS / name)
        print(f"  {name:20s} {px}x{px}")

    ico_sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    big.save(ICONS / "icon.ico", format="ICO", sizes=[(n, n) for n in ico_sizes])
    print(f"  icon.ico             含 {len(ico_sizes)} 个尺寸: {ico_sizes}")


if __name__ == "__main__":
    main()

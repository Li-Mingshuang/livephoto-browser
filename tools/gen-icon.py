"""生成 app 图标（多尺寸 .ico + 各尺寸 png）。

为什么用脚本生成：
  1. 可复现 —— 任何时候 `python tools/gen-icon.py` 都得到同样的图标；
  2. 没有版权顾虑（自己画的）；
  3. 尺寸齐全 —— Windows 在 16/20/24/32/40/48/64/128/256 不同位置用不同尺寸，
     只给一张大图让系统缩，任务栏和标题栏上会糊。

设计（1024 画布，超采样后下采样，边缘自然抗锯齿）：
  - 圆角方块（近似 Apple squircle）+ 蓝→青对角渐变 + 顶部柔光
  - 白色播放三角（圆角用"先高斯模糊再阈值化"得到）
  - 左上角一个"实况照片"标记：两个同心圆环 + 中心圆点（iOS 实况照片的记号）

构图意图：三角说"会播放"，同心圆说"这是实况照片"，两者分开摆，
小尺寸先糊掉角上的圆环、留下蓝底 + 白三角，识别依然成立。

用法：python tools/gen-icon.py
"""
from __future__ import annotations

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


def live_badge() -> Image.Image:
    """左上角的"实况照片"标记：两个同心圆环 + 中心圆点（iOS 实况照片的那个记号）。

    这个记号是"会动的照片"最直接的语义。放在角上，小尺寸糊掉也不影响主体识别。
    """
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    cx = cy = int(0.255 * C)
    for r, w in ((int(0.088 * C), int(0.020 * C)), (int(0.050 * C), int(0.020 * C))):
        d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=255, width=w)
    rr = int(0.018 * C)
    d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=255)
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

    glyph = ImageChops.lighter(live_badge(), play_triangle())
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

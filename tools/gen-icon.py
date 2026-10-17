"""生成 app 图标（多尺寸 .ico + 各尺寸 png），并支持出候选图。

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

用法：
  python tools/gen-icon.py                    # 写出正式图标（变体 badge）
  python tools/gen-icon.py --variant ring     # 换变体
  python tools/gen-icon.py --sheet out.png    # 只出候选对比图，不写正式图标
"""
from __future__ import annotations

import argparse
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

VARIANTS = ["ring", "badge", "frame", "live"]
DEFAULT_VARIANT = "badge"


# ---------------------------------------------------------------- 基础绘制
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


def triangle(cx: int, cy: int, w: float, h: float) -> Image.Image:
    """以 (cx, cy) 为中心的白色实心三角，圆角。尺寸按画布比例。"""
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    w, h = int(w * C), int(h * C)
    left, top = cx - w // 2, cy - h // 2
    grow = int(0.012 * C)
    d.polygon(
        [(left - grow, top - grow), (left + w + grow, cy), (left - grow, top + h + grow)],
        fill=255,
    )
    return blur_round(layer, 0.022 * C)


def rings(cx: int, cy: int, radii: list[tuple[float, float]], dot: float = 0.0) -> Image.Image:
    """同心圆环（半径、线宽都按画布比例），可选中心圆点。"""
    layer = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(layer)
    for r, w in radii:
        r, w = int(r * C), int(w * C)
        d.ellipse([cx - r, cy - r, cx + r, cy + r], outline=255, width=w)
    if dot > 0:
        rr = int(dot * C)
        d.ellipse([cx - rr, cy - rr, cx + rr, cy + rr], fill=255)
    return layer


def play_triangle() -> Image.Image:
    """居中播放三角（右移 2% 做视觉居中）。"""
    return triangle(C // 2 + int(0.020 * C), C // 2, 0.40, 0.46)


# ---------------------------------------------------------------- 各变体的图形
def glyph_ring() -> Image.Image:
    """第一版：约 300° 圆环套住播放三角（想表达"播放 + 循环"）。"""
    ring = rings(C // 2, C // 2, [(0.385, 0.028)])
    # 缺口：把圆环右上角切掉一块
    cut = Image.new("L", (C, C), 0)
    ImageDraw.Draw(cut).ellipse(
        [int(0.30 * C), int(-0.18 * C), int(0.78 * C), int(0.22 * C)], fill=255
    )
    ring = ImageChops.subtract(ring, cut)
    return ImageChops.lighter(ring, play_triangle())


def glyph_badge() -> Image.Image:
    """当前版：居中播放三角 + 左上角实况照片记号。"""
    return ImageChops.lighter(live_badge(), play_triangle())


def glyph_frame() -> Image.Image:
    """变体：白描边相框，框内一个播放三角（"一张能播放的照片"）。"""
    frame = Image.new("L", (C, C), 0)
    d = ImageDraw.Draw(frame)
    w, h = int(0.62 * C), int(0.50 * C)
    stroke = int(0.045 * C)
    d.rounded_rectangle(
        [C // 2 - w // 2, C // 2 - h // 2, C // 2 + w // 2, C // 2 + h // 2],
        radius=int(0.09 * C),
        outline=255,
        width=stroke,
    )
    tri = triangle(C // 2 + int(0.012 * C), C // 2, 0.22, 0.26)
    return ImageChops.lighter(frame, tri)


def glyph_live() -> Image.Image:
    """变体：同心圆记号作为主体，右下角一个小播放三角。"""
    mark = rings(C // 2, C // 2, [(0.30, 0.034), (0.19, 0.034)], dot=0.085)
    tri = triangle(int(0.70 * C), int(0.72 * C), 0.16, 0.19)
    return ImageChops.lighter(mark, tri)


def live_badge() -> Image.Image:
    """左上角的实况照片记号：两个同心圆环 + 中心圆点。"""
    return rings(int(0.255 * C), int(0.255 * C), [(0.088, 0.020), (0.050, 0.020)], dot=0.018)


GLYPHS = {
    "ring": glyph_ring,
    "badge": glyph_badge,
    "frame": glyph_frame,
    "live": glyph_live,
}


# ---------------------------------------------------------------- 合成与输出
def compose(variant: str = DEFAULT_VARIANT) -> Image.Image:
    bg = gradient()

    glow = Image.new("L", (C, C), 0)
    ImageDraw.Draw(glow).ellipse([-0.25 * C, -0.70 * C, 1.25 * C, 0.50 * C], fill=255)
    glow = glow.filter(ImageFilter.GaussianBlur(0.12 * C)).point(lambda v: v // 5)
    bg = Image.composite(Image.new("RGB", (C, C), (255, 255, 255)), bg, glow)

    mask = squircle_mask()
    icon = Image.new("RGBA", (C, C), (0, 0, 0, 0))
    icon.paste(bg, (0, 0), mask)

    glyph = ImageChops.multiply(GLYPHS[variant](), mask)
    white = Image.new("RGBA", (C, C), (255, 255, 255, 0))
    white.putalpha(glyph)
    icon.alpha_composite(white)
    return icon


def write_icons(variant: str) -> None:
    big = compose(variant)
    ICONS.mkdir(parents=True, exist_ok=True)
    for px, name in {1024: "icon.png", 256: "128x128@2x.png", 128: "128x128.png", 32: "32x32.png"}.items():
        big.resize((px, px), Image.LANCZOS).save(ICONS / name)
        print(f"  {name:20s} {px}x{px}")
    ico_sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    big.save(ICONS / "icon.ico", format="ICO", sizes=[(n, n) for n in ico_sizes])
    print(f"  icon.ico             含 {len(ico_sizes)} 个尺寸: {ico_sizes}  (变体 {variant})")


def write_sheet(out: Path, stock: Path | None) -> None:
    """候选对比图：每列一个变体，上排 192px、下排 48/32/16px，浅深底各一行。"""
    cols: list[tuple[str, Image.Image]] = []
    if stock and stock.exists():
        cols.append(("A-stock", Image.open(stock).convert("RGBA")))
    letters = "BCDEFG"
    for i, v in enumerate(VARIANTS):
        cols.append((f"{letters[i]}-{v}", compose(v)))

    big_px, pad = 192, 34
    col_w = big_px + pad
    width = pad + len(cols) * col_w
    height = 2 * (big_px + 108) + 40
    sheet = Image.new("RGB", (width, height), (255, 255, 255))
    d = ImageDraw.Draw(sheet)

    def band(y: int, color: tuple[int, int, int]) -> None:
        d.rectangle([0, y, width, y + big_px + 108], fill=color)

    band(0, (255, 255, 255))
    band(big_px + 108, (28, 28, 30))

    for row, (bg_y, fg) in enumerate(((0, (40, 40, 46)), (big_px + 108, (230, 230, 235)))):
        for i, (label, img) in enumerate(cols):
            x = pad + i * col_w
            big = img.resize((big_px, big_px), Image.LANCZOS)
            sheet.paste(big, (x, bg_y + 16), big)
            d.text((x, bg_y + 16 + big_px + 4), label, fill=fg)
            # 小尺寸一排
            sx = x
            for n in (48, 32, 16):
                small = img.resize((n, n), Image.LANCZOS)
                sheet.paste(small, (sx, bg_y + 16 + big_px + 26), small)
                sx += n + 10
    sheet.save(out)
    print(f"候选对比图：{out}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--variant", default=DEFAULT_VARIANT, choices=VARIANTS)
    ap.add_argument("--sheet", type=Path, default=None, help="只输出候选对比图")
    ap.add_argument("--stock", type=Path, default=None, help="对比图里附带的原始图标 png")
    args = ap.parse_args()

    if args.sheet:
        write_sheet(args.sheet, args.stock)
    else:
        write_icons(args.variant)


if __name__ == "__main__":
    main()

"""Baseline: how fast can we make grid thumbnails from this library?"""
import os, time, io, statistics, sys
from PIL import Image
import pillow_heif
pillow_heif.register_heif_opener()

ROOT = r"F:\DCIM"
heics = []
for dp, dn, fn in os.walk(ROOT):
    for f in fn:
        if f.lower().endswith(('.heic', '.heif')):
            heics.append(os.path.join(dp, f))
        if len(heics) >= 24:
            break
    if len(heics) >= 24:
        break

print("samples:", len(heics))
print("cores:", os.cpu_count())

full, thumb, emb = [], [], []
for p in heics:
    t0 = time.perf_counter()
    im = Image.open(p)
    im.load()
    t1 = time.perf_counter()
    full.append((t1 - t0) * 1000)
    w, h = im.size
    im2 = im.convert('RGB')
    im2.thumbnail((512, 512), Image.Resampling.LANCZOS)
    buf = io.BytesIO()
    im2.save(buf, 'WEBP', quality=82, method=4)
    t2 = time.perf_counter()
    thumb.append((t2 - t1) * 1000)

    # what the embedded EXIF thumbnail costs
    try:
        t3 = time.perf_counter()
        im3 = Image.open(p)
        im3.getexif()
        t4 = time.perf_counter()
        emb.append((t4 - t3) * 1000)
    except Exception:
        pass

def stat(name, v):
    if not v:
        return
    v = sorted(v)
    print(f"  {name:28s} p50={statistics.median(v):7.1f}ms  p90={v[int(len(v)*0.9)-1]:7.1f}ms  min={v[0]:7.1f}  max={v[-1]:7.1f}")

print(f"sizes: {w}x{h} (last sample)")
stat("HEIC full decode", full)
stat("decode->512 webp encode", thumb)
print("\nprojection for 10,000 HEIC at p50 full-decode, 8 parallel workers:")
p50 = statistics.median(full)
print(f"  {p50*10000/8/1000:.0f} s single-thread-equivalent /8 workers = {p50*10000/8/1000:.0f}s")

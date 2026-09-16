"""Deeper probe: how does iCloud-for-Windows actually name/encode Live Photos?"""
import os, re, collections, sys

ROOT = r"C:\Users\lms\Pictures\iCloud Photos\Photos"

print("== raw read permission test ==")
try:
    with open(os.path.join(ROOT, "IMG_0020.MOV"), "rb") as fh:
        head = fh.read(32)
    print("read OK, first bytes:", head)
except Exception as e:
    print("READ FAILED:", type(e).__name__, e)

files = sorted(os.listdir(ROOT))
print("\n== video-ish files ==")
vids = [f for f in files if os.path.splitext(f)[1].lower() in ('.mov', '.mp4')]
for f in vids:
    print(f"  {os.path.getsize(os.path.join(ROOT,f))/1048576:6.2f} MB  {f}")

print("\n== still files (first 40) ==")
stills = [f for f in files if os.path.splitext(f)[1].lower() in ('.heic', '.jpg', '.jpeg')]
for f in stills[:40]:
    print(f"  {os.path.getsize(os.path.join(ROOT,f))/1048576:6.2f} MB  {f}")
print(f"  ... total stills: {len(stills)}")

# name pattern analysis
def stem_parts(names):
    pats = collections.Counter()
    for n in names:
        s = os.path.splitext(n)[0]
        pats[re.sub(r'\d+', '#', s)] += 1
    return pats

print("\n== name patterns ==")
print("videos:", dict(stem_parts(vids)))
print("stills:", dict(stem_parts(stills)))

MARKERS = {
    b'com.apple.quicktime.content.identifier': 'CONTENT_IDENTIFIER',
    b'com.apple.quicktime.still-image-time': 'STILL_IMAGE_TIME',
    b'com.apple.quicktime.live-photo-info': 'LIVE_PHOTO_INFO',
    b'17U': None,  # quicktime udta '17U' atom? placeholder
}
CODECS = [b'hvc1', b'hev1', b'avc1', b'avc3', b'mp4v', b'ap4h']

print("\n== video marker/codec scan ==")
for f in vids:
    p = os.path.join(ROOT, f)
    with open(p, 'rb') as fh:
        blob = fh.read()
    marks = [lab for m, lab in MARKERS.items() if lab and m in blob]
    codecs = [c.decode() for c in CODECS if c in blob]
    m = re.search(rb'com\.apple\.quicktime\.content\.identifier', blob)
    cid = None
    if m:
        seg = blob[m.start():m.start()+200]
        # identifier value usually follows as a 4-byte size then the uuid string
        t = re.search(rb'[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}', seg, re.I)
        cid = t.group(0).decode() if t else 'present-but-unparsed'
    print(f"  {f:16s} codecs={codecs} marks={marks} cid={cid}")

print("\n== still marker scan (stills paired to video names) ==")
still_names = set(stills)
for f in vids:
    stem = os.path.splitext(f)[0]
    cands = [n for n in still_names if os.path.splitext(n)[0] == stem]
    print(f"  {f}: exact-stem sibling -> {cands}")

print("\n== do any stills carry a ContentIdentifier? ==")
hits = 0
for f in stills[:60]:
    p = os.path.join(ROOT, f)
    with open(p, 'rb') as fh:
        blob = fh.read()
    if b'ContentIdentifier' in blob or b'content.identifier' in blob:
        hits += 1
        m = re.search(rb'ContentIdentifier', blob)
        print(f"  {f}: {blob[max(0,m.start()-40):m.start()+70]!r}")
print(f"  hits among {min(60,len(stills))} sampled stills: {hits}")

print("\n== HEIC decode attempt ==")
try:
    from PIL import Image
    import pillow_heif
    pillow_heif.register_heif_opener()
    heics = [f for f in stills if f.lower().endswith(('.heic', '.heif'))]
    print("  heic count:", len(heics))
    if heics:
        p = os.path.join(ROOT, heics[0])
        print("  trying:", p)
        im = Image.open(p)
        print("  OK:", im.size, im.mode, im.format)
except Exception as e:
    print("  FAILED:", type(e).__name__, repr(e))

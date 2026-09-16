"""Analyse real iPhone-exported Live Photo pairs on a local disk.

Determines: naming/pairing convention, HEIC container brand, MOV video codec,
and whether Apple content-identifier metadata links the two halves.
"""
import os, re, sys, collections

ROOTS = sys.argv[1:] or [r"F:\DCIM"]
MAX = 100_000

def collect(roots):
    out = []
    for root in roots:
        for dp, dn, fn in os.walk(root):
            dn[:] = [d for d in dn if not d.startswith('.')]
            for f in fn:
                out.append(os.path.join(dp, f))
                if len(out) >= MAX:
                    return out
    return out

files = collect(ROOTS)
print("scanned:", len(files))
ext = collections.Counter(os.path.splitext(f)[1].lower() for f in files)
print("ext:", dict(sorted(ext.items(), key=lambda kv: -kv[1])[:16]))

stills = [f for f in files if os.path.splitext(f)[1].lower() in ('.heic', '.heif', '.jpg', '.jpeg', '.png')]
vids = [f for f in files if os.path.splitext(f)[1].lower() in ('.mov', '.mp4')]
print("stills:", len(stills), " videos:", len(vids))

# ---------- pairing by identical stem ----------
still_by_key = collections.defaultdict(list)
for f in stills:
    d, b = os.path.split(f)
    still_by_key[(d.lower(), os.path.splitext(b)[0].lower())].append(f)

paired, orphans = [], []
for v in vids:
    d, b = os.path.split(v)
    k = (d.lower(), os.path.splitext(b)[0].lower())
    if k in still_by_key:
        paired.append((still_by_key[k][0], v))
    else:
        orphans.append(v)

print(f"\n-- stem pairing --")
print(f"  paired  : {len(paired)}")
print(f"  orphan  : {len(orphans)}")
if paired:
    exts = collections.Counter(os.path.splitext(a)[1].lower() + '+' + os.path.splitext(b)[1].lower() for a, b in paired)
    print("  combos  :", dict(exts))
    print("  examples:")
    for a, b in paired[:5]:
        print(f"    {os.path.basename(a):22s} + {os.path.basename(b):22s} share stem")
if orphans:
    print("  orphan examples:", [os.path.basename(o) for o in orphans[:5]])

# ---------- metadata analysis on sample pairs ----------
CODECS = [b'hvc1', b'hev1', b'avc1', b'avc3', b'ap4h', b'vp09']
UID_RE = re.compile(rb'[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}')

def read_head(p, n):
    with open(p, 'rb') as fh:
        return fh.read(n)

print("\n-- sample pair metadata (up to 6) --")
for a, b in paired[:6]:
    print(f"\n  {os.path.basename(a)}  {os.path.getsize(a)/1048576:.2f} MB   brand={read_head(a,12)[4:12]}")
    sb = read_head(a, 3_000_000)
    for marker in (b'ContentIdentifier', b'com.apple.quicktime.content.identifier', b'apple-metadata'):
        if marker in sb:
            m = re.search(re.escape(marker), sb)
            print(f"    still has {marker.decode()}: {sb[m.start():m.start()+90]!r}")
    print(f"  {os.path.basename(b)}  {os.path.getsize(b)/1048576:.2f} MB   brand={read_head(b,12)[4:12]}")
    mb = read_head(b, 6_000_000)
    codecs = [c.decode() for c in CODECS if c in mb]
    print(f"    mov codecs: {codecs}")
    for marker in (b'com.apple.quicktime.content.identifier', b'com.apple.quicktime.still-image-time',
                   b'com.apple.quicktime.live-photo-info'):
        if marker in mb:
            m = re.search(re.escape(marker), mb)
            uid = UID_RE.search(mb[m.start():m.start()+200])
            print(f"    mov has {marker.decode():45s} uuid={uid.group(0).decode() if uid else '?'}")

# ---------- aggregate: do MOVs carry content identifiers? ----------
print("\n-- aggregate marker stats over 40 videos --")
stat = collections.Counter()
for v in vids[:40]:
    try:
        mb = read_head(v, 6_000_000)
    except Exception as e:
        stat['read_error'] += 1
        continue
    if b'com.apple.quicktime.content.identifier' in mb:
        stat['has_content_identifier'] += 1
    if b'com.apple.quicktime.still-image-time' in mb:
        stat['has_still_image_time'] += 1
    for c in ('hvc1', 'hev1', 'avc1'):
        if c.encode() in mb:
            stat[f'codec_{c}'] += 1
print(" ", dict(stat))

# ---------- how many stills carry a content identifier? ----------
print("\n-- still content-identifier stats over 40 stills --")
stat2 = collections.Counter()
for s in stills[:40]:
    blob = read_head(s, 3_000_000)
    stat2['total'] += 1
    if b'ContentIdentifier' in blob:
        stat2['has_ContentIdentifier'] += 1
    if b'com.apple.quicktime.content.identifier' in blob:
        stat2['has_qt_content_identifier'] += 1
print(" ", dict(stat2))

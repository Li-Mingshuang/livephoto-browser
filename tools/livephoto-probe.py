"""Probe a real iPhone Live Photo library to learn its actual on-disk shape.

Read-only reconnaissance used to ground the design proposal.
"""
import os, sys, json, collections, struct, re

ROOT = sys.argv[1] if len(sys.argv) > 1 else r"C:\Users\lms\Pictures\iCloud Photos\Photos"
MAX_SCAN = 4000


def walk(root, limit=MAX_SCAN):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if not d.startswith('.')]
        for fn in filenames:
            if fn.startswith('.'):
                continue
            out.append(os.path.join(dirpath, fn))
            if len(out) >= limit:
                return out
    return out


files = walk(ROOT)
ext = collections.Counter(os.path.splitext(f)[1].lower() for f in files)
print("root:", ROOT)
print("scanned files:", len(files))
print("extensions:", dict(sorted(ext.items(), key=lambda kv: -kv[1])))

# --- pairing analysis -------------------------------------------------
stems = collections.defaultdict(set)
for f in files:
    d, base = os.path.split(f)
    stem, e = os.path.splitext(base)
    stems[(d, stem.lower())].add(e.lower())

moves = {k: v for k, v in stems.items() if '.mov' in v or '.mp4' in v}
paired_still = 0
orphan_mov = 0
pair_ext = collections.Counter()
for (d, stem), exts in moves.items():
    stills = exts & {'.heic', '.heif', '.jpg', '.jpeg', '.png'}
    if stills:
        paired_still += 1
        pair_ext[tuple(sorted(stills))] += 1
    else:
        orphan_mov += 1
print("\n-- pairing by identical filename stem --")
print("mov-like files:", len(moves))
print("  with sibling still:", paired_still)
print("  orphan mov        :", orphan_mov)
print("  still ext combos  :", dict(pair_ext))

# --- sample pair deep dive -------------------------------------------
sample = None
for (d, stem), exts in sorted(moves.items()):
    if exts & {'.heic', '.heif', '.jpg', '.jpeg'}:
        for e in ('.heic', '.jpg', '.jpeg'):
            if e in exts:
                sample = (os.path.join(d, stem + e), os.path.join(d, stem + '.mov'))
                break
    if sample:
        break

MARKERS = {
    b'com.apple.quicktime.content.identifier': 'mov:content-identifier',
    b'com.apple.quicktime.still-image-time': 'mov:still-image-time',
    b'com.apple.quicktime.live-photo': 'mov:live-photo-flag',
    b'ContentIdentifier': 'still:ContentIdentifier',
    b'apple-metadata': 'still:apple-metadata',
}

def scan_markers(path, head=2_000_000):
    found = []
    with open(path, 'rb') as fh:
        blob = fh.read(head)
    for m, label in MARKERS.items():
        if m in blob:
            found.append(label)
    return found, blob


def video_codec(blob):
    hits = [c.decode() for c in (b'hvc1', b'hev1', b'avc1', b'avc3', b'vp09', b'ap4h') if c in blob]
    return hits


if sample:
    still, mov = sample
    print("\n-- sample pair --")
    for p in (still, mov):
        print(f"{os.path.basename(p)}  {os.path.getsize(p)/1024:.0f} KB")
    sm, sblob = scan_markers(still)
    mm, mblob = scan_markers(mov, head=4_000_000)
    print("still markers:", sm)
    print("mov   markers:", mm)
    print("mov   codec  :", video_codec(mblob))
    print("still brand  :", sblob[4:12])
    print("mov   brand  :", mblob[4:12])
    # extract the content identifier value if present
    m = re.search(rb'com\.apple\.quicktime\.content\.identifier', mblob)
    if m:
        print("mov id ctx:", mblob[m.start():m.start()+120])
    m = re.search(rb'ContentIdentifier', sblob)
    if m:
        print("still id ctx:", sblob[max(0, m.start()-60):m.start()+90])

# --- HEIC decode check ------------------------------------------------
print("\n-- HEIC decode capability --")
try:
    import pillow_heif
    from PIL import Image
    pillow_heif.register_heif_opener()
    heics = [f for f in files if os.path.splitext(f)[1].lower() in ('.heic', '.heif')]
    if heics:
        with Image.open(heics[0]) as im:
            print("pillow_heif OK ->", im.size, im.mode, "exif bytes:", len(im.info.get('exif', b'')))
            ex = im.getexif()
            print("exif keys:", list(ex.keys())[:20])
except Exception as e:
    print("pillow_heif FAILED:", e)

# --- folder shape -----------------------------------------------------
print("\n-- layout --")
for d in sorted({os.path.dirname(f) for f in files})[:10]:
    n = len([f for f in files if os.path.dirname(f) == d])
    print(f"  {n:5d}  {d}")

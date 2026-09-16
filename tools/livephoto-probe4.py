"""Minimal ISO-BMFF (MOV/MP4) box parser focused on Live Photo specifics."""
import os, struct, sys, collections

def parse_boxes(buf, start, end, depth=0, out=None, path=""):
    if out is None:
        out = []
    pos = start
    while pos + 8 <= end:
        size = struct.unpack('>I', buf[pos:pos+4])[0]
        typ = buf[pos+4:pos+8].decode('latin1')
        hdr = 8
        if size == 1:
            size = struct.unpack('>Q', buf[pos+8:pos+16])[0]
            hdr = 16
        elif size == 0:
            size = end - pos
        if size < hdr:
            break
        out.append((depth, typ, pos, size, path + "/" + typ))
        child = path + "/" + typ
        if typ in ('moov', 'trak', 'mdia', 'minf', 'stbl', 'udta', 'meta', 'ilst', 'moov/udta'):
            inner = pos + hdr
            if typ == 'meta':
                inner += 4  # version/flags
            parse_boxes(buf, inner, pos + size, depth + 1, out, child)
        pos += size
    return out


def analyse(path):
    with open(path, 'rb') as fh:
        buf = fh.read()
    boxes = parse_boxes(buf, 0, len(buf))
    info = {'file': os.path.basename(path), 'size': len(buf)}
    # mvhd
    for d, t, p, s, cp in boxes:
        if t == 'mvhd':
            ver = buf[p+8]
            if ver == 0:
                ts, dur = struct.unpack('>II', buf[p+20:p+28])
            else:
                ts, dur = struct.unpack('>IQ', buf[p+28:p+36])
            info['duration_s'] = round(dur / ts, 3)
            info['timescale'] = ts
        if t == 'hdlr':
            info.setdefault('handlers', []).append(buf[p+16:p+20].decode('latin1'))
        if t == 'stsd':
            n = struct.unpack('>I', buf[p+12:p+16])[0]
            entry = p + 16
            if n and entry + 8 <= len(buf):
                fmt = buf[entry+4:entry+8].decode('latin1')
                w, h = struct.unpack('>HH', buf[entry+32:entry+36]) if entry + 36 <= len(buf) else (0, 0)
                info.setdefault('sample_entries', []).append({'format': fmt, 'w': w, 'h': h})
        if t in ('keys', 'ilst'):
            pass
        if t == 'uuid':
            info.setdefault('uuids', []).append(buf[p+8:p+24].hex())
    # still-image-time metadata value
    key = b'com.apple.quicktime.still-image-time'
    i = buf.find(key)
    if i >= 0:
        info['still_image_time_raw'] = buf[i:i+120].hex()
    ci = buf.find(b'com.apple.quicktime.content.identifier')
    if ci >= 0:
        info['content_id_raw'] = buf[ci:ci+160].hex()
    # count metadata keys
    info['has_still_image_time'] = key in buf
    info['has_content_identifier'] = b'com.apple.quicktime.content.identifier' in buf
    return info


targets = sys.argv[1:]
for t in targets:
    try:
        r = analyse(t)
        print("=" * 70)
        for k, v in r.items():
            print(f"  {k}: {v}")
    except Exception as e:
        print(f"FAILED {t}: {type(e).__name__} {e}")

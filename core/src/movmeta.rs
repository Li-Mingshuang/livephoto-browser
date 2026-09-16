//! MOV/MP4 容器元数据解析（纯 Rust，不依赖 Media Foundation）。
//!
//! 只需要几件事，所以刻意做成"轻量头部扫描"，避免为了拿元数据去建 MF 解码链：
//! - `mvhd`：时长
//! - 视频轨 `stsd`：编码 4CC（`hvc1`/`hevc`/`avc1`…）与宽高
//! - 标记扫描：`com.apple.quicktime.still-image-time` / `content.identifier` /
//!   `live-photo-info` —— 用来判断一个孤立视频是不是"剧照丢了"的 Live Photo 视频
//!
//! 实测（`core/src/bin/bench_mf.rs` 同一批文件）：Live Photo 的 MOV 是
//! `hvc1` 1920×1440、时长 1.8~2.7 秒，且带 `still-image-time` 与 `live-photo-info`。

use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default)]
pub struct MovMeta {
    pub duration_ms: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub has_still_image_time: bool,
    pub has_content_identifier: bool,
    pub has_live_photo_info: bool,
    /// 文件里出现的 `com.apple.quicktime.*` 键名，便于排查新机型的新字段
    pub apple_keys: Vec<String>,
}

impl MovMeta {
    pub fn is_live_video(&self) -> bool {
        self.has_still_image_time || self.has_live_photo_info
    }

    pub fn is_hevc(&self) -> bool {
        matches!(self.codec.as_str(), "hvc1" | "hev1" | "hevc")
    }
}

const KEY_STILL_IMAGE_TIME: &[u8] = b"com.apple.quicktime.still-image-time";
const KEY_CONTENT_ID: &[u8] = b"com.apple.quicktime.content.identifier";
const KEY_LIVE_INFO: &[u8] = b"com.apple.quicktime.live-photo-info";
const APPLE_PREFIX: &[u8] = b"com.apple.quicktime.";

/// 单次读取窗口大小。Live Photo 的 MOV 通常 2~7MB，一次读完即可；
/// 大文件则读头尾两段（`moov` 可能在文件末尾）。
const WINDOW: u64 = 8 * 1024 * 1024;

pub fn read(path: &Path) -> Result<MovMeta> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("打不开 {}", path.display()))?;
    let len = file.metadata()?.len();

    let mut buf: Vec<u8> = Vec::new();
    if len <= WINDOW * 2 {
        std::io::Read::read_to_end(&mut file, &mut buf)?;
    } else {
        use std::io::{Read, Seek, SeekFrom};
        let mut head = vec![0u8; WINDOW as usize];
        file.read_exact(&mut head)?;
        let mut tail = vec![0u8; WINDOW as usize];
        file.seek(SeekFrom::End(-(WINDOW as i64)))?;
        file.read_exact(&mut tail)?;
        buf = head;
        buf.extend_from_slice(&tail);
    }

    let mut meta = MovMeta::default();

    meta.has_still_image_time = find(&buf, KEY_STILL_IMAGE_TIME).is_some();
    meta.has_content_identifier = find(&buf, KEY_CONTENT_ID).is_some();
    meta.has_live_photo_info = find(&buf, KEY_LIVE_INFO).is_some();
    meta.apple_keys = collect_apple_keys(&buf);

    // 解析 moov 里的 mvhd 与视频轨 stsd
    if let Some(moov) = find_box(&buf, 0, buf.len(), b"moov") {
        if let Some(mvhd) = find_box(&buf, moov.0, moov.1, b"mvhd") {
            meta.duration_ms = parse_mvhd(&buf, mvhd.0, mvhd.1);
        }
        // 逐个 trak 找视频轨
        let mut pos = moov.0;
        while pos + 8 <= moov.1 {
            let size = be32(&buf, pos) as usize;
            if size < 8 {
                break;
            }
            if &buf[pos + 4..pos + 8] == b"trak" {
                if let Some((w, h, codec)) = parse_trak_video(&buf, pos + 8, pos + size) {
                    meta.width = w;
                    meta.height = h;
                    meta.codec = codec;
                    break;
                }
            }
            pos += size;
        }
    }

    Ok(meta)
}

/// 只做标记扫描，不解析容器（用于快速分类大量视频）。
pub fn scan_markers_only(path: &Path, window: u64) -> Result<MovMeta> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; window as usize];
    let n = std::io::Read::read(&mut file, &mut buf)?;
    buf.truncate(n);
    Ok(MovMeta {
        has_still_image_time: find(&buf, KEY_STILL_IMAGE_TIME).is_some(),
        has_content_identifier: find(&buf, KEY_CONTENT_ID).is_some(),
        has_live_photo_info: find(&buf, KEY_LIVE_INFO).is_some(),
        ..Default::default()
    })
}

/// 解析 `mvhd`。`start` 指向**体起点**（`find_box` 已去掉 8 字节 box 头）。
///
/// mvhd 体布局：
/// - v0: version+flags(4) creation(4) modification(4) timescale(4) duration(4)
/// - v1: version+flags(4) creation(8) modification(8) timescale(4) duration(8)
fn parse_mvhd(buf: &[u8], start: usize, end: usize) -> u64 {
    let limit = end.min(buf.len());
    if start + 4 > limit {
        return 0;
    }
    let version = buf[start];
    let (timescale, duration) = if version == 1 {
        if start + 32 > limit {
            return 0;
        }
        (be32(buf, start + 20) as u64, be64(buf, start + 24))
    } else {
        if start + 20 > limit {
            return 0;
        }
        (be32(buf, start + 12) as u64, be32(buf, start + 16) as u64)
    };
    if timescale == 0 {
        return 0;
    }
    duration * 1000 / timescale
}

/// 解析单个 `trak`，若是视频轨则返回 (宽, 高, 4CC)。
fn parse_trak_video(buf: &[u8], start: usize, end: usize) -> Option<(u32, u32, String)> {
    let mdia = find_box(buf, start, end, b"mdia")?;
    let hdlr = find_box(buf, mdia.0, mdia.1, b"hdlr")?;
    // hdlr 体：version+flags(4) pre_defined(4) handler_type(4) ...
    // 注意 handler_type 在**体偏移 8**处（不是 16 —— 16 是从 box 起点算的）。
    if hdlr.0 + 12 > hdlr.1 {
        return None;
    }
    let handler = &buf[hdlr.0 + 8..hdlr.0 + 12];
    if handler != b"vide" {
        return None;
    }

    let minf = find_box(buf, mdia.0, mdia.1, b"minf")?;
    let stbl = find_box(buf, minf.0, minf.1, b"stbl")?;
    let stsd = find_box(buf, stbl.0, stbl.1, b"stsd")?;

    // stsd 体：version+flags(4) entry_count(4) 然后才是第一个 sample entry。
    // 所以 entry 的起点是 stsd.0 + 8（不是 +16）。
    let entry = stsd.0 + 8;
    if entry + 36 > stsd.1 {
        return None;
    }
    let codec = String::from_utf8_lossy(&buf[entry + 4..entry + 8]).to_string();
    // VisualSampleEntry（相对 entry 起点）：
    //   size(4) type(4) reserved(6) data_ref_index(2) pre_defined(2) reserved(2)
    //   pre_defined[3](12) → width(2) @ +32, height(2) @ +34
    let width = be16(buf, entry + 32) as u32;
    let height = be16(buf, entry + 34) as u32;
    Some((width, height, codec))
}

/// 在 [start, end) 内找第一个类型匹配的子 box，返回其内容区间（去掉头部）。
fn find_box(buf: &[u8], start: usize, end: usize, typ: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = start;
    let end = end.min(buf.len());
    while pos + 8 <= end {
        let mut size = be32(buf, pos) as usize;
        let t = &buf[pos + 4..pos + 8];
        let mut hdr = 8usize;
        if size == 1 {
            if pos + 16 > end {
                return None;
            }
            size = be64(buf, pos + 8) as usize;
            hdr = 16;
        } else if size == 0 {
            size = end - pos;
        }
        if size < hdr || pos + size > buf.len() {
            return None;
        }
        if t == typ {
            return Some((pos + hdr, pos + size));
        }
        pos += size;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn collect_apple_keys(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = find(&buf[pos..], APPLE_PREFIX) {
        let at = pos + i;
        let end = buf[at..]
            .iter()
            .position(|c| !(c.is_ascii_alphanumeric() || *c == b'.' || *c == b'-' || *c == b'_'))
            .map(|e| at + e)
            .unwrap_or(buf.len());
        if end - at >= APPLE_PREFIX.len() && end - at < 96 {
            let key = String::from_utf8_lossy(&buf[at..end]).to_string();
            if !out.contains(&key) {
                out.push(key);
            }
        }
        pos = at + APPLE_PREFIX.len();
        if out.len() > 40 {
            break;
        }
    }
    out
}

fn be16(b: &[u8], o: usize) -> u16 {
    ((b[o] as u16) << 8) | b[o + 1] as u16
}

fn be32(b: &[u8], o: usize) -> u32 {
    ((b[o] as u32) << 24) | ((b[o + 1] as u32) << 16) | ((b[o + 2] as u32) << 8) | b[o + 3] as u32
}

fn be64(b: &[u8], o: usize) -> u64 {
    ((be32(b, o) as u64) << 32) | be32(b, o + 4) as u64
}

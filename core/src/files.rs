//! 目录扫描：**只读元数据，绝不打开文件内容**。
//!
//! 这条纪律是硬性的，原因见 `docs/方案.md` §4.5：
//! iCloud / OneDrive 的照片目录里大量文件是"云端占位"（数据不在本地），
//! 一旦打开就可能是"触发下载"或直接报
//! `0x8007018B The cloud file provider is not running`。
//! 只读目录项元数据则永远是本地可得的，所以哪怕整库都在云上也能秒开网格。
//!
//! 性能上用 `FindFirstFileExW` + `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`：
//! - `FindExInfoBasic` 不返回 8.3 短名（省一次查询）
//! - `LARGE_FETCH` 让内核一次多取几批目录项（大目录明显更快）

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    /// 小写扩展名，不含点
    pub ext: String,
    pub size: u64,
    /// Unix 秒（取最后写入时间）
    pub mtime: i64,
    /// 完整的 dwFileAttributes（含高位标志）
    pub attrs: u32,
    pub is_dir: bool,
}

/// 云端占位相关的属性位。
pub const ATTR_OFFLINE: u32 = 0x1000; // FILE_ATTRIBUTE_OFFLINE
pub const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000; // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
pub const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000; // FILE_ATTRIBUTE_RECALL_ON_OPEN
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_SPARSE_FILE: u32 = 0x200;
pub const ATTR_REPARSE_POINT: u32 = 0x400;

impl FileEntry {
    /// 数据是否只在云端（本地无内容）。
    ///
    /// 实测本机 iCloud 相册目录：`attrs = 0x401620`
    /// = Archive | SparseFile | ReparsePoint | Offline | RecallOnDataAccess。
    pub fn is_cloud_placeholder(&self) -> bool {
        self.attrs & (ATTR_OFFLINE | ATTR_RECALL_ON_DATA_ACCESS | ATTR_RECALL_ON_OPEN) != 0
    }
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub dirs_scanned: usize,
    pub files: usize,
    pub placeholders: usize,
    pub ms: f64,
    /// 达到 max_files 上限而提前停止
    pub truncated: bool,
    pub errors: Vec<String>,
}

/// 递归扫描目录树。
///
/// `max_files = 0` 表示不限制。
pub fn scan_tree(root: &Path, max_files: usize, out: &mut Vec<FileEntry>) -> ScanStats {
    let t0 = std::time::Instant::now();
    let mut stats = ScanStats::default();

    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if max_files > 0 && out.len() >= max_files {
            stats.truncated = true;
            break;
        }
        stats.dirs_scanned += 1;
        match read_dir_entries(&dir) {
            Ok(items) => {
                for it in items {
                    if it.is_dir {
                        // 跳过隐藏目录与回收站类目录，避免无意义遍历
                        if !it.name.starts_with('.') && !it.name.eq_ignore_ascii_case("$RECYCLE.BIN") {
                            stack.push(it.path.clone());
                        }
                        continue;
                    }
                    if it.is_cloud_placeholder() {
                        stats.placeholders += 1;
                    }
                    out.push(it);
                    stats.files += 1;
                    if max_files > 0 && out.len() >= max_files {
                        stats.truncated = true;
                        break;
                    }
                }
            }
            Err(e) => stats.errors.push(format!("{}: {e}", dir.display())),
        }
    }

    stats.ms = t0.elapsed().as_secs_f64() * 1000.0;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    stats
}

/// 列出一个目录下的所有条目（含子目录，便于调用方决定是否继续下钻）。
#[cfg(windows)]
pub fn read_dir_entries(dir: &Path) -> anyhow::Result<Vec<FileEntry>> {
    use std::ffi::c_void;

    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        FIND_FIRST_EX_FLAGS, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
    };

    let pattern = extended_path(&dir.join("*"));
    let mut data = WIN32_FIND_DATAW::default();

    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut _ as *mut c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_FLAGS(FIND_FIRST_EX_LARGE_FETCH.0),
        )
    };

    let handle = match handle {
        Ok(h) => h,
        Err(e) => {
            // 空目录会以 ERROR_FILE_NOT_FOUND 返回，不算错误
            let code = e.code().0 as u32;
            if code == 0x8007_0002 {
                return Ok(Vec::new());
            }
            anyhow::bail!("{e}");
        }
    };

    let mut items = Vec::new();
    loop {
        let name = utf16_to_string(&data.cFileName);
        if !name.is_empty() && name != "." && name != ".." {
            let attrs = data.dwFileAttributes;
            let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
            items.push(FileEntry {
                path: dir.join(&name),
                ext: extension_of(&name),
                name,
                size,
                mtime: filetime_to_unix(data.ftLastWriteTime),
                attrs,
                is_dir: attrs & ATTR_DIRECTORY != 0,
            });
        }
        data = WIN32_FIND_DATAW::default();
        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            break;
        }
    }
    unsafe {
        let _ = FindClose(handle);
    }
    Ok(items)
}

#[cfg(not(windows))]
pub fn read_dir_entries(dir: &Path) -> anyhow::Result<Vec<FileEntry>> {
    let mut items = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let md = e.metadata()?;
        let name = e.file_name().to_string_lossy().to_string();
        items.push(FileEntry {
            path: e.path(),
            ext: extension_of(&name),
            name,
            size: md.len(),
            mtime: 0,
            attrs: 0,
            is_dir: md.is_dir(),
        });
    }
    Ok(items)
}

/// 转成 `\\?\` 扩展路径，绕开 260 字符限制（相机导出的目录名往往很长）。
#[cfg(windows)]
fn extended_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let s = p.as_os_str();
    let mut wide: Vec<u16> = s.encode_wide().collect();
    let already = wide.len() >= 4 && wide[0] == b'\\' as u16 && wide[1] == b'\\' as u16
        && (wide[2] == b'?' as u16 || wide[2] == b'.' as u16);
    if !already && p.is_absolute() {
        let mut prefixed: Vec<u16> = r"\\?\".encode_utf16().collect();
        prefixed.extend_from_slice(&wide);
        wide = prefixed;
    }
    wide.push(0);
    wide
}

#[cfg(windows)]
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(windows)]
fn filetime_to_unix(ft: windows::Win32::Foundation::FILETIME) -> i64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    if ticks == 0 {
        return 0;
    }
    // FILETIME 是 1601-01-01 起的 100ns 计数；换算到 Unix 纪元
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    ((ticks.saturating_sub(EPOCH_DIFF)) / 10_000_000) as i64
}

pub fn extension_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// 文件名主干（去掉最后一个扩展名）。
pub fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

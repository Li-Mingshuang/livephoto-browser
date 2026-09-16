//! 缩略图磁盘缓存。
//!
//! 设计要点：
//! - **缓存键 = blake3(路径 + mtime + size + 层级 + 编码器版本)**。
//!   文件内容变了、或我们改了缩略图参数，键自然失效，不需要任何手动清理逻辑。
//! - 缓存是**纯派生数据**：整目录删掉只损失时间，不损失任何照片。
//! - M1 不引入数据库：命中判断就是一次文件存在性检查（NTFS 元数据很快），
//!   这样少一个组件、少一处不一致。等库大到扫描变慢再上 SQLite。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// 编码器版本：改了缩略图编码参数就把它 +1，让旧缓存自动失效。
const ENCODER_VERSION: u32 = 1;

#[derive(Serialize, Clone, Debug, Default)]
pub struct CacheStats {
    pub files: usize,
    pub bytes: u64,
}

#[derive(Clone)]
pub struct ThumbCache {
    root: PathBuf,
}

impl ThumbCache {
    /// 默认根目录：`%LOCALAPPDATA%\LivePhoto\cache\v1`
    pub fn open_default() -> Result<Self> {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let root = base.join("LivePhoto").join("cache").join("v1");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建缓存目录失败：{}", root.display()))?;
        Ok(Self { root })
    }

    pub fn with_root(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 计算缓存键。
    #[allow(clippy::too_many_arguments)]
    pub fn key(
        tier: &str,
        still_path: Option<&str>,
        still_mtime: i64,
        still_size: u64,
        video_path: Option<&str>,
        video_mtime: i64,
        video_size: u64,
    ) -> String {
        let mut h = blake3::Hasher::new();
        h.update(tier.as_bytes());
        h.update(b"|");
        h.update(&ENCODER_VERSION.to_le_bytes());
        for (p, mt, sz) in [
            (still_path, still_mtime, still_size),
            (video_path, video_mtime, video_size),
        ] {
            match p {
                Some(p) => {
                    h.update(p.as_bytes());
                    h.update(&mt.to_le_bytes());
                    h.update(&sz.to_le_bytes());
                }
                None => {
                    h.update(b"-");
                }
            }
            h.update(b"|");
        }
        h.finalize().to_hex()[..32].to_string()
    }

    fn path_for(&self, tier: &str, key: &str) -> PathBuf {
        // 两级分片，避免单目录文件过多
        self.root
            .join(tier)
            .join(&key[0..2])
            .join(format!("{key}.jpg"))
    }

    pub fn get(&self, tier: &str, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.path_for(tier, key)).ok()
    }

    pub fn has(&self, tier: &str, key: &str) -> bool {
        self.path_for(tier, key).is_file()
    }

    pub fn put(&self, tier: &str, key: &str, bytes: &[u8]) -> Result<()> {
        let p = self.path_for(tier, key);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // 先写临时文件再改名：避免并发读到一个写了一半的文件
        let tmp = p.with_extension("jpg.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// 统计缓存占用（只统计 `.jpg`，递归一层分片目录）。
    pub fn stats(&self) -> CacheStats {
        let mut s = CacheStats::default();
        for tier_entry in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            if !tier_entry.path().is_dir() {
                continue;
            }
            for shard in std::fs::read_dir(tier_entry.path()).into_iter().flatten().flatten() {
                for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                    if let Ok(md) = f.metadata() {
                        if md.is_file() {
                            s.files += 1;
                            s.bytes += md.len();
                        }
                    }
                }
            }
        }
        s
    }

    /// 列出所有缓存文件：`(路径, 修改时间(unix 秒))`
    fn list_files(&self) -> Vec<(PathBuf, i64)> {
        let mut out = Vec::new();
        for tier in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            if !tier.path().is_dir() {
                continue;
            }
            for shard in std::fs::read_dir(tier.path()).into_iter().flatten().flatten() {
                for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
                    let Ok(md) = f.metadata() else { continue };
                    if !md.is_file() {
                        continue;
                    }
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    out.push((f.path(), mtime));
                }
            }
        }
        out
    }

    /// 按"最旧优先"把缓存压到上限以内，并顺带清理残留的 `.tmp`。
    ///
    /// 为什么必须做：全屏层（2048px）单张约 700KB，按当前比例 1 万张就是 **7GB** ——
    /// 缓存是纯派生数据、可以随便删，但不该无限长。
    /// 用 mtime 近似 LRU；不刷新命中文件的 mtime（那会让每次读都写盘），
    /// 所以实际语义是"最近写入的优先保留"，对缩略图这种"写入即被看"的场景足够。
    pub fn enforce_limit(&self, max_bytes: u64) -> EvictStats {
        let mut st = EvictStats::default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let files = self.list_files();
        let mut real: Vec<(PathBuf, i64, u64)> = Vec::new();
        for (p, mtime) in files {
            if p.extension().map(|e| e == "tmp").unwrap_or(false) {
                if now - mtime > 300 && std::fs::remove_file(&p).is_ok() {
                    st.removed_tmp += 1;
                }
                continue;
            }
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            real.push((p, mtime, size));
        }

        let total: u64 = real.iter().map(|f| f.2).sum();
        st.bytes_before = total;
        if total <= max_bytes {
            st.bytes_after = total;
            st.files_kept = real.len();
            return st;
        }

        // 从最旧的开始删，但跳过最近 2 分钟写入的（可能还在被读）
        real.sort_by_key(|f| f.1);
        let mut cur = total;
        for (p, mtime, size) in real.iter() {
            if cur <= max_bytes {
                break;
            }
            if now - mtime < 120 {
                continue;
            }
            if std::fs::remove_file(p).is_ok() {
                cur = cur.saturating_sub(*size);
                st.removed += 1;
                st.bytes_freed += size;
            }
        }
        st.bytes_after = cur;
        st.files_kept = real.len().saturating_sub(st.removed as usize);
        st
    }
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct EvictStats {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub bytes_freed: u64,
    pub removed: u64,
    pub removed_tmp: u64,
    pub files_kept: usize,
}

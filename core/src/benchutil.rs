//! 基准测试的公共设施：统计、结果落盘、样本收集。

use std::path::{Path, PathBuf};
use std::time::Instant;

/// 一次测量的原始样本（毫秒）。
#[derive(Default)]
pub struct Samples {
    pub name: String,
    pub ms: Vec<f64>,
}

impl Samples {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ms: Vec::new(),
        }
    }

    pub fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let t0 = Instant::now();
        let out = f();
        self.ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        out
    }

    pub fn push(&mut self, ms: f64) {
        self.ms.push(ms);
    }

    pub fn summary(&self) -> serde_json::Value {
        let mut v = self.ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        let pct = |p: f64| -> f64 {
            if n == 0 {
                return 0.0;
            }
            let i = (((p / 100.0) * (n as f64 - 1.0)).round() as usize).min(n - 1);
            v[i]
        };
        let mean = if n == 0 { 0.0 } else { v.iter().sum::<f64>() / n as f64 };
        serde_json::json!({
            "name": self.name,
            "n": n,
            "mean_ms": round2(mean),
            "p50_ms": round2(pct(50.0)),
            "p90_ms": round2(pct(90.0)),
            "p99_ms": round2(pct(99.0)),
            "min_ms": round2(v.first().copied().unwrap_or(0.0)),
            "max_ms": round2(v.last().copied().unwrap_or(0.0)),
        })
    }
}

pub fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// 结果目录：<repo>/m0-results（可通过 LIVEPHOTO_RESULTS 覆盖）。
pub fn results_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LIVEPHOTO_RESULTS") {
        return PathBuf::from(p);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("m0-results");
    p
}

pub fn write_result(kind: &str, value: &serde_json::Value) -> anyhow::Result<PathBuf> {
    let dir = results_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{kind}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)?;
    Ok(path)
}

/// 递归收集指定扩展名的文件（大小写不敏感），最多 limit 个。
pub fn collect_files(root: &Path, exts: &[&str], limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    stack.push(p);
                }
            } else if ft.is_file() {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if exts.contains(&ext.as_str()) {
                    out.push(p);
                    if limit > 0 && out.len() >= limit {
                        return out;
                    }
                }
            }
        }
    }
    out.sort();
    out
}

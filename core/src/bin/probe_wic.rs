//! 逐步骤打点的 WIC 解码探针，用于定位解码在哪一步阻塞。
//!
//! 用法： probe_wic <file> [max_dim|none] [mta|sta] [mf]

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use livephoto_core::media::{com_init, com_init_sta, wic};

fn step(msg: &str) {
    println!("[{}] {msg}", now_ms());
    let _ = std::io::stdout().flush();
}

fn now_ms() -> u128 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let s = START.get_or_init(Instant::now);
    s.elapsed().as_millis()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| r"F:\DCIM".into()));
    let max_dim: Option<u32> = args
        .get(2)
        .filter(|s| s.as_str() != "none")
        .and_then(|s| s.parse().ok());
    let apartment = args.get(3).cloned().unwrap_or_else(|| "mta".into());
    let use_mf = args.iter().any(|a| a == "mf");

    step(&format!(
        "start, file = {}, max_dim = {max_dim:?}, apartment = {apartment}, mf = {use_mf}",
        path.display()
    ));

    if use_mf {
        use windows::Win32::Media::MediaFoundation::{MFStartup, MF_VERSION, MFSTARTUP_FULL};
        step("MFStartup");
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
    }

    if apartment == "sta" {
        step("com_init_sta");
        com_init_sta()?;
    } else {
        step("com_init (MTA)");
        com_init()?;
    }

    step("WicFactory::new");
    let factory = wic::WicFactory::new()?;

    step("WicFactory::size (只读头部元数据)");
    match factory.size(&path) {
        Ok((w, h)) => step(&format!("  size = {w}x{h}")),
        Err(e) => step(&format!("  size 失败: {e:#}")),
    }

    step(&format!("decode(max_dim = {max_dim:?})"));
    let t0 = Instant::now();
    match factory.decode(&path, max_dim, wic::PixelLayout::Rgb24) {
        Ok(b) => {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            step(&format!(
                "  解码成功 {}x{} {} bytes 用时 {:.1}ms",
                b.w,
                b.h,
                b.bytes(),
                ms
            ));
            let t1 = Instant::now();
            match b.encode_jpeg(82) {
                Ok(jpeg) => step(&format!(
                    "  JPEG 编码 {} bytes 用时 {:.1}ms",
                    jpeg.len(),
                    t1.elapsed().as_secs_f64() * 1000.0
                )),
                Err(e) => step(&format!("  JPEG 编码失败: {e:#}")),
            }
        }
        Err(e) => step(&format!("  解码失败: {e:#}")),
    }

    step("done");
    Ok(())
}

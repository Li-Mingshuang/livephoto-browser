//! LivePhoto 浏览器核心层。
//!
//! 刻意不依赖 Tauri：所有扫描/配对/解码/缓存/推理逻辑都在这里，
//! 这样 bench 和 CLI 工具可以秒级编译，UI 层只做展示与交互。

pub mod benchutil;
pub mod cache;
pub mod files;
pub mod media;
pub mod movmeta;
pub mod pairing;
pub mod thumbs;

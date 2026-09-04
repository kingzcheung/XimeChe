//! 剪贴板 / 快捷发送本地存储与系统剪贴板监听（移植自 macOS 版 `ximeyi-clipboard`）。
//!
//! - `store`：SQLite 存储，表结构与 Android 版完全对齐（`clipboard_entries`，
//!   `PRAGMA user_version = 3`），db 文件可跨端互换；
//! - `watcher`：Wayland 剪贴板监听（data-control 协议），替代 macOS 的
//!   `NSPasteboard.changeCount` 轮询。

pub mod store;
pub mod watcher;

use std::path::PathBuf;

/// 剪贴板数据库目录（`~/.config/xime`）。
pub fn default_db_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".config/xime")
}

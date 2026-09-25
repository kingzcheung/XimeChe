//! 剪贴板同步桥：本地剪贴板（`xime-clipboard`）↔ `clipboard_sync` Lua 插件。
//!
//! 对齐 Android 版 `ClipboardSyncBridge` 的语义：
//! - **push**：watcher 捕获文本 → hash 去重 → `push(profile)` 给所有已启用插件；
//! - **pull**：daemon 启动与打开剪贴板面板时各拉取一次 → 回声抑制（跳过自己刚
//!   推送的内容）→ 其余经 `upsert_and_trim` 入库（存储层按文本去重/置顶/裁剪）；
//! - **独立线程**：Lua 插件的网络请求（host.http，20s 超时）不阻塞按键处理。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;

use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};
use xime_plugin::{PluginManager, PluginManifest, PluginRuntime};

use xime_clipboard::store::ClipboardStore;

/// 同步插件加载描述（运行时在桥线程内创建，规避 Lua state 跨线程约束）。
#[derive(Debug, Clone)]
pub struct SyncPluginDescriptor {
    pub id: String,
    pub manifest: PluginManifest,
    pub plugin_dir: PathBuf,
    pub config_file: PathBuf,
}

/// 同步桥消息。
pub enum SyncMessage {
    /// watcher 捕获到文本（push 触发）。
    Captured(String),
    /// 请求拉取（daemon 启动 / 打开剪贴板面板）。
    Pull,
    /// 重载插件（安装/卸载/启停后经 DBus 通知）。
    Reload(Vec<SyncPluginDescriptor>),
}

/// 文本 SHA-256 摘要（hex 小写，对齐 Android ClipboardProfile.hash）。
pub fn text_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 构造 profile JSON（字段对齐 Android `ClipboardProfile`，snake_case）。
fn profile_json(text: &str, hash: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "text",
        "hash": hash,
        "text": text,
        "size": text.len(),
        "source": "linux",
    })
}

/// 扫描已启用的 clipboard_sync 插件（daemon 启动 / ReloadPlugins 时调用）。
pub fn scan_descriptors() -> Vec<SyncPluginDescriptor> {
    let root = crate::plugin_host::plugins_dir();
    let manager = PluginManager::new(&root);
    manager
        .list()
        .into_iter()
        .filter(|r| r.enabled)
        .filter_map(|record| {
            let manifest = match manager.load_manifest(&record.id) {
                Ok(m) if m.plugin_type() == xime_plugin::PluginType::ClipboardSync => m,
                Ok(_) => return None,
                Err(e) => {
                    warn!("Sync scan: manifest of '{}' failed: {e}", record.id);
                    return None;
                }
            };
            Some(SyncPluginDescriptor {
                id: record.id.clone(),
                plugin_dir: manager.plugin_dir(&record.id),
                config_file: manager.config_path(&record.id),
                manifest,
            })
        })
        .collect()
}

/// 桥线程状态。
struct SyncState {
    store: Arc<ClipboardStore>,
    runtimes: HashMap<String, PluginRuntime>,
    /// 最近一次成功推送的 hash（push 去重 + pull 回声抑制）。
    last_pushed_hash: Option<String>,
}

impl SyncState {
    fn handle_captured(&mut self, text: &str) {
        if text.is_empty() || self.runtimes.is_empty() {
            return;
        }
        let hash = text_hash(text);
        if self.last_pushed_hash.as_deref() == Some(hash.as_str()) {
            debug!("Sync push: identical to last pushed, skipping");
            return;
        }
        let profile = profile_json(text, &hash);
        let mut pushed = false;
        for (id, runtime) in &self.runtimes {
            if runtime.clipboard_push(&profile) {
                pushed = true;
                debug!("Sync pushed to '{}' ({} chars)", id, text.chars().count());
            } else {
                debug!("Sync push to '{}' failed/unsupported", id);
            }
        }
        if pushed {
            self.last_pushed_hash = Some(hash);
        }
    }

    fn handle_pull(&mut self) {
        if self.runtimes.is_empty() {
            return;
        }
        for (id, runtime) in &self.runtimes {
            let Some(value) = runtime.clipboard_pull() else {
                debug!("Sync pull from '{}': no change", id);
                continue;
            };
            let profiles: Vec<serde_json::Value> = match value {
                serde_json::Value::Array(items) => items,
                v @ serde_json::Value::Object(_) => vec![v],
                _ => continue,
            };
            for profile in profiles {
                let Some(text) = profile.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                // 回声抑制：自己刚推送的内容不写回本地
                let remote_hash = profile
                    .get("hash")
                    .and_then(|h| h.as_str())
                    .map(str::to_owned);
                if remote_hash.as_deref() == self.last_pushed_hash.as_deref() {
                    debug!("Sync pull: echo of own push, skipping");
                    continue;
                }
                if let Err(e) = self
                    .store
                    .upsert_and_trim(text, xime_clipboard::store::now_millis())
                {
                    error!("Sync pull: store failed: {e}");
                } else {
                    debug!("Sync pulled from '{}': {} chars", id, text.chars().count());
                }
            }
        }
    }

    fn handle_reload(&mut self, descriptors: Vec<SyncPluginDescriptor>) {
        for runtime in self.runtimes.values() {
            runtime.call_on_unload();
        }
        self.runtimes.clear();
        let mut loaded = 0;
        for d in &descriptors {
            match PluginRuntime::load(&d.plugin_dir, &d.manifest.entry, &d.config_file) {
                Ok(runtime) => {
                    runtime.call_on_load();
                    self.runtimes.insert(d.id.clone(), runtime);
                    loaded += 1;
                }
                Err(e) => error!("Sync plugin '{}' load failed: {e}", d.id),
            }
        }
        info!(
            "Sync bridge: {}/{} clipboard_sync plugins loaded",
            loaded,
            descriptors.len()
        );
    }
}

/// 启动同步桥线程（进程生命周期内常驻）。
pub fn spawn_bridge(store: Arc<ClipboardStore>, rx: Receiver<SyncMessage>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("xime-clipboard-sync".into())
        .spawn(move || {
            let mut state = SyncState {
                store,
                runtimes: HashMap::new(),
                last_pushed_hash: None,
            };
            while let Ok(msg) = rx.recv() {
                match msg {
                    SyncMessage::Captured(text) => state.handle_captured(&text),
                    SyncMessage::Pull => state.handle_pull(),
                    SyncMessage::Reload(descriptors) => state.handle_reload(descriptors),
                }
            }
            debug!("Sync bridge: channel closed, exiting");
        })
        .expect("spawn clipboard sync bridge thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 远端模拟插件：push 写入 host.config，pull 读回（沙箱无 io，用 host.config 充当远端）。
    const FAKE_SYNC_LUA: &str = r#"
local plugin = {}
function plugin.push(profile)
  local list = host.json.decode(host.config.get("remote") or "[]")
  table.insert(list, profile)
  host.config.set("remote", host.json.encode(list))
  return true
end
function plugin.pull()
  return host.json.decode(host.config.get("remote") or "[]")
end
function plugin.testConnection() return nil end
function plugin.remoteCount()
  return #host.json.decode(host.config.get("remote") or "[]")
end
return plugin
"#;

    fn test_manifest() -> PluginManifest {
        PluginManifest::parse(
            "id: com.test.sync\nname: Test Sync\nversion: 1.0.0\ntype: clipboard_sync\nentry: main.lua\n",
        )
        .unwrap()
    }

    fn setup() -> (tempfile::TempDir, Arc<ClipboardStore>, SyncState) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ClipboardStore::open(dir.path().join("clipboard.db")).unwrap());
        let state = SyncState {
            store: store.clone(),
            runtimes: HashMap::new(),
            last_pushed_hash: None,
        };
        (dir, store, state)
    }

    fn load_fake_plugin(state: &mut SyncState, dir: &std::path::Path) {
        std::fs::write(dir.join("main.lua"), FAKE_SYNC_LUA).unwrap();
        let manifest = test_manifest();
        let runtime = PluginRuntime::load(dir, &manifest.entry, &dir.join("config.yaml")).unwrap();
        runtime.call_on_load();
        state.runtimes.insert(manifest.id.clone(), runtime);
    }

    /// 读取模拟插件远端（host.config["remote"]）条目数。
    fn remote_count(state: &SyncState) -> usize {
        state
            .runtimes
            .values()
            .next()
            .and_then(|r| r.call_fn::<i64>("remoteCount", ()))
            .unwrap_or(-1) as usize
    }

    #[test]
    fn test_text_hash() {
        assert_eq!(text_hash("hello").len(), 64);
        assert_eq!(text_hash("hello"), text_hash("hello"));
        assert_ne!(text_hash("hello"), text_hash("hello "));
    }

    #[test]
    fn test_profile_json_fields() {
        let hash = text_hash("abc");
        let p = profile_json("abc", &hash);
        assert_eq!(p["type"], "text");
        assert_eq!(p["text"], "abc");
        assert_eq!(p["hash"], hash);
        assert_eq!(p["size"], 3);
        assert_eq!(p["source"], "linux");
    }

    #[test]
    fn test_captured_pushes_and_dedups() {
        let (dir, store, mut state) = setup();
        load_fake_plugin(&mut state, dir.path());

        state.handle_captured("hello");
        assert_eq!(remote_count(&state), 1);
        // 第一次 pull 带回自己的推送（回声抑制）→ 本地不入库
        state.handle_pull();
        assert_eq!(store.count_clipboard().unwrap(), 0, "pull 回声应被抑制");

        // 相同文本再次捕获：push 去重跳过（远端仍只有 1 条）
        state.handle_captured("hello");
        assert_eq!(remote_count(&state), 1, "相同文本不应重复推送");
        state.handle_pull();
        assert_eq!(store.count_clipboard().unwrap(), 0);
    }

    #[test]
    fn test_pull_writes_remote_profiles() {
        let (dir, store, mut state) = setup();
        load_fake_plugin(&mut state, dir.path());

        // 本机推 A
        state.handle_captured("A");
        // 模拟另一台设备写入远端 B：直接调插件 push 一个 B profile
        let b_hash = text_hash("B");
        let profile = profile_json("B", &b_hash);
        if let Some(runtime) = state.runtimes.values().next() {
            assert!(runtime.clipboard_push(&profile));
        }

        // 拉取：A 是回声（跳过），B 应入库
        state.handle_pull();
        let items = store.list_clipboard(10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "B");
    }

    #[test]
    fn test_empty_state_noop() {
        let (_dir, store, mut state) = setup();
        state.handle_captured("hello");
        state.handle_pull();
        state.handle_reload(vec![]);
        assert_eq!(store.count_clipboard().unwrap(), 0);
    }

    #[test]
    fn test_reload_loads_and_unloads() {
        let (dir, _store, mut state) = setup();
        let d = SyncPluginDescriptor {
            id: "com.test.sync".into(),
            manifest: test_manifest(),
            plugin_dir: dir.path().to_path_buf(),
            config_file: dir.path().join("config.yaml"),
        };
        std::fs::write(dir.path().join("main.lua"), FAKE_SYNC_LUA).unwrap();
        state.handle_reload(vec![d.clone()]);
        state.handle_captured("x");
        assert_eq!(remote_count(&state), 1);
        // 重载为空：运行时清空（无法查询远端），push 无操作
        state.handle_reload(vec![]);
        state.handle_captured("y");
        // 重新加载后 last_pushed_hash 重置 → "y" 可推
        state.handle_reload(vec![d]);
        state.handle_captured("y");
        assert_eq!(remote_count(&state), 2);
    }
}

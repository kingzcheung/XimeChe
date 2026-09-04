use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, warn};
use xime_clipboard::store::ClipboardStore;
use xime_plugin::{
    ClipboardEntryInfo, ClipboardReadApi, EmojiItem, HostApis, PluginManager, PluginRecord,
    PluginRuntime, QuickSendItemInfo, QuickSendReadApi,
};

/// 剪贴板只读 API 适配器（capability: clipboard_read）。
struct ClipboardReadAdapter(Arc<ClipboardStore>);

impl ClipboardReadApi for ClipboardReadAdapter {
    fn recent(&self, limit: usize) -> Vec<ClipboardEntryInfo> {
        self.0
            .list_clipboard(limit as i64)
            .unwrap_or_default()
            .into_iter()
            .take(limit)
            .map(|e| ClipboardEntryInfo {
                text: e.text,
                timestamp: e.timestamp,
                is_pinned: e.is_pinned,
            })
            .collect()
    }
}

/// 快捷发送只读 API 适配器（capability: quick_send_read）。
struct QuickSendReadAdapter(Arc<ClipboardStore>);

impl QuickSendReadApi for QuickSendReadAdapter {
    fn list(&self) -> Vec<QuickSendItemInfo> {
        self.0
            .list_quick_send()
            .unwrap_or_default()
            .into_iter()
            .map(|e| QuickSendItemInfo {
                text: e.text,
                code: e.code,
            })
            .collect()
    }
}

/// 按存储构建宿主 API 集合（供 PluginRuntime 能力门禁注入）。
pub fn host_apis(store: &Arc<ClipboardStore>) -> HostApis {
    HostApis {
        clipboard: Some(Arc::new(ClipboardReadAdapter(store.clone()))),
        quick_send: Some(Arc::new(QuickSendReadAdapter(store.clone()))),
    }
}

/// 插件宿主：加载已启用插件并提供契约查询。
///
/// 目录布局与设置程序一致（libximecore `xime-setup` 的 plugins_dir）：
/// `~/.config/<name>/plugins/`，registry.yaml 记录已安装插件。
pub struct PluginHost {
    /// 已加载的插件运行时（key = 插件 id）。
    runtimes: HashMap<String, PluginRuntime>,
    /// 剪贴板存储（宿主只读 API 数据源）。
    clipboard: Arc<ClipboardStore>,
}

impl PluginHost {
    pub fn new(clipboard: Arc<ClipboardStore>) -> Self {
        let mut host = Self {
            runtimes: HashMap::new(),
            clipboard,
        };
        host.load_enabled_plugins();
        host
    }

    /// 重新扫描插件目录：卸载的移除、新增/启用的加载。安装/卸载后由设置程序经 DBus 通知。
    pub fn reload(&mut self) {
        self.runtimes.clear();
        self.load_enabled_plugins();
    }

    fn load_enabled_plugins(&mut self) {
        let root = plugins_dir();
        let manager = PluginManager::new(&root);
        let records = manager.list();
        if records.is_empty() {
            debug!("No installed plugins in {}", root.display());
            return;
        }

        let mut loaded = 0;
        for record in &records {
            if !record.enabled {
                debug!("Plugin '{}' disabled, skipping", record.id);
                continue;
            }
            match self.load_plugin(&manager, record) {
                Ok(runtime) => {
                    debug!("Loaded plugin '{}' v{}", record.name, record.version);
                    runtime.call_on_load();
                    self.runtimes.insert(record.id.clone(), runtime);
                    loaded += 1;
                }
                Err(e) => {
                    warn!("Failed to load plugin '{}': {}", record.id, e);
                }
            }
        }
        info!(
            "Plugin host: loaded {}/{} enabled plugins",
            loaded,
            records.len()
        );
    }

    fn load_plugin(
        &self,
        manager: &PluginManager,
        record: &PluginRecord,
    ) -> Result<PluginRuntime, String> {
        let manifest = manager
            .load_manifest(&record.id)
            .map_err(|e| format!("manifest: {e}"))?;
        let dir = manager.plugin_dir(&record.id);
        let config_file = manager.config_path(&record.id);
        PluginRuntime::load_with_apis(
            &dir,
            &manifest,
            &config_file,
            xime_plugin::NetworkPolicy::from_manifest(&manifest.network),
            host_apis(&self.clipboard),
        )
        .map_err(|e| format!("runtime: {e}"))
    }

    /// 已加载的 emoji 类插件数量。
    pub fn emoji_plugin_count(&self) -> usize {
        self.runtimes
            .values()
            .filter(|r| !r.get_categories().is_empty())
            .count()
    }

    /// 广播 `text_committed` 下行事件（对齐 Android PluginEventDispatcher）。
    ///
    /// 仅 manifest `capabilities.events` 声明了该事件且实现了 `onPluginEvent`
    /// 的插件会被调用；同步调用，插件网络请求受 host.http 20s 超时约束。
    pub fn emit_text_committed(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let payload = serde_json::json!({ "text": text });
        for (id, runtime) in &self.runtimes {
            if runtime.deliver_event("text_committed", &payload) {
                debug!("text_committed delivered to '{id}'");
            }
        }
    }

    /// 从所有已加载 emoji 插件汇总表情候选。
    pub fn query_emojis(&self, search_text: &str, top_k: usize) -> Vec<EmojiItem> {
        let mut out = Vec::new();
        for runtime in self.runtimes.values() {
            for category in runtime.get_categories() {
                for item in runtime.get_emojis(&category, search_text, top_k) {
                    out.push(item);
                    if out.len() >= top_k {
                        return out;
                    }
                }
            }
        }
        out
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        panic!("PluginHost::default 需要 ClipboardStore，请使用 PluginHost::new(store)")
    }
}

/// 插件目录：`~/.config/<name>/plugins`（与设置程序一致）。
pub fn plugins_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    PathBuf::from(&home)
        .join(".config")
        .join(xime_config::app_metadata().config_dir_name)
        .join("plugins")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugins_dir_default() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let dir = plugins_dir();
        assert_eq!(
            dir,
            PathBuf::from(&home).join(".config/xime/plugins"),
            "plugins dir should default to ~/.config/xime/plugins"
        );
    }

    #[test]
    fn test_plugin_host_empty() {
        // 直接构造空 host（避免读取用户真实插件目录）；store 用内存库
        let store = Arc::new(ClipboardStore::open(":memory:").expect("in-memory store"));
        let host = PluginHost {
            runtimes: HashMap::new(),
            clipboard: store,
        };
        assert_eq!(host.emoji_plugin_count(), 0);
        assert!(host.query_emojis("", 10).is_empty());
    }

    #[test]
    fn test_host_apis_adapters() {
        let store = Arc::new(ClipboardStore::open(":memory:").unwrap());
        store.upsert_and_trim("abc", 1).unwrap();
        store.insert_quick_send("地址", "dz", 2).unwrap();
        let apis = host_apis(&store);
        let clip = apis.clipboard.as_ref().unwrap().recent(5);
        assert_eq!(clip.len(), 1);
        assert_eq!(clip[0].text, "abc");
        let qs = apis.quick_send.as_ref().unwrap().list();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].code, "dz");
    }

    /// 安装一个最小 emoji 插件到临时目录，验证加载与查询。
    #[test]
    fn test_plugin_host_loads_installed_plugin() {
        use std::io::Write;

        let root = std::env::temp_dir().join(format!("xime_plugin_host_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // 构造 .xipk 并安装
        let xipk = root.join("test.xipk");
        let file = std::fs::File::create(&xipk).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("manifest.yaml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            "id: com.example.kaomoji\nname: Test Emoji\nversion: 1.0.0\ntype: emoji\nentry: main.lua\n"
                .as_bytes(),
        )
        .unwrap();
        zip.start_file("main.lua", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            "local plugin = {}\nfunction plugin.getCategories() return { \"颜文字\" } end\nfunction plugin.getEmojis(category, searchText, topK)\n  local list = { { id=\"k1\", text=\"(ﾟ∀ﾟ)\", category=\"颜文字\" }, { id=\"k2\", text=\"(^u^)\", category=\"颜文字\" } }\n  local out = {}\n  for i, e in ipairs(list) do\n    if searchText == \"\" or string.find(e.text, searchText, 1, true) then table.insert(out, e) end\n    if #out >= topK then break end\n  end\n  return out\nend\nfunction plugin.getCategoryLayoutConfig(category) return { columns = 3 } end\nreturn plugin\n"
                .as_bytes(),
        )
        .unwrap();
        zip.finish().unwrap();

        let manager = xime_plugin::PluginManager::new(&root);
        manager.install_from_zip(&xipk, false).unwrap();

        // 临时目录注入 HOME 不可行，直接构造 host 并手动加载
        let store = Arc::new(ClipboardStore::open(":memory:").unwrap());
        let mut host = PluginHost {
            runtimes: HashMap::new(),
            clipboard: store,
        };
        let records = manager.list();
        assert_eq!(records.len(), 1);
        assert!(records[0].enabled);
        let runtime = host
            .load_plugin(&manager, &records[0])
            .expect("plugin should load");
        host.runtimes.insert(records[0].id.clone(), runtime);

        assert_eq!(host.emoji_plugin_count(), 1);
        let all = host.query_emojis("", 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].text, "(ﾟ∀ﾟ)");
        // 搜索过滤
        let matched = host.query_emojis("u", 10);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].text, "(^u^)");

        std::fs::remove_dir_all(&root).unwrap();
    }
}

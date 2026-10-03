use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;

use tracing::{debug, error, info};
use xime_clipboard::store::ClipboardStore;
use xime_config::XimeConfig;
use xime_tray::{InputMode, TrayManager};
use xime_ui::PanelTheme;
use xime_ui::{PanelGrid, PanelHit, PanelList, PanelListItem, PanelPage, PANEL_MIN_WIDTH};
use xime_wayland::{connect_im_from_fd, connect_im_to_env, ImBackend};
use xime_xkb::XkbContext;
use xime_xkb::{keysym_to_letter, Keysym, ModifierState};

use crate::clipboard_sync::{scan_descriptors, SyncMessage};
use crate::{symbols, DaemonCommand, PluginHost, RimeEngine};

/// 面板路由状态（对齐 XimeYao：菜单为默认页，子页共用面板区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PanelState {
    #[default]
    Closed,
    Open(PanelPage),
}

/// 面板共享数据（daemon 持有权威状态；show 时整份注入 im 层）。
#[derive(Default)]
struct PanelData {
    list: PanelList,
    grid: PanelGrid,
    /// 网格数据源（表情/符号分类全量，refresh_grid_cells 按 tab/page 取窗）。
    grid_source: Vec<(String, Vec<String>)>,
}

/// 快捷发送编码注入状态（对齐 Android quick-send-demo 的"编码命中"语义）。
///
/// Rime 候选存在时：快捷发送条目**追加**在 Rime 当页候选之后（数字键超出
/// Rime 候选数的部分由宿主接管）；Rime 无候选时：完全接管（高亮/翻行/回车）。
#[derive(Debug, Default)]
struct QuickSendInject {
    /// 匹配到的 (code, text) 条目。
    items: Vec<(String, String)>,
    /// Rime 当页候选数（决定数字键归属）。
    rime_count: usize,
    /// Rime 无候选时（接管模式）的本地高亮。
    highlighted: usize,
}

impl QuickSendInject {
    fn active(&self) -> bool {
        !self.items.is_empty()
    }

    fn clear(&mut self) {
        self.items.clear();
        self.rime_count = 0;
        self.highlighted = 0;
    }

    /// 数字键位置（0 起）是否命中快捷发送条目；返回条目索引。
    fn digit_hit(&self, pos: usize) -> Option<usize> {
        pos.checked_sub(self.rime_count)
            .filter(|&i| i < self.items.len())
    }
}

/// 快捷发送编码前缀匹配（跳过无编码条目），最多 `limit` 条。
fn match_quick_send_codes(
    entries: &[(String, String)],
    raw_input: &str,
    limit: usize,
) -> Vec<(String, String)> {
    if raw_input.is_empty() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|(code, _)| code.starts_with(raw_input))
        .take(limit)
        .cloned()
        .collect()
}

/// 从配置构建渲染主题（亮/暗模式 + 字号/圆角/高亮色）。
fn build_theme(config: &XimeConfig, dark: bool) -> PanelTheme {
    PanelTheme::for_mode(
        dark,
        config.style.font_size,
        config.style.corner_radius,
        config.get_primary_color(),
    )
}

/// 决定按键是否转发给应用（孤儿释放抑制）。
/// - Rime 消费的按下（`press_consumed`）不转发；其释放事件也不转发，避免孤儿释放。
/// - 其他按键（未被消费的按下、未被消费的释放）正常转发。
fn should_forward_key(
    pressed: bool,
    result: bool,
    consumed_presses: &std::collections::HashSet<u32>,
    key: u32,
) -> bool {
    let press_consumed = result && pressed;
    let release_of_consumed = !pressed && consumed_presses.contains(&key);
    !press_consumed && !release_of_consumed
}

/// 空格兜底判定（对齐 XimeYao）：组合态 + 无候选时，空格不上屏首选
/// （没有首选可选）而应直接上屏编码本身。
///
/// 返回 Some(编码) = 拦截空格并上屏该文本；None = 交给 Rime 正常处理。
/// 带 Ctrl/Alt/Shift/Super 的空格（启停 IM / 全半角切换 / 窗口快捷键）
/// 一律不拦。
fn space_fallback_input(
    sym: u32,
    modifiers: &ModifierState,
    raw_input: Option<&str>,
    num_candidates: usize,
) -> Option<String> {
    if sym != 0x20 || modifiers.ctrl || modifiers.alt || modifiers.shift || modifiers.super_key {
        return None;
    }
    let input = raw_input?;
    if input.is_empty() || num_candidates > 0 {
        return None;
    }
    Some(input.to_string())
}

/// 最近一次候选窗内容（菜单开/关后重绘用，主题以当前值为准）。
type CandidateCache = (Vec<xime_ui::CandidateItem>, usize);

pub struct WaylandLoop {
    command_rx: Receiver<DaemonCommand>,
    tray: Arc<TrayManager>,
    rt_handle: tokio::runtime::Handle,
    /// 最近一次候选内容缓存（菜单开/关后重绘用）。
    candidate_cache: std::sync::Mutex<Option<CandidateCache>>,
    /// 剪贴板/快捷发送存储。
    clipboard: Arc<ClipboardStore>,
    /// 剪贴板同步桥命令通道。
    sync_tx: std::sync::mpsc::Sender<SyncMessage>,
}

impl WaylandLoop {
    pub fn new(
        command_rx: Receiver<DaemonCommand>,
        tray: Arc<TrayManager>,
        rt_handle: tokio::runtime::Handle,
        clipboard: Arc<ClipboardStore>,
        sync_tx: std::sync::mpsc::Sender<SyncMessage>,
    ) -> Self {
        Self {
            command_rx,
            tray,
            rt_handle,
            candidate_cache: std::sync::Mutex::new(None),
            clipboard,
            sync_tx,
        }
    }

    pub fn run(self) {
        info!("Wayland loop thread started");

        let mut conn: Option<Box<dyn ImBackend>> = None;
        let mut xkb: Option<XkbContext> = None;
        let mut rime = RimeEngine::new();
        let mut plugin_host = PluginHost::new();
        let mut xime_config = XimeConfig::load();
        let _last_key_root_binding = xime_config.get_last_key_root_binding();
        let primary_color = xime_config.get_primary_color();
        self.rt_handle.block_on(async {
            self.tray.set_primary_color(primary_color).await;
        });
        // 渲染主题：配置样式 + 亮/暗模式（暗色由 portal 监听任务推送）。
        let mut dark_mode = false;
        let mut theme = build_theme(&xime_config, dark_mode);
        debug!(
            "Loaded hotkeys: show_key={}, primary_color={:?}",
            xime_config.wubi_radicals.hotkeys.show_key, primary_color
        );

        let mut candidate_window_visible = false;
        let mut panel = PanelData::default();
        let mut quick_send = QuickSendInject::default();
        let mut panel_state = PanelState::Closed;
        let mut last_panel_width: u32 = 0;
        let mut consumed_presses: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut last_input_keysym: Option<u32> = None;
        let mut ctrl_root_visible = false;
        let mut last_ascii_mode = false;
        let mut last_active = false;
        // 输入法启停开关（Ctrl+Space 切换）：停用态按键直接转发，不做任何处理。
        let mut im_enabled = true;
        // 暗色模式切换后待重绘标记（渲染需在 conn 作用域内进行）
        let mut pending_theme_redraw = false;

        // 剪贴板同步桥初始化：加载 clipboard_sync 插件并拉取一次
        let _ = self.sync_tx.send(SyncMessage::Reload(scan_descriptors()));
        let _ = self.sync_tx.send(SyncMessage::Pull);

        // 无 launcher 的会话（GNOME 等）：直接连接 $WAYLAND_DISPLAY 使用 v2 协议。
        // KWin 下普通 socket 不暴露 IM 协议，此步会失败，随后等待 launcher 传入 fd。
        match connect_im_to_env() {
            Ok(backend) => {
                info!("Connected directly to compositor (standalone mode)");
                conn = Some(backend);
            }
            Err(e) => {
                debug!(
                    "Direct connection not available (waiting for launcher fd): {}",
                    e
                );
            }
        }

        loop {
            use std::sync::mpsc::TryRecvError;

            match self.command_rx.try_recv() {
                Ok(DaemonCommand::OpenWaylandSocket(fd, display_name)) => {
                    debug!("Connecting from fd for display {}", display_name);

                    xkb = XkbContext::new().ok();

                    match connect_im_from_fd(fd) {
                        Ok(backend) => {
                            info!("Connected via launcher fd (KWin mode)");
                            // 托盘菜单注入方案切换组（levers 此时可用了）。
                            let schemas = rime.available_schemas();
                            let current = rime.get_current_schema().unwrap_or_default();
                            self.rt_handle
                                .block_on(self.tray.update_schema_menu(schemas, current));
                            conn = Some(backend);
                        }
                        Err(e) => {
                            error!("Failed to connect: {}", e);
                        }
                    }
                }
                Ok(DaemonCommand::ToggleMode) => {
                    debug!("ToggleMode command received");
                    if rime.session().is_some() {
                        let new_ascii = rime.toggle_ascii_mode();
                        last_ascii_mode = new_ascii;
                        let tray_mode = if new_ascii {
                            InputMode::English
                        } else {
                            InputMode::Chinese
                        };
                        self.rt_handle.block_on(async {
                            self.tray.set_mode(tray_mode).await;
                        });
                        debug!("Tray updated after toggle: ascii_mode={}", new_ascii);

                        // IM 未激活时（如旧 Chromium 应用复制后不重新 enable
                        // text-input），键盘事件完全不经过 IM，Shift/Ctrl+Space
                        // 都无效；托盘点击是唯一可达的控制通道，借 KWin 的
                        // forceActivate 强制激活，让本次切换真正生效。
                        if !last_active {
                            debug!("IM inactive on toggle, requesting KWin forceActivate");
                            self.rt_handle.block_on(self.tray.force_activate_im());
                        }
                    }
                }
                Ok(DaemonCommand::Deploy) => {
                    debug!("Deploy command received, starting Rime deployment...");
                    let result = rime.redeploy_with_result();
                    let (summary, body) = match result {
                        librime::DeployResult::Success => {
                            ("部署完成".to_string(), "Rime 配置已重新加载".to_string())
                        }
                        librime::DeployResult::Failure => (
                            "部署失败".to_string(),
                            "Rime 部署返回失败，请检查 xime.log 或配置文件".to_string(),
                        ),
                    };
                    let handle = self.rt_handle.clone();
                    handle.spawn(async move {
                        notify_desktop(&summary, &body).await;
                    });
                }
                Ok(DaemonCommand::ReloadStyle) => {
                    debug!("ReloadStyle command received, reloading xime config...");
                    let new_config = XimeConfig::load();
                    let new_color = new_config.get_primary_color();
                    self.rt_handle.block_on(async {
                        self.tray.set_primary_color(new_color).await;
                    });
                    xime_config = new_config;
                    theme = build_theme(&xime_config, dark_mode);

                    debug!("Style config reloaded, new primary_color={:?}", new_color);
                }
                Ok(DaemonCommand::DarkMode(dark)) => {
                    debug!("DarkMode command received: dark={}", dark);
                    dark_mode = dark;
                    theme = build_theme(&xime_config, dark_mode);
                    // 候选栏可见时标记重绘（渲染需在 conn 作用域内进行）
                    pending_theme_redraw = true;
                }
                Ok(DaemonCommand::ReloadPlugins) => {
                    debug!("ReloadPlugins command received, reloading plugins...");
                    plugin_host.reload();
                    // 同步桥：重载 clipboard_sync 插件并立即拉取一次
                    let _ = self.sync_tx.send(SyncMessage::Reload(scan_descriptors()));
                    let _ = self.sync_tx.send(SyncMessage::Pull);
                }
                Ok(DaemonCommand::SelectSchema(schema_id, result_tx)) => {
                    debug!("SelectSchema command received: {}", schema_id);
                    let ok = rime.select_schema(&schema_id);
                    let _ = result_tx.send(ok);
                    debug!("SelectSchema result: {}", ok);
                    if ok {
                        // 托盘菜单 ✓ 跟随（列表不变，只换选中项）。
                        let schemas = rime.available_schemas();
                        self.rt_handle
                            .block_on(self.tray.update_schema_menu(schemas, schema_id));
                    }
                }
                Ok(DaemonCommand::ListDictEntries(dict, query, result_tx)) => {
                    debug!("ListDictEntries command received: {dict} query={query:?}");
                    // 关会话 → 导出（userdb 独占）→ 重建；期间按键会丢 composition，
                    // 设置页词典浏览与打字互斥（对齐 XimeYao 词典写路径的语义）。
                    let result = rime
                        .with_user_dict_closed(|| crate::user_dict::list_entries(&dict, &query));
                    let _ = result_tx.send(result);
                }
                Ok(DaemonCommand::UserDictOp(op, result_tx)) => {
                    debug!("UserDictOp command received: {op:?}");
                    let result = rime.with_user_dict_closed(|| op.run());
                    let _ = result_tx.send(result);
                }
                Ok(DaemonCommand::Shutdown) => {
                    debug!("Shutdown requested, exiting process with status 0");
                    // 必须整进程退出：DBus 主循环不感知该命令；exit(0) 为正常
                    // 退出，KWin 不会计入 QProcess::CrashExit 崩溃保护。
                    std::process::exit(0);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    debug!("Command channel disconnected");
                    break;
                }
            }

            if let Some(c) = conn.as_mut() {
                if let Err(e) = c.dispatch_events() {
                    debug!("Dispatch error: {}", e);
                    conn = None;
                    self.rt_handle.block_on(async {
                        self.tray.set_visible(false).await;
                    });
                    last_active = false;
                    continue;
                }

                if let Err(e) = c.handle_unavailable() {
                    debug!("handle_unavailable error: {}", e);
                }

                let is_active = c.is_active();

                if is_active != last_active {
                    debug!("State changed: active={}", is_active);
                    // 托盘常驻：失活时不再隐藏图标（fcitx5 风格）。图标是 IM
                    // 未激活时唯一可达的控制入口（键盘事件不经过 IM），藏掉
                    // 会让用户在"卡死"时失去恢复手段。
                    if is_active {
                        self.rt_handle.block_on(async {
                            self.tray.set_visible(true).await;
                        });
                    }
                    last_active = is_active;

                    if !is_active {
                        // 失焦（切换窗口/输入框）时彻底清理 UI 状态：
                        // 立即隐藏候选栏/菜单面板/Ctrl 字根窗口，关闭 emoji 面板，
                        // 清除按键消费记录与字根缓存。
                        // 否则残留的候选栏会一直显示在新输入框上，遮挡并吞掉
                        // 点击事件，导致新输入框无法获得焦点、输入法无法重新激活。
                        c.hide_candidate_window();
                        c.hide_panel();
                        c.hide_root_window();
                        let _ = c.flush();
                        candidate_window_visible = false;
                        quick_send.clear();
                        panel_state = PanelState::Closed;
                        ctrl_root_visible = false;
                        last_input_keysym = None;
                        consumed_presses.clear();
                        continue;
                    }
                }

                if is_active {
                    self.handle_active_state(
                        c.as_mut(),
                        &mut xkb,
                        &mut rime,
                        &mut plugin_host,
                        &mut panel,
                        &mut quick_send,
                        &mut panel_state,
                        &mut last_panel_width,
                        &xime_config,
                        &theme,
                        &mut candidate_window_visible,
                        &mut consumed_presses,
                        &mut last_input_keysym,
                        &mut ctrl_root_visible,
                        &mut last_ascii_mode,
                        &mut im_enabled,
                    );
                }

                // 主题切换后的候选栏重绘（仅候选栏可见且面板未展开时）
                if pending_theme_redraw {
                    pending_theme_redraw = false;
                    if candidate_window_visible && matches!(panel_state, PanelState::Closed) {
                        self.redraw_menu_candidates(
                            c.as_mut(),
                            &theme,
                            &mut candidate_window_visible,
                        );
                    }
                }
            } else {
                pending_theme_redraw = false;
            }

            thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_active_state(
        &self,
        c: &mut dyn ImBackend,
        xkb: &mut Option<XkbContext>,
        rime: &mut RimeEngine,
        plugin_host: &mut PluginHost,
        panel: &mut PanelData,
        quick_send: &mut QuickSendInject,
        panel_state: &mut PanelState,
        last_panel_width: &mut u32,
        xime_config: &XimeConfig,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
        consumed_presses: &mut std::collections::HashSet<u32>,
        last_input_keysym: &mut Option<u32>,
        ctrl_root_visible: &mut bool,
        last_ascii_mode: &mut bool,
        im_enabled: &mut bool,
    ) {
        if let Some(ref mut x) = xkb {
            if let Some((fd, size)) = c.get_keymap_pending() {
                if let Err(e) = x.set_keymap_from_owned_fd(fd, size) {
                    debug!("Keymap error: {}", e);
                }
            }

            let (depressed, latched, locked, group) = c.get_modifiers();
            x.update_modifiers(depressed, latched, locked, group);
        }

        // 指针事件：菜单按钮 / 面板入口点击
        let pointer_events = c.pop_pointer_events();
        for pe in pointer_events {
            if pe.button != 272 || !pe.pressed {
                continue; // 只处理左键按下
            }
            debug!(
                "Pointer press: x={}, y={}, on_menu={}",
                pe.x, pe.y, pe.on_menu
            );
            self.handle_pointer_press(
                c,
                plugin_host,
                panel,
                panel_state,
                last_panel_width,
                theme,
                candidate_window_visible,
                &pe,
            );
        }

        let events = c.pop_key_events();
        for event in events {
            debug!(
                "Key event: keycode={}, pressed={}",
                event.key, event.pressed
            );

            if let Some(ref mut x) = xkb {
                let keysym = x.key_from_keycode(event.key + 8);
                if let Some(sym) = keysym {
                    self.handle_key_event(
                        c,
                        x,
                        rime,
                        plugin_host,
                        panel,
                        quick_send,
                        panel_state,
                        last_panel_width,
                        xime_config,
                        theme,
                        event,
                        sym,
                        candidate_window_visible,
                        consumed_presses,
                        last_input_keysym,
                        ctrl_root_visible,
                        last_ascii_mode,
                        im_enabled,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_key_event(
        &self,
        c: &mut dyn ImBackend,
        xkb: &XkbContext,
        rime: &mut RimeEngine,
        plugin_host: &mut PluginHost,
        panel: &mut PanelData,
        quick_send: &mut QuickSendInject,
        panel_state: &mut PanelState,
        last_panel_width: &mut u32,
        xime_config: &XimeConfig,
        theme: &PanelTheme,
        event: xime_wayland::KeyEvent,
        sym: Keysym,
        candidate_window_visible: &mut bool,
        consumed_presses: &mut std::collections::HashSet<u32>,
        last_input_keysym: &mut Option<u32>,
        ctrl_root_visible: &mut bool,
        last_ascii_mode: &mut bool,
        im_enabled: &mut bool,
    ) {
        let modifiers = xkb.get_modifiers();
        let release_mask = if !event.pressed {
            librime::K_RELEASE_MASK
        } else {
            0
        };
        debug!(
            "keysym={}, modifiers={}, release={}",
            sym.raw(),
            modifiers.effective,
            release_mask
        );

        // Ctrl+Space：启停输入法（fcitx 风格）。任意状态下优先处理。
        if event.pressed && modifiers.ctrl && sym.raw() == 0x20 {
            *im_enabled = !*im_enabled;
            if *im_enabled {
                debug!("Input method enabled (Ctrl+Space)");
            } else {
                debug!("Input method disabled (Ctrl+Space)");
                // 停用：丢弃组合、清空 preedit、关闭全部 UI
                rime.clear_composition();
                c.clear_preedit();
                c.hide_candidate_window();
                c.hide_panel();
                c.hide_root_window();
                let _ = c.flush();
                *candidate_window_visible = false;
                quick_send.clear();
                *panel_state = PanelState::Closed;
                *ctrl_root_visible = false;
                *last_input_keysym = None;
                consumed_presses.clear();
                self.rt_handle.block_on(async {
                    self.tray.set_mode(InputMode::English).await;
                });
            }
            // 消费按下（释放由 consumed_presses 抑制，避免孤儿释放）
            consumed_presses.insert(event.key);
            return;
        }

        // 停用态：按键直接转发，不做任何处理（被消费按下的释放仍抑制）。
        if !*im_enabled {
            if event.pressed || !consumed_presses.contains(&event.key) {
                c.forward_key(event.serial, event.time, event.key, event.pressed);
            } else {
                consumed_presses.remove(&event.key);
            }
            return;
        }

        let is_ctrl = sym.raw() == 0xFFE3 || sym.raw() == 0xFFE4;
        debug!(
            "is_ctrl={}, candidate_visible={}, last_key={:?}",
            is_ctrl, candidate_window_visible, last_input_keysym
        );

        // 面板打开时（对齐 XimeYao 桌面交互）：Esc 关闭；`;` 上屏分号（面板保持）；
        // 其余可打印/编辑键收起面板继续输入（修饰键/Ctrl 不收起、不拦截）。
        if event.pressed && matches!(panel_state, PanelState::Open(_)) {
            let raw = sym.raw();
            let is_modifier = matches!(raw, 0xFFE1 | 0xFFE2 | 0xFFE9 | 0xFFEA | 0xFFE3 | 0xFFE4);
            let printable_or_edit = (0x20..0x7F).contains(&raw)
                || matches!(raw, 0xFF08 | 0xFF0D | 0xFF8D | 0xFF1B)
                || matches!(raw, 0xFF51..=0xFF54);
            if printable_or_edit && !is_modifier {
                if raw == 0xFF1B {
                    // Escape：直接关闭面板，恢复候选栏。
                    self.close_panel(c, panel, panel_state, theme, candidate_window_visible);
                    consumed_presses.insert(event.key);
                    return;
                }
                if raw == 0x3B {
                    // 分号：上屏分号（面板保持打开，可连续输入）。
                    c.commit_string(";");
                    let _ = c.flush();
                    plugin_host.emit_text_committed(";");
                    consumed_presses.insert(event.key);
                    return;
                }
                // 键入收起：关闭面板，按键继续走正常输入路径。
                self.close_panel(c, panel, panel_state, theme, candidate_window_visible);
            }
        }

        // `;`（中文态、无组合输入）：打开表情页（对齐 XimeYao 的分号触发入口）。
        if event.pressed
            && sym.raw() == 0x3B
            && matches!(panel_state, PanelState::Closed)
            && rime
                .session()
                .and_then(|s| s.status().ok())
                .is_some_and(|st| !st.is_ascii_mode && !st.is_composing)
        {
            // 兜底：触发前重载插件（daemon 早于插件安装启动时也能用）。
            plugin_host.reload();
            if plugin_host.emoji_plugin_count() > 0 {
                self.open_panel_page(
                    c,
                    plugin_host,
                    panel,
                    panel_state,
                    PanelPage::Emoji,
                    theme,
                    candidate_window_visible,
                );
                consumed_presses.insert(event.key);
                return;
            }
        }

        // 快捷发送编码注入：数字键/导航键按当前展示状态接管或落穿
        if quick_send.active()
            && self.handle_quick_send_key(
                c,
                rime,
                plugin_host,
                quick_send,
                &event,
                sym,
                theme,
                last_panel_width,
                consumed_presses,
                candidate_window_visible,
            )
        {
            return;
        }

        if *candidate_window_visible
            && is_ctrl
            && self.handle_ctrl_key(
                c,
                xime_config,
                theme,
                &event,
                sym,
                modifiers,
                candidate_window_visible,
                last_input_keysym,
                ctrl_root_visible,
                rime,
            )
        {
            return;
        }

        if let Some(session) = rime.session() {
            let result = session.process_key(
                sym.raw() as i32,
                modifiers.effective as i32 | release_mask as i32,
            );
            debug!("Rime result: {:?}", result);

            // 空格兜底：Rime 处理后组合仍在且无候选（confirm 落空/未消费），
            // 直接上屏编码，保证空格始终有产出（对齐 XimeYao）。
            let raw_input = session.get_input().map(str::to_string);
            let num_candidates = session.context().map_or(0, |ctx| ctx.menu().num_candidates);
            if let Some(raw) =
                space_fallback_input(sym.raw(), &modifiers, raw_input.as_deref(), num_candidates)
            {
                c.commit_string(&raw);
                let _ = c.flush();
                plugin_host.emit_text_committed(&raw);
                session.clear_composition();
                c.clear_preedit();
                let _ = c.flush();
                consumed_presses.insert(event.key);
                debug!("Space with no candidates: committed raw input '{raw}'");
                return;
            }

            if result && event.pressed {
                let letter = keysym_to_letter(sym.raw());
                if letter.is_some() {
                    *last_input_keysym = Some(sym.raw());
                    debug!("Recorded last input keysym={}", sym.raw());
                }
            }

            if let Ok(status) = session.status() {
                let is_ascii = status.is_ascii_mode;
                if is_ascii != *last_ascii_mode {
                    *last_ascii_mode = is_ascii;
                    let tray_mode = if is_ascii {
                        InputMode::English
                    } else {
                        InputMode::Chinese
                    };
                    self.rt_handle.block_on(async {
                        self.tray.set_mode(tray_mode).await;
                    });
                    debug!("Tray updated: ascii_mode={}", is_ascii);
                }
                debug!("ascii_mode={}, composing={}", is_ascii, status.is_composing);
            }

            // 回车键（Return / 数字键盘回车）：与其他按键一致，按 Rime 消费结果决定转发。
            // - 组合态下 Rime 吞掉回车并提交编码（result=true），不再转发，
            //   应用只收到上屏的编码文本，不会多出回车符。
            // - 空组合态下 Rime 不消费（result=false），转发给应用（正常换行/执行命令）。
            // 被吞按下的释放事件同样抑制（孤儿释放抑制，对齐 fcitx5）。
            if should_forward_key(event.pressed, result, consumed_presses, event.key) {
                c.forward_key(event.serial, event.time, event.key, event.pressed);
            }
            if result && event.pressed {
                consumed_presses.insert(event.key);
            } else {
                consumed_presses.remove(&event.key);
            }

            if let Some(commit) = session.commit() {
                let committed = commit.text();
                c.commit_string(committed);
                let _ = c.flush();
                debug!("Committed: {}", committed);
                plugin_host.emit_text_committed(committed);
            }

            if let Some(ctx) = session.context() {
                if let Some(p) = ctx.composition().preedit {
                    c.set_preedit(p, p.len() as i32);
                } else {
                    c.clear_preedit();
                }
                let _ = c.flush();

                let menu = ctx.menu();
                // 快捷发送编码匹配：原始输入 = preedit[..sel_start]
                let comp = ctx.composition();
                let raw_input = comp
                    .preedit
                    .and_then(|p| p.get(..comp.sel_start.min(p.len())))
                    .unwrap_or("");
                let matched: Vec<(String, String)> = if raw_input.is_empty() {
                    Vec::new()
                } else {
                    let entries = self
                        .clipboard
                        .list_quick_send()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| !e.code.is_empty())
                        .map(|e| (e.code, e.text))
                        .collect::<Vec<_>>();
                    match_quick_send_codes(&entries, raw_input, 9)
                };
                quick_send.items = matched;
                quick_send.rime_count = menu.num_candidates;
                quick_send.highlighted = 0;

                if menu.num_candidates > 0 {
                    // candidate_count 配置限制展示条数（对齐 macOS 版 max_candidates）
                    let max_candidates = xime_config.style.candidate_count.clamp(1, 9) as usize;
                    let mut candidate_items: Vec<xime_ui::CandidateItem> = menu
                        .candidates
                        .iter()
                        .take(max_candidates)
                        .enumerate()
                        .map(|(i, x)| {
                            let comment = x.comment.map(|c| c.to_string()).unwrap_or_default();
                            debug!("candidate {} text='{}' comment='{}'", i, x.text, comment);
                            xime_ui::CandidateItem {
                                text: x.text.to_string(),
                                comment,
                                index: i,
                            }
                        })
                        .collect();
                    // 快捷发送条目追加在 Rime 当页候选之后（数字键 9 位以内）
                    let room = 9usize.saturating_sub(candidate_items.len());
                    for (code, qs_text) in quick_send.items.iter().take(room) {
                        candidate_items.push(xime_ui::CandidateItem {
                            text: qs_text.clone(),
                            comment: code.clone(),
                            index: candidate_items.len(),
                        });
                    }
                    let highlighted_index =
                        menu.highlighted_candidate_index.min(max_candidates - 1);
                    debug!("highlighted_index={}", highlighted_index);
                    if let Err(e) =
                        c.show_candidate_window(&candidate_items, highlighted_index, theme)
                    {
                        debug!("Candidate window error: {}", e);
                    }
                    *last_panel_width = c.candidate_width(&candidate_items, theme);
                    // 缓存最近候选（菜单开/关后重绘）
                    if let Ok(mut cache) = self.candidate_cache.lock() {
                        *cache = Some((candidate_items.clone(), highlighted_index));
                    }
                    *candidate_window_visible = true;
                } else if !quick_send.items.is_empty() {
                    // Rime 无候选：宿主接管候选展示（快捷发送编码命中）
                    self.render_quick_send(
                        c,
                        quick_send,
                        theme,
                        last_panel_width,
                        candidate_window_visible,
                    );
                } else {
                    quick_send.clear();
                    if *candidate_window_visible {
                        c.hide_candidate_window();
                        let _ = c.flush();
                        *candidate_window_visible = false;
                        // 无候选时清空缓存，避免菜单重绘展示过期内容
                        if let Ok(mut cache) = self.candidate_cache.lock() {
                            *cache = None;
                        }
                    }
                }
            }
        }
    }

    // ── 面板（对齐 XimeYao：菜单/列表/网格子页，进页 reload 一次数据，
    // 绘帧只读内存；点击命中与绘制同一几何 panel_hit）──────────────────

    /// 打开面板页面：加载数据 → 注入 im 层 → 渲染（复用候选栏内容撑底）。
    #[allow(clippy::too_many_arguments)]
    fn open_panel_page(
        &self,
        c: &mut dyn ImBackend,
        _plugin_host: &mut PluginHost,
        panel: &mut PanelData,
        panel_state: &mut PanelState,
        page: PanelPage,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
    ) {
        match page {
            PanelPage::Clipboard => {
                panel.list = self.load_panel_list(page);
                // 打开剪贴板面板时拉取一次远端（对齐 Android pullOnce 语义）。
                let _ = self.sync_tx.send(SyncMessage::Pull);
            }
            PanelPage::QuickSend => panel.list = self.load_panel_list(page),
            PanelPage::Emoji => {
                // 内置表情表（对齐 XimeYao：离线零配置，不依赖插件）。
                panel.grid_source = crate::emoji::GROUPS
                    .iter()
                    .map(|g| {
                        (
                            g.category.to_string(),
                            g.symbols.iter().map(|s| s.to_string()).collect(),
                        )
                    })
                    .collect();
                panel.grid.recent =
                    crate::recent_usage::load(crate::recent_usage::RecentKind::Emoji);
            }
            PanelPage::Symbol => {
                panel.grid_source = symbols::GROUPS
                    .iter()
                    .map(|g| {
                        (
                            g.category.to_string(),
                            g.symbols.iter().map(|s| s.to_string()).collect(),
                        )
                    })
                    .collect();
                panel.grid.recent =
                    crate::recent_usage::load(crate::recent_usage::RecentKind::Symbol);
            }
            PanelPage::Menu => {}
        }
        let tabs: Vec<String> = if page.is_grid_page() {
            std::iter::once(xime_ui::RECENT_LABEL.to_string())
                .chain(panel.grid_source.iter().map(|(name, _)| name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let has_pager = page.is_grid_page()
            && panel
                .grid_source
                .iter()
                .any(|(_, glyphs)| glyphs.len() > xime_ui::GRID_PER_PAGE);
        panel.grid = PanelGrid {
            source: page,
            tab: 0,
            page: 0,
            recent: panel.grid.recent.clone(),
            cells: Vec::new(),
            item_count: 0,
            tabs,
            has_pager,
        };
        Self::refresh_grid_cells(panel);
        *panel_state = PanelState::Open(page);
        self.show_panel_page(c, panel, page, theme, candidate_window_visible);
    }

    /// 按 tab/页码重填网格格子（进页、切标签、翻页后调用）。
    fn refresh_grid_cells(panel: &mut PanelData) {
        let tab = panel.grid.clamped_tab();
        let (items, total): (Vec<String>, usize) = if tab == 0 {
            let recent = panel.grid.recent.clone();
            let total = recent.len();
            (recent, total)
        } else {
            let group = panel
                .grid_source
                .get(tab - 1)
                .cloned()
                .unwrap_or_default()
                .1;
            let total = group.len();
            (group, total)
        };
        panel.grid.item_count = total;
        let start = panel.grid.clamped_page() * xime_ui::GRID_PER_PAGE;
        panel.grid.cells = (0..xime_ui::GRID_PER_PAGE)
            .map(|i| items.get(start + i).cloned())
            .collect();
    }

    /// 渲染面板当前页（注入 im 层 + 复用候选栏内容撑底）。
    fn show_panel_page(
        &self,
        c: &mut dyn ImBackend,
        panel: &PanelData,
        page: PanelPage,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
    ) {
        if let Err(e) = c.show_panel(page, &panel.list, &panel.grid) {
            debug!("Panel show error: {}", e);
        }
        let cached = self.candidate_cache.lock().ok().and_then(|g| g.clone());
        let (candidates, highlighted) = cached.unwrap_or_else(|| (Vec::new(), 0usize));
        if let Err(e) = c.show_candidate_window(&candidates, highlighted, theme) {
            debug!("Panel render candidate window error: {}", e);
        }
        let _ = c.flush();
        *candidate_window_visible = true;
    }

    /// 关闭面板（清数据 + 恢复候选栏）。
    fn close_panel(
        &self,
        c: &mut dyn ImBackend,
        panel: &mut PanelData,
        panel_state: &mut PanelState,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
    ) {
        *panel_state = PanelState::Closed;
        *panel = PanelData::default();
        c.hide_panel();
        self.redraw_menu_candidates(c, theme, candidate_window_visible);
    }

    /// 加载列表子页数据（剪切板 / 快捷发送）。
    fn load_panel_list(&self, page: PanelPage) -> PanelList {
        let items: Vec<PanelListItem> = match page {
            PanelPage::Clipboard => self
                .clipboard
                .list_clipboard(200)
                .unwrap_or_default()
                .into_iter()
                .map(|e| PanelListItem {
                    text: e.text,
                    code: String::new(),
                })
                .collect(),
            PanelPage::QuickSend => self
                .clipboard
                .list_quick_send()
                .unwrap_or_default()
                .into_iter()
                .map(|e| PanelListItem {
                    text: e.text,
                    code: e.code,
                })
                .collect(),
            _ => Vec::new(),
        };
        PanelList {
            source: page,
            items,
            page: 0,
        }
    }

    /// 处理候选栏菜单按钮 / 面板点击（命中测试与绘制同源：xime_ui::panel_hit）。
    #[allow(clippy::too_many_arguments)]
    fn handle_pointer_press(
        &self,
        c: &mut dyn ImBackend,
        plugin_host: &mut PluginHost,
        panel: &mut PanelData,
        panel_state: &mut PanelState,
        last_panel_width: &u32,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
        pe: &xime_wayland::PointerEvent,
    ) {
        if pe.button != 272 || !pe.pressed {
            return; // 只处理左键按下
        }
        let width = (*last_panel_width).max(PANEL_MIN_WIDTH);
        let bar = theme.bar_height();

        // 候选栏区域：菜单按钮开合。
        if pe.y < bar as i32 {
            if xime_ui::menu_button_hit(pe.x, pe.y, *last_panel_width, bar) {
                match panel_state {
                    PanelState::Closed => {
                        self.open_panel_page(
                            c,
                            plugin_host,
                            panel,
                            panel_state,
                            PanelPage::Menu,
                            theme,
                            candidate_window_visible,
                        );
                    }
                    PanelState::Open(_) => {
                        self.close_panel(c, panel, panel_state, theme, candidate_window_visible);
                    }
                }
            }
            return;
        }

        let page = match panel_state {
            PanelState::Open(page) => *page,
            PanelState::Closed => return,
        };
        // 面板内坐标（扣除候选栏高度）。
        let (px, py) = (pe.x.max(0) as u32, (pe.y - bar as i32).max(0) as u32);
        let Some(hit) = xime_ui::panel_hit(page, width, &panel.list, &panel.grid, px, py) else {
            // 面板空白点击：不动作（对齐 XimeYao，不误关）。
            return;
        };
        match hit {
            PanelHit::MenuItem(card) => {
                let target = match card {
                    xime_ui::menu::MenuCard::Clipboard => Some(PanelPage::Clipboard),
                    xime_ui::menu::MenuCard::QuickSend => Some(PanelPage::QuickSend),
                    xime_ui::menu::MenuCard::Emoji => Some(PanelPage::Emoji),
                    xime_ui::menu::MenuCard::Symbol => Some(PanelPage::Symbol),
                    // 语音输入：引擎未接，提示后留在菜单。
                    xime_ui::menu::MenuCard::VoiceInput => {
                        let handle = self.rt_handle.clone();
                        handle.spawn(async move {
                            notify_desktop("语音输入", "暂未开放：本地语音引擎还在路上").await;
                        });
                        None
                    }
                    // 设置：启动设置程序（动作，不开子页）。
                    xime_ui::menu::MenuCard::Settings => {
                        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
                        std::process::Command::new(format!("{home}/.local/bin/xime-setup"))
                            .spawn()
                            .map_err(|e| debug!("Failed to launch xime-setup: {e}"))
                            .ok();
                        None
                    }
                };
                match target {
                    Some(target_page) => {
                        self.open_panel_page(
                            c,
                            plugin_host,
                            panel,
                            panel_state,
                            target_page,
                            theme,
                            candidate_window_visible,
                        );
                    }
                    None => {
                        // 动作类：重绘菜单（状态不变）。
                        self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                    }
                }
            }
            PanelHit::Back => {
                self.open_panel_page(
                    c,
                    plugin_host,
                    panel,
                    panel_state,
                    PanelPage::Menu,
                    theme,
                    candidate_window_visible,
                );
            }
            PanelHit::ListItem(row) => {
                // 上屏后收起面板（XimeYao collapse_panel 语义）。
                if let Some(item) = panel.list.item_at(row) {
                    let text = item.text.clone();
                    c.commit_string(&text);
                    let _ = c.flush();
                    debug!(
                        "List item committed: {} ({} chars)",
                        text,
                        text.chars().count()
                    );
                    plugin_host.emit_text_committed(&text);
                    self.close_panel(c, panel, panel_state, theme, candidate_window_visible);
                }
            }
            PanelHit::GlyphCell(slot) => {
                // 点字形 = 上屏，面板保持打开（可连续选择）；更新最近使用。
                if let Some(glyph) = panel.grid.item_at(slot).map(str::to_string) {
                    c.commit_string(&glyph);
                    let _ = c.flush();
                    debug!("Grid glyph committed: {glyph}");
                    plugin_host.emit_text_committed(&glyph);
                    let kind = if page == PanelPage::Emoji {
                        crate::recent_usage::RecentKind::Emoji
                    } else {
                        crate::recent_usage::RecentKind::Symbol
                    };
                    crate::recent_usage::push(kind, &glyph);
                    panel.grid.recent = crate::recent_usage::load(kind);
                    Self::refresh_grid_cells(panel);
                    self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                }
            }
            PanelHit::GlyphTab(tab) => {
                // 切分类标签：回到该类第一页并刷新网格。
                if panel.grid.select_tab(tab) {
                    Self::refresh_grid_cells(panel);
                    self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                }
            }
            PanelHit::PrevPage => {
                if page.is_grid_page() {
                    if panel.grid.prev_page() {
                        Self::refresh_grid_cells(panel);
                        self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                    }
                } else if panel.list.prev_page() {
                    self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                }
            }
            PanelHit::NextPage => {
                if page.is_grid_page() {
                    if panel.grid.next_page() {
                        Self::refresh_grid_cells(panel);
                        self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                    }
                } else if panel.list.next_page() {
                    self.show_panel_page(c, panel, page, theme, candidate_window_visible);
                }
            }
        }
    }

    fn redraw_menu_candidates(
        &self,
        c: &mut dyn ImBackend,
        theme: &PanelTheme,
        candidate_window_visible: &mut bool,
    ) {
        let cached = self.candidate_cache.lock().ok().and_then(|g| g.clone());
        if let Some((candidates, highlighted)) = cached {
            if let Err(e) = c.show_candidate_window(&candidates, highlighted, theme) {
                debug!("Menu redraw candidate window error: {}", e);
            }
            let _ = c.flush();
            *candidate_window_visible = true;
            debug!("Candidate window redrawn after menu state change");
        } else {
            c.hide_candidate_window();
            let _ = c.flush();
            *candidate_window_visible = false;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_ctrl_key(
        &self,
        c: &mut dyn ImBackend,
        xime_config: &XimeConfig,
        theme: &PanelTheme,
        event: &xime_wayland::KeyEvent,
        _sym: Keysym,
        modifiers: ModifierState,
        _candidate_window_visible: &mut bool,
        last_input_keysym: &mut Option<u32>,
        ctrl_root_visible: &mut bool,
        rime: &mut RimeEngine,
    ) -> bool {
        if event.pressed {
            let ctrl_pressed = modifiers.ctrl;
            let alt_pressed = modifiers.alt;
            let shift_pressed = modifiers.shift;
            let super_pressed = modifiers.super_key;

            if ctrl_pressed && !alt_pressed && !shift_pressed && !super_pressed {
                if let Some(last_key) = *last_input_keysym {
                    let letter = keysym_to_letter(last_key);
                    debug!("last_key={}, letter={:?}", last_key, letter);
                    if let Some(letter) = letter {
                        let schema = rime.get_current_schema().unwrap_or_default();
                        let root = xime_config.get_root_for_key(&schema, letter);
                        debug!("root for '{}' (schema={}) = {:?}", letter, schema, root);
                        if let Some(root) = root {
                            debug!("Ctrl pressed, showing root for '{}': {}", letter, root);
                            if let Err(e) = c.show_root_window(letter, &root, theme) {
                                debug!("Failed to show root window: {}", e);
                            } else {
                                *ctrl_root_visible = true;
                            }
                        }
                    }
                }
            }
        } else if *ctrl_root_visible {
            debug!("Ctrl released, restoring candidate window");
            c.hide_root_window();
            *ctrl_root_visible = false;

            if let Some(session) = rime.session() {
                if let Some(ctx) = session.context() {
                    let menu = ctx.menu();
                    if menu.num_candidates > 0 {
                        let candidate_items: Vec<xime_ui::CandidateItem> = menu
                            .candidates
                            .iter()
                            .enumerate()
                            .map(|(i, x)| {
                                let comment = x.comment.map(|c| c.to_string()).unwrap_or_default();
                                xime_ui::CandidateItem {
                                    text: x.text.to_string(),
                                    comment,
                                    index: i,
                                }
                            })
                            .collect();
                        let highlighted_index = menu.highlighted_candidate_index;
                        if let Err(e) =
                            c.show_candidate_window(&candidate_items, highlighted_index, theme)
                        {
                            debug!("Failed to restore candidate window: {}", e);
                        }
                        if let Err(e) = c.flush() {
                            debug!("Failed to flush: {}", e);
                        }
                    }
                }
            }
        }
        true
    }

    // ── 快捷发送编码注入（从 main 恢复：独立功能，与面板改造无关）──
    fn render_quick_send(
        &self,
        c: &mut dyn ImBackend,
        quick_send: &QuickSendInject,
        theme: &PanelTheme,
        last_panel_width: &mut u32,
        candidate_window_visible: &mut bool,
    ) {
        let candidate_items: Vec<xime_ui::CandidateItem> = quick_send
            .items
            .iter()
            .enumerate()
            .map(|(i, (code, text))| xime_ui::CandidateItem {
                text: text.clone(),
                comment: code.clone(),
                index: i,
            })
            .collect();
        let highlighted_index = quick_send.highlighted.min(8);
        if let Err(e) = c.show_candidate_window(&candidate_items, highlighted_index, theme) {
            debug!("Quick send render error: {}", e);
        }
        *last_panel_width = c.candidate_width(&candidate_items, theme);
        if let Ok(mut cache) = self.candidate_cache.lock() {
            *cache = Some((candidate_items, highlighted_index));
        }
        let _ = c.flush();
        *candidate_window_visible = true;
    }

    /// 提交快捷发送条目：上屏、清组合、退出注入状态。
    #[allow(clippy::too_many_arguments)]
    fn commit_quick_send(
        &self,
        c: &mut dyn ImBackend,
        rime: &mut RimeEngine,
        plugin_host: &PluginHost,
        quick_send: &mut QuickSendInject,
        index: usize,
        candidate_window_visible: &mut bool,
    ) {
        let Some((_, text)) = quick_send.items.get(index) else {
            return;
        };
        let text = text.clone();
        c.commit_string(&text);
        let _ = c.flush();
        debug!("Quick send committed: {text}");
        plugin_host.emit_text_committed(&text);
        rime.clear_composition();
        c.clear_preedit();
        quick_send.clear();
        if let Ok(mut cache) = self.candidate_cache.lock() {
            *cache = None;
        }
        c.hide_candidate_window();
        let _ = c.flush();
        *candidate_window_visible = false;
    }

    /// 注入态按键处理。返回 true 表示按键已被宿主消费。
    ///
    /// - 数字键：位置超出 Rime 当页候选数时提交对应快捷发送条目，否则落穿给 Rime
    /// - Rime 无候选（接管模式）：Return/Space 提交高亮、↑↓/←→/Tab 移动高亮、
    ///   Escape 清组合退出
    /// - 其余按键（含字母）：落穿给 Rime 正常处理
    #[allow(clippy::too_many_arguments)]
    fn handle_quick_send_key(
        &self,
        c: &mut dyn ImBackend,
        rime: &mut RimeEngine,
        plugin_host: &PluginHost,
        quick_send: &mut QuickSendInject,
        event: &xime_wayland::KeyEvent,
        sym: Keysym,
        theme: &PanelTheme,
        last_panel_width: &mut u32,
        consumed_presses: &mut std::collections::HashSet<u32>,
        candidate_window_visible: &mut bool,
    ) -> bool {
        // 我们消费的按下，其释放一并吞掉（孤儿释放抑制）
        if !event.pressed {
            return consumed_presses.remove(&event.key);
        }
        let raw = sym.raw();

        // 数字键归属判定
        let digit_pos = match raw {
            k @ (0x31..=0x39) => Some((k - 0x31) as usize),
            0x30 => Some(9),
            _ => None,
        };
        if let Some(pos) = digit_pos {
            if let Some(index) = quick_send.digit_hit(pos) {
                consumed_presses.insert(event.key);
                self.commit_quick_send(
                    c,
                    rime,
                    plugin_host,
                    quick_send,
                    index,
                    candidate_window_visible,
                );
                return true;
            }
            // Rime 候选范围内的数字：正常选择（落穿）
            return false;
        }

        // Rime 无候选：接管导航与提交键
        if quick_send.rime_count == 0 {
            match raw {
                0xFF0D | 0xFF8D | 0x20 => {
                    // Return / KP_Enter / Space：提交高亮
                    consumed_presses.insert(event.key);
                    let index = quick_send
                        .highlighted
                        .min(quick_send.items.len().saturating_sub(1));
                    self.commit_quick_send(
                        c,
                        rime,
                        plugin_host,
                        quick_send,
                        index,
                        candidate_window_visible,
                    );
                    true
                }
                0xFF1B => {
                    // Escape：清组合退出注入态
                    quick_send.clear();
                    rime.clear_composition();
                    c.clear_preedit();
                    c.hide_candidate_window();
                    let _ = c.flush();
                    if let Ok(mut cache) = self.candidate_cache.lock() {
                        *cache = None;
                    }
                    *candidate_window_visible = false;
                    true
                }
                0xFF52 | 0xFF51 => {
                    // Up / Left：上一个
                    quick_send.highlighted = quick_send.highlighted.saturating_sub(1);
                    self.render_quick_send(
                        c,
                        quick_send,
                        theme,
                        last_panel_width,
                        candidate_window_visible,
                    );
                    true
                }
                0xFF54 | 0xFF53 | 0xFF09 => {
                    // Down / Right / Tab：下一个
                    if quick_send.highlighted + 1 < quick_send.items.len() {
                        quick_send.highlighted += 1;
                    }
                    self.render_quick_send(
                        c,
                        quick_send,
                        theme,
                        last_panel_width,
                        candidate_window_visible,
                    );
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }
}

/// 发 freedesktop 桌面通知（org.freedesktop.Notifications，KDE/GNOME 标准）。
/// 对齐 XimeYao 的部署结果 toast：失败也通知（用户能看到部署按钮没白点）。
async fn notify_desktop(summary: &str, body: &str) {
    let Ok(conn) = zbus::Connection::session().await else {
        debug!("Desktop notification: no session bus");
        return;
    };
    let result = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "xime",           // app_name
                0u32,             // replaces_id
                "input-keyboard", // app_icon（主题图标）
                summary,
                body,
                Vec::<String>::new(), // actions
                std::collections::HashMap::<String, zbus::zvariant::Value>::new(), // hints
                4000i32,              // expire_timeout (ms)
            ),
        )
        .await;
    if let Err(e) = result {
        debug!("Desktop notification failed: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_space_fallback_input() {
        let none = ModifierState::default();
        // 组合态 + 无候选：空格上屏编码
        assert_eq!(
            space_fallback_input(0x20, &none, Some("wubi"), 0),
            Some("wubi".into())
        );
        // 有候选：交给 Rime confirm 首选
        assert_eq!(space_fallback_input(0x20, &none, Some("wubi"), 3), None);
        // 无组合：正常空格
        assert_eq!(space_fallback_input(0x20, &none, None, 0), None);
        assert_eq!(space_fallback_input(0x20, &none, Some(""), 0), None);
        // 带修饰键：不拦（Ctrl+Space 启停 / Shift+Space 全半角）
        let with_shift = ModifierState {
            shift: true,
            ..ModifierState::default()
        };
        assert_eq!(
            space_fallback_input(0x20, &with_shift, Some("wubi"), 0),
            None
        );
        // 非空格键不拦
        assert_eq!(space_fallback_input(0xFF0D, &none, Some("wubi"), 0), None);
    }

    #[test]
    fn test_should_forward_key_empty() {
        let consumed = HashSet::new();
        // 空组合态按下：未消费 → 转发
        assert!(should_forward_key(true, false, &consumed, 10));
        // 空组合态释放：未消费 → 转发
        assert!(should_forward_key(false, false, &consumed, 10));
    }

    #[test]
    fn test_should_forward_key_consumed_press() {
        let mut consumed = HashSet::new();
        // Rime 消费的按下（如组合态回车）：不转发
        assert!(!should_forward_key(true, true, &consumed, 10));
        consumed.insert(10);
        // 其释放事件：不转发（孤儿释放抑制）
        assert!(!should_forward_key(false, false, &consumed, 10));
    }

    #[test]
    fn test_should_forward_key_after_commit() {
        let consumed = HashSet::new();
        // 组合态回车提交编码后，同键的第二次按下（已清空组合）：正常转发
        assert!(should_forward_key(true, false, &consumed, 10));
    }

    #[test]
    fn test_should_forward_key_different_keys() {
        let mut consumed = HashSet::new();
        consumed.insert(10);
        // 10 的释放被抑制，但其他键不受影响
        assert!(!should_forward_key(false, false, &consumed, 10));
        assert!(should_forward_key(false, false, &consumed, 20));
    }

    fn qs_entries() -> Vec<(String, String)> {
        vec![
            ("dz".into(), "地址".into()),
            ("dh".into(), "电话".into()),
            ("dz2".into(), "地址2".into()),
        ]
    }

    #[test]
    fn test_match_quick_send_codes() {
        // 空输入不匹配
        assert!(match_quick_send_codes(&qs_entries(), "", 9).is_empty());
        // 前缀匹配：dz 命中 2 条（按声明顺序）
        let m = match_quick_send_codes(&qs_entries(), "dz", 9);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], ("dz".to_string(), "地址".to_string()));
        assert_eq!(m[1], ("dz2".to_string(), "地址2".to_string()));
        // 精确 + 更长前缀
        assert_eq!(match_quick_send_codes(&qs_entries(), "dh", 9).len(), 1);
        assert!(match_quick_send_codes(&qs_entries(), "dzz", 9).is_empty());
        // limit 截断
        assert_eq!(match_quick_send_codes(&qs_entries(), "d", 2).len(), 2);
    }

    #[test]
    fn test_quick_send_digit_hit() {
        // rime_count = 5：数字 1-5（pos 0-4）属 Rime，6+（pos 5+）属快捷发送
        let qs = QuickSendInject {
            items: vec![("dz".into(), "地址".into()), ("dh".into(), "电话".into())],
            rime_count: 5,
            highlighted: 0,
        };
        assert_eq!(qs.digit_hit(0), None);
        assert_eq!(qs.digit_hit(4), None);
        assert_eq!(qs.digit_hit(5), Some(0));
        assert_eq!(qs.digit_hit(6), Some(1));
        assert_eq!(qs.digit_hit(7), None);
        // 接管模式（rime_count = 0）：全部数字归宿主
        let qs = QuickSendInject {
            items: vec![("dz".into(), "地址".into())],
            rime_count: 0,
            highlighted: 0,
        };
        assert_eq!(qs.digit_hit(0), Some(0));
        assert_eq!(qs.digit_hit(1), None);
    }

    #[test]
    fn test_panel_grid_paging_and_tabs() {
        // 35 项分类 → 2 页（GRID_PER_PAGE=32）；翻页/切标签与 daemon 填格逻辑联动
        let mut data = PanelData {
            grid_source: vec![("数".to_string(), (0..35).map(|i| format!("s{i}")).collect())],
            ..PanelData::default()
        };
        data.grid = PanelGrid {
            source: PanelPage::Symbol,
            tab: 1,
            page: 0,
            recent: vec!["r0".to_string()],
            cells: Vec::new(),
            item_count: 0,
            tabs: vec!["最近".to_string(), "数".to_string()],
            has_pager: true,
        };
        WaylandLoop::refresh_grid_cells(&mut data);
        // 第 1 页：32 格满页
        assert_eq!(data.grid.page_count(), 2);
        assert_eq!(data.grid.item_at(0), Some("s0"));
        assert_eq!(data.grid.item_at(31), Some("s31"));
        assert_eq!(data.grid.item_at(32), None);
        // 下一页：剩 3 项
        assert!(data.grid.next_page());
        WaylandLoop::refresh_grid_cells(&mut data);
        assert_eq!(data.grid.item_at(0), Some("s32"));
        assert_eq!(data.grid.item_at(2), Some("s34"));
        assert_eq!(data.grid.item_at(3), None);
        assert!(!data.grid.next_page());
        // 切回「最近」标签：回到该标签第 0 页
        assert!(data.grid.select_tab(0));
        assert_eq!(data.grid.clamped_page(), 0);
        WaylandLoop::refresh_grid_cells(&mut data);
        assert_eq!(data.grid.item_at(0), Some("r0"));
        assert_eq!(data.grid.item_at(1), None);
        // 重复切同一标签不动作
        assert!(!data.grid.select_tab(0));
    }
}

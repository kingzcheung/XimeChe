# XimeChe（曦码·澈输入法）重要决策

## 2026-10-26: serial 必须与 context 成对绑定，不能共用一个 u32 字段

**问题**：WorkBuddy 里中文输入时好时坏——候选词能出来，但选词后
"不上屏"或上到别的窗口；重启 daemon 后换个窗口又正常。

**根因**：`InputMethodV1Data.serial` 是**单个 `u32`**，但 KWin 会为每个
文本输入客户端各建一个 `zwp_input_method_context_v1`，它们**同时**往这个
字段上报 `commit_state`，后写覆盖先写。

协议语义：`commit_string` / `preedit_string` / `delete_surrounding_text`
的第一个参数是该 **context 自己**最近一次的 serial。客户端会把
序列号不匹配的请求直接丢弃。

**日志实锤**（21:43:28–31）：`commit_state` 序列严格交替出现两条：

```
294 1805 1806 295 1807 1808 296 1809 1810 297 1811 1812 298 1813 …
```

全天统计：小序列（<1000）1494 次 : 大序列（≥1000）902 次。两条序列
交叉说明确实是两个 context 在并发上报，而它们写进同一个 `state.serial`。

**决策**：
1. 新增 `ActiveContext { ctx, serial }`，serial 与 context 对象同生共死；
   `InputMethodV1Data.context` 类型改为
   `Arc<Mutex<Option<ActiveContext>>>`。
2. `Dispatch<ZwpInputMethodContextV1>` 里先判定事件来源是不是**当前活跃
   context**（`active.ctx == *proxy`），只有活跃 context 的
   `commit_state` / `surrounding_text` / `content_type` / `reset` 才允许
   修改共享状态。非活跃 context 的 `reset` 尤其不能清——那是另一个窗口的
   组合状态，清掉会把当前窗口正在打的字吞掉。
3. `commit_string` / `set_preedit` / `clear_preedit` 一律用
   `active.serial`，不再读 `state.serial`。后者保留但**仅供诊断**，
   源码注释已标明不可用于发送。
4. `deactivate` 只在停用的正是当前活跃 context 时才清引用。否则窗口 A
   失焦发的 DEACTIVATE 会把窗口 B 刚 grab 到的 context 抹掉——表现为
   "切回 B 突然打不了字"。

**验证**：`xime-wayland` 18 个单元测试通过；`clippy -D warnings` 干净。
真机需重新安装后，在两个窗口间来回切换打字验证（一边有候选窗、一边
选中英文输入），观察是否还出现上屏丢失。

## 2026-10-26: v1 必须转发 modifiers，否则终端 Ctrl+C 全废

**问题**：终端里 `Ctrl+C`（SIGINT）、`Ctrl+X`、`Ctrl+Z` 完全无效。

**根因**：`grab_keyboard` 之后只调用了 `zwp_input_method_context_v1.key()`
转发按键，**从未调用配套的 `.modifiers()`**——全仓库搜 `.modifiers(`
在 `im_v1.rs` 里零命中。

协议语义（`input-method-unstable-v1.xml`）：`key` 只转发
`wl_keyboard::key` 事件（按键本身），修饰位（Mod4/Ctrl 等）必须通过
`modifiers(serial, mods_depressed, mods_latched, mods_locked, group)`
另行通知。缺了它，应用收到的是一个**不带 Ctrl 的孤立字符 `c`**，自然
不会触发 SIGINT。Alt+Tab、Super 等同理失效。

**日志实锤**（21:34:20）：
```
keysym=65507 (Ctrl_L) modifiers=4            ← Ctrl 按下，Rime result: false
keysym=99    (c)      modifiers=4  release=0 ← C 按下，Rime result: false
```
daemon 侧修饰状态完全正确（`modifiers=4`）、Rime 也没消费（应转发），
所以逻辑层没问题——问题就是转发时丢了 modifiers。

**决策**：
1. `im_v1::forward_modifiers()` 封装 `ctx.modifiers(...)`；
   `ImBackend` 加同名方法，**默认空实现**（v2 走 text-input-v3，修饰状态
   由合成器维护，客户端无需转发）。
2. daemon 在 `forward_key` **之前**转发修饰键，且仅在
   `effective` 变化时发（`forwarded_modifiers: Option<u32>` 缓存，
   None 表示首帧必发）。
3. 重新激活（新 context）时重置 `forwarded_modifiers = None`，避免沿用
   上个窗口残留的 Ctrl 按下态。
4. serial 用**键盘事件的 serial**（不是 commit_state 的），应用按 serial
   顺序匹配修饰状态与按键。

**验证**：新增 `test_system_shortcut_always_forwarded`（Rime 消费时带修饰
键仍须转发）；xime-wayland 73 + xime-daemon 72 测试全过。

## 2026-10-26: 退出前必须拆 IM 连接，否则残留 grab 会瘫痪系统快捷键

**问题**：daemon 退出后，用户报告"系统里很多快捷键都不响应"（Alt+Tab、
Super、Ctrl+C 等一概失效）。

**根因**：`clean_exit()` 是 `unsafe { libc::_exit(0) }`——**跳过所有析构**，
Wayland 连接的正常关闭流程完全不执行。KWin 侧若没能及时回收 keyboard
grab，键盘事件会持续被路由到一个已死客户端的通道，于是所有按键（含快捷键）
都进了黑洞。两条退出路径（`DaemonCommand::Shutdown`、命令通道断开）都直接
调 `clean_exit()`，没有任何清理。

**协议约束**：`zwp_input_method_v1` **没有 release grab 请求**，客户端无法
主动释放 grab；合成器在 deactivate 或客户端断开时才回收。因此唯一能做的是
**让连接干净地断开**，把"客户端消失"这个信号可靠地交到 KWin 手上。

**决策**：新增 `shutdown_im_connection(&mut conn)`，在两条退出路径的
`clean_exit()` **之前**调用：
1. `conn.take()` 取出后端，调 `ImBackend::shutdown()`（拆对象 + flush）；
2. 再 `flush()` 兜一次，确保断开前请求送达；
3. `drop` 关闭 socket，KWin 立刻感知客户端断开并回收 grab。

**为什么不靠 Drop impl**：`_exit` 不跑析构，所以必须显式调用，不能指望任何
`Drop`。

**验证**：`cargo build` / `cargo test -p xime-daemon -p xime-wayland`
（73 + 18 全过）；release 产物已安装。

## 2026-10-26: 禁止双 IM 连接——KWin 会话下让 launcher 独占

**问题**：输入法在 Chromium 138 系应用（WorkBuddy / QQ）里"完全打不了中文"，
日志表现为 ACTIVATE/DEACTIVATE 成对快速抖动（21:26:10 起 4 秒内 12 次
ACTIVATE、6 次 DEACTIVATE），按键被吞。

**根因**（日志实锤，非客户端 bug）：daemon 启动时 `connect_im_to_env()` 与
launcher 送 fd 的 `connect_im_from_fd()` **都成功了**，于是同时存在两条
Wayland 连接、两个 `zwp_input_method_v1` 对象向 KWin 注册。两个客户端争抢
keyboard grab，互踢对方 → 激活态高频翻转。证据是同一次事件在日志里成对
出现（`serial=944` 两次、`945` 两次…），且启动日志同时打出
"Connected directly to compositor (standalone mode)" 与
"Connected via launcher fd (KWin mode)"。

原代码假设"KWin 下普通 socket 不暴露 IM 协议，直连必败"——**在 KWin 6.7.4
上不成立**，普通 socket 同样暴露 `zwp_input_method_v1`。

**关键约束**：`zwp_input_method_v1` 协议**没有 destroy 请求**（对象由合成器
创建，客户端只能 `get_input_panel_surface`），客户端无法主动注销 IM 对象，
只靠丢弃引用或 drop 连接都不保证合成器及时回收——所以"连接泄漏后再补救"
这条路走不通，必须**从源头不建立第二条连接**。

**决策**：
1. 启动时先用 `pgrep -x xime-launcher` 判断会话类型：
   - launcher 在跑（KWin 会话）→ **跳过直连**，只等 launcher 传 fd；
   - launcher 缺席（GNOME/wlroots）→ 才直连（standalone）。
2. `ImBackend` 新增 `shutdown()`：launcher 接管前先拆旧连接并 flush，
   作为二道防线（防止未来分支又出现双连接）。
3. v1/v2 各自的 `teardown_im_objects()`：v2 可 destroy 后 roundtrip，
   v1 只能清引用 + flush，真正注销依赖随后的 drop。

**验证**：`cargo clippy -D warnings` 无新增告警；xime-wayland 73 + 
xime-daemon 18 测试全过。真机验证待重启 daemon 后观察 ACTIVATE/DEACTIVATE
是否停止成对抖动。

**排除的假设**（记录避免重走）：WorkBuddy 内置 Chrome/138.0.7204.251，与
QQ 3.2.29 同源，一度怀疑是 Chromium 138 的 text-input disable 后不重新
enable 的 bug（KWin 493098）。但用户日志显示 ACTIVATE **确实到达** daemon，
说明 text-input 是通的，该假设不成立。

## 2026-10-11: Wayland 事件泵非阻塞化 + sync 楔死探测

**问题**：`dispatch_events()` 用阻塞 `roundtrip()`（内含 `blocking_read`）。
2026-10-11 实锤（gdb 栈 + 7.5 小时零日志）：KWin 侧停止应答后 socket 不报
错、roundtrip 永久挂起——wayland 线程连同按键处理、DBus 命令全部冻结；
而 2026-10-05 的断连自愈只在 dispatch **返回 Err** 时触发，挂起状态下自愈
永远不启动。后果：IM 激活的窗口里所有按键（用户报"Ctrl+A 无法全选"）被
楔死的键盘 grab 吸进黑洞，且托盘一切操作无效，只能整进程 kill。

**决策**：`dispatch_events` 重写为三段式（v1/v2 同构）：
1. `flush()` + `dispatch_pending()`——已到达事件立即派发；
2. `prepare_read()` + `poll(fd, 0)` + `read_events()`——0 超时探可读，
   没有事件立即返回（主循环保持 ~1ms 节奏，按键延迟不变）；
3. **sync 楔死探测**：每 1s 发一次 `wl_display.sync`（回调 Done 时清
   标记），未 ack 超 2s 判定连接楔死，dispatch 返回 `Err(TimedOut)`——
   复用既有 HealScheduler 恢复链（杀 launcher → KWin 重拉 → 新 fd）。

**理由**：
1. 楔死检测必须有"主动探针"：安静（无事件）与死亡（不回 sync）在纯
   被动读上不可区分，sync 是最小代价的心跳；
2. 超时返回 Err 而非线程内自救：conn 所有权在主循环，沿用 2026-10-05
   已验证的恢复链是最小改动；
3. poll(0) 而非长超时阻塞：按键路径的延迟预算是毫秒级。

**验证**：213 tests 全过；真机重启后 v1 Context 事件持续流入（serial
递增）、12+ 探针周期零误报；楔死场景恢复依赖下次复现（错误日志已带
"connection wedged" 标记可取证）。

## 2026-10-01: rime 数据目录迁移单目录模型（对齐 XimeYao）

**问题**：Unix 沿用双目录模型（shared=`~/.local/share/xime/rime-data` 只读装
方案，user=`~/.config/xime/rime` 放用户数据），实际暴露三类问题：
1. 方案来源混乱：方案码表在 shared、部署产物和用户数据在 user，方案词表、
   快捷短语、词典管理等一切"读方案目录"的功能都要做目录回退查找；
2. 升级语义缺失：shared 是安装脚本铺的静态副本，方案包更新后用户目录无感知，
   "用户弃用的方案"、"用户定制"与"待升级文件"无法区分；
3. 与跨端契约漂移：XimeYao（Windows）与 Xime（Android）已先后迁移到单目录
   （XimeYao DECISIONS 2026-09-11 明言"旧 shared/user 分离导致方案来源混乱"），
   libximecore 的跨端功能（方案词表/词典管理）被迫为 Linux 维护特例。

**决策**：
1. `shared_data_dir == user_data_dir == ~/.config/xime/rime`（daemon/setup
   启动时显式 `set_rime_paths`，libximecore 的 `default_rime_paths` 默认值
   不变——macOS 等其他宿主不受影响）；
2. dev-install/install 铺的 `~/.local/share/xime/rime-data`（或系统
   `/usr/share/xime/rime-data`）**降级为随包数据源**：启动时经
   `xime_config::ensure_bundled_rime_data` 部署进 rime 目录——
   - 首装（rime 目录无 `*.schema.yaml`）全量复制（递归含 lua/）；
   - 升级只强更"内容有变化且文件名不含 custom"的文件；
   - 用户弃用的 builtin 方案不强更（以 default.custom.yaml 启用列表为基准）；
3. `schema_dict` 等读路径全部回到单目录签名，删除目录数组回退。

**理由**：
1. XimeYao 同款迁移已验证（其 server 启动部署逻辑逐行移植）；
2. 部署语义解决了双目录下"更新方案包"只能靠重跑安装脚本的问题；
3. 用户数据一次性迁移（首装路径触发全量复制），userdb/词库/custom 全保留。

**验证**：真机迁移后 userdb（96120 条词库）、wubi86.custom.yaml、
custom_phrase.txt 无损；方案词表 91397 条 40ms；libximecore 4 组部署单测。

## 2026-08-15: 插件下载即安装 + daemon 插件宿主 + emoji 候选窗

**问题**：插件下载后需手动"安装"，且 daemon 无插件加载能力，插件安装后不生效。

**决策**：
1. 插件下载即安装：下载到临时 .xipk → 立即解压注册 → 删除临时包，去掉"安装"按钮
2. daemon 新增 `PluginHost`：启动时加载 enabled 插件（复用 libximecore xime-plugin 的 PluginRuntime/mlua 沙箱），提供 emoji 契约查询
3. emoji 候选窗：中文态 `;` 触发，字符搜索，数字键/回车选择上屏（与 Android 版契约一致）
4. 插件管理页（设置侧边栏）：列表 + 启用开关 + 卸载二次确认

**理由**：
1. 两步下载安装对用户无意义，下载即装降低心智负担
2. 插件契约与 Android 版 Xime 一致（getCategories/getEmojis），同一 .xipk 两端复用
3. 分阶段：先 emoji 类（候选窗能直接呈现），speech/clipboard 类后续接入

## 2026-08-15: 应用元数据参数化（AppMetadata）

**问题**：libximecore 是跨平台共享库，内部硬编码 "Xime"/"xime" 名称（配置路径、librime distribution 标识），无法被不同宿主平台复用。

**决策**：
1. xime-config 新增 `AppMetadata` 结构（metadata.rs）：`display_name` / `config_dir_name` / `config_file_base` / `distribution_name` / `distribution_code_name` / `app_name` / `version`
2. `set_app_metadata()` 全局注入，未注入时默认 "Xime" 保持兼容
3. 参数化范围**仅限核心**：配置路径（`~/.config/xime`、`/usr/share/xime`）、librime distribution/app 标识、`xime.yaml` 配置文件名、`Xime::SchemaConfigManager` generator_id
4. xime-setup UI 文案（"Xime 设置"、"关于 Xime" 等）**不改**，避免组件签名大规模改动
5. librime `deploy_all` 新增 `deploy_all_with_config(config_name)`，不破坏原签名
6. XimeChe 注入：`config_dir_name` 沿用 `xime`（兼容既有安装），`distribution_name = XimeChe`，显示名"曦码·澈输入法"

**理由**：
1. 目录沿用 xime 保证老用户配置/词库无缝迁移
2. 只参数化功能相关的名称，UI 文案属于显示层，各平台可自行维护
3. librime 是底层 crate，不能依赖 xime-config，故用独立函数参数而非全局状态

## 2026-08-15: 多合成器适配（v2 协议 + 后端抽象）

**问题**：当前只适配 KWin 的 `zwp_input_method_v1`，GNOME 45+ 只暴露 v2。

**决策**：
1. 完整实现 `zwp_input_method_v2` 后端（参考 fcitx5 waylandimserverv2）
2. 新增 `ImBackend` trait 抽象 v1/v2，daemon 用 `Box<dyn ImBackend>` 操作
3. 连接策略：
   - launcher（KWin）：fd 传入后探测 global，优先 v1，无则 v2
   - 直接连接（GNOME）：daemon 启动时连 `$WAYLAND_DISPLAY`，检测 v2
4. 按键转发：v2 无 forward_key 请求，用 `zwp_virtual_keyboard_v1.key()`（fcitx5 同款）
5. 提交语义：v2 请求 double-buffered，必须 `commit(serial)`（serial=done 计数）
6. 候选窗：v2 用 `zwp_input_popup_surface_v2`（合成器锚定光标），v1 保持 overlay_panel

**理由**：
1. GNOME（mutter 45+）、KWin 6、wlroots（Sway/Hyprland）都暴露 v2，一份后端覆盖大多数桌面
2. fcitx5 验证过的方案，避免踩 v2 语义坑（grab 事件、vk 转发、unavailable 重建）
3. GNOME 无 VirtualKeyboard/launcher 机制，必须支持 daemon 直接连接普通 socket

## 2025-05-12: 架构分离决策

**问题**：单进程架构无法满足 KDE VirtualKeyboard 要求
- KWin 通过 `WAYLAND_SOCKET` fd 启动输入法
- `zwp_input_method` 协议不暴露在普通 `wayland-0` socket 上
- 需要按需启动机制

**决策**：采用 daemon + launcher 分离架构
- launcher：接收 `WAYLAND_SOCKET`，通过 DBus 传递 fd
- daemon：DBus 服务，按需激活，处理 Wayland 事件

**理由**：
1. fcitx5 使用相同架构（fcitx5-wayland-launcher）
2. DBus 按需激活符合 KDE VirtualKeyboard 规范
3. 分离后 launcher 轻量，daemon 可持续运行

## 2025-05-12: DBus Service 配置

**问题**：launcher 调用 `org.xime.Xime.OpenWaylandSocket` 时找不到服务

**决策**：创建 DBus service 文件
- 位置：`resources/dbus/org.xime.Xime.service.in`
- 安装：`~/.local/share/dbus-1/services/` 或 `/usr/share/dbus-1/services/`
- DBus 会按需激活 daemon

**理由**：DBus 激活机制要求 service 文件定义服务名称和可执行路径

## 2025-05-12: zwp_input_method_v1 vs v2

**决策**：优先支持 v1 协议（KWin）

**理由**：
1. KWin 5.27 使用 `zwp_input_method_v1`
2. `WAYLAND_SOCKET` fd 是单次使用，必须先尝试 v1
3. v2 用于 Sway/Hyprland，作为备选

## 2025-05-12: 候选窗口定位

**决策**：使用 `zwp_input_panel_surface_v1.set_overlay_panel()`

**理由**：
1. compositor 自动定位在光标附近
2. 不需要手动计算坐标
3. fcitx5 使用相同方法

## 2025-05-15: 配置系统架构

**问题**：主题颜色修改后不生效，需要重启 daemon

**决策**：创建独立的 `xime-config` crate + DBus `ReloadStyle` 方法

**架构**：
1. `xime-config` crate：共享配置模块，所有 crate 依赖
2. 配置合并：系统默认 (`/usr/share/xime/xime.yaml`) + 用户覆盖 (`~/.config/xime/xime.yaml`)
3. 内置默认：编译时嵌入，确保系统配置缺失时有 fallback
4. DBus IPC：`ReloadStyle` 方法通知 daemon 重新加载配置

**理由**：
1. 共享模块避免代码重复
2. 配置合并允许系统级和用户级配置共存
3. DBus IPC 实现无需重启的实时更新

## 2025-05-15: font_size 类型修复

**问题**：用户配置 `font_size: 14.0` 解析失败

**原因**：`StyleConfig.font_size: i32` 不接受浮点数

**决策**：`font_size` 改为 `f32` 类型

**理由**：
1. YAML 中 `14.0` 是浮点数，`serde_yaml` 严格类型检查
2. `f32` 兼容整数和浮点数输入
3. 后续可支持小数字号（如 14.5）
## 2026-09-05: 候选栏主题样式与亮/暗色模式（移植自 macOS 版）

**问题**：候选栏只有 primary_color 一个配置生效，字号/圆角硬编码；无暗色模式

**决策**：
1. `xime-ui` 新增 `PanelTheme`（字号/圆角/亮暗两套配色），渲染层全部 theme 驱动
2. 亮/暗检测用 `org.freedesktop.portal.Settings` 的 color-scheme（DBus，KDE/GNOME 标准）
3. `ImBackend` 接口直接传 `&PanelTheme`（而非拆散的颜色参数）
4. 候选栏高度随字号自适应（≤16 保持 36px 不变，保证既有命中几何稳定）

**理由**：
1. 对齐 macOS 版 XimeYi 的 UiStyle + ui_colors 双模式设计
2. portal 是跨桌面环境的官方接口，无需分别对接 kde/gtk settings
3. theme 整体传递避免后续每加一个样式字段就改一遍 trait 签名

## 2026-09-05: 剪贴板监听协议选型（data-control，非轮询/外部命令）

**问题**：macOS 版靠轮询 `NSPasteboard.changeCount` 捕获剪贴板；Wayland 下没有等价物

**决策**：后台线程 + 独立 Wayland 连接 + data-control 协议事件驱动监听；
优先 `ext-data-control-unstable-v1`（标准暂定），回退 `zwlr-data-control-unstable-v1`

**理由**：
1. 符合项目「无框架依赖、一切自行掌控」原则（不依赖 wl-clipboard 外部命令）
2. 事件驱动无轮询开销；文本读取用 socketpair + 1 MiB 上限防阻塞
3. KDE（主目标环境）与 wlroots 系均支持；GNOME 两者皆无 → 自动捕获不可用（记录为已知限制，面板功能本身可用）
4. 存储层原样移植 macOS 版：表结构对齐 Android（`clipboard_entries` v3），db 文件三端可互换

## 2026-09-05: 插件系统应用模式对齐 Android 版（能力门禁 + 分层职责）

**问题**：XimeChe 插件系统只有 emoji 查询一条通道，无钩子；剪贴板功能与插件零耦合

**决策**（参照 Android 版 Xime 的三层模式）：
1. 剪贴板本地功能是宿主服务（xime-clipboard），插件只做「同步」——
   clipboard_sync 契约（push/pull/testConnection，libximecore 已有）由 daemon
   SyncBridge 独立线程接线，push 用 SHA-256 hash 去重，pull 做回声抑制
2. 宿主数据对插件只读：host.clipboard / host.quickSend 按 manifest
   capabilities 门禁注入（trait 由宿主实现，依赖反转），未声明能力不可见
3. 下行事件（text_committed）同步投递，仅 capabilities.events 订阅者收到
4. network.hosts fail-closed：未声明网络能力的插件禁止 host.http.request；
   allowCustomHosts: true 视为用户显式授权放行
5. 快捷发送编码注入候选栏用宿主原生实现（Rime 候选后追加 + 数字键分区接管），
   不做 transformCandidates 同步钩子——Wayland 按键是同步循环，Lua 卡顿
   直接冻结输入，风险高于 Android

**理由**：
1. Android 版验证过的职责划分：本地剪贴板稳定在宿主，跨设备同步交给插件生态
2. fail-closed 补上了 host.http 白名单缺失的安全洞（此前任意插件可联网）
3. 同步在独立线程：Lua http 20s 超时不阻塞按键路径

## 2026-10-23: 打包：一个 stage.sh + 各发行版原生构建

**背景**：此前只有 `cargo-deb` 产出的 `.deb`，Fedora / Arch 用户只能自己编译。

**决策**：
1. 安装布局集中到 `packaging/stage.sh`；`.deb` 继续走 `cargo-deb`（已验证过的产物不折腾），
   两边文件清单需要人工同步，并在两处注释里写明。
2. rpm / arch 在各自发行版容器里**从源码编译**，而不是把 Ubuntu 二进制换个壳：
   glibc 版本、依赖关系才对；CI 各 job 并行，墙钟时间不增加。
3. Arch 包直接跑仓库里的 PKGBUILD，等于每次发版都验证 AUR 配方，并由 CI 生成
   `.SRCINFO`（手写容易偏移，AUR 会因为 PKGBUILD 与 `.SRCINFO` 不一致拒收）。
4. 版本号唯一来源是 tag，且必须与 `Cargo.toml` 的 `[workspace.package] version` 一致，
   不一致直接失败——宁可红，不出版本号错乱的包。
5. Flatpak / Snap 明确不做：输入法 daemon 必须作为 KWin 的虚拟键盘客户端工作
   （KWin 通过 `WAYLAND_SOCKET` 传 fd，daemon 再用 `zwp_input_method_v2` 收发按键），
   沙箱里既拿不到这个 fd，也无法被 KWin 选为虚拟键盘。这不是权限问题，是架构冲突。

**理由**：包管理器用户不该被迫自己装 Rust 工具链；而输入法要接入的是 KWin 的
输入法通道，深到任何沙箱格式都覆盖不了。

/**
 * IPC 通道名（Tauri 2）。**command 与 event 两类命名规则不同，勿混用**：
 *
 *  1. **command**（经 `invoke()` 调用）：值 **必须逐字等于 Rust `#[tauri::command]` 的函数名**
 *     （snake_case，且必须出现在 `src-tauri/src/main.rs` 的 `generate_handler![]` 里）。
 *     Tauri 的命令名就是 Rust 函数名，**冒号在 Rust 标识符里不合法** —— 故 command 值里
 *     绝不能出现 `:`。（历史坑：本文件曾从 Electron 照搬 `'config:get'` 风格，Rust 侧永远匹配不上，
 *     运行期报 `Command config:get not found`，而调用方 `.catch()` 把错误吞了、tsc 也查不出字符串值，
 *     导致前端功能全断却全绿。改名前先确认目标 command 真实存在。）
 *
 *  2. **event**（经 `listen()` 订阅）：值是 Tauri 的自由字符串，**冒号合法且是既定约定**。
 *     `EVENT_*` 的值与 `src-tauri/src/events.rs` 的 `channel` 常量逐字对应（单一真值，两侧须同改）。
 *     **不要把 event 一起 snake_case 化** —— 那会与 Rust `emit()` 的名字错开、订阅静默收不到。
 */
export const IPC_CHANNELS = {
  // 代理控制
  PROXY_START: 'proxy_start',
  PROXY_STOP: 'proxy_stop',
  PROXY_GET_STATUS: 'proxy_get_status',
  PROXY_RESTART: 'proxy_restart',
  // §2 待应用差集（pull 模型，configChanged/STARTED/STOPPED 后拉）：节点集相对运行核快照的增/改/删差集，供动作条汇总
  // + 待入池/待生效徽标数据源。核未运行（无快照）→ 空差集（动作条隐藏）。
  PROXY_GET_PENDING_CHANGES: 'proxy_get_pending_changes',
  // §2 动作条「立即应用」：把 Rust 侧最新 config force-restart 入核（复用 F11 applyConfigForcingRestart，绕 P2-A/B defer）。
  PROXY_APPLY_PENDING_CHANGES: 'proxy_apply_pending_changes',
  KERNEL_PROBE_OUTBOUND: 'kernel_probe_outbound', // 自定义协议兼容性 probe（当前内核 sing-box check）

  // 配置管理
  CONFIG_GET: 'config_get',
  CONFIG_SAVE: 'config_save',
  CONFIG_CLASSIFY_STAGED: 'config_classify_staged',
  CONFIG_UPDATE_MODE: 'config_update_mode',
  CONFIG_GET_VALUE: 'config_get_value',
  CONFIG_SET_VALUE: 'config_set_value',
  CONFIG_GET_PRIVACY_MODE: 'config_get_privacy_mode',
  CONFIG_SET_PRIVACY_MODE: 'config_set_privacy_mode',
  PRIVACY_SET_PASSWORD: 'privacy_set_password',
  PRIVACY_UNLOCK: 'privacy_unlock',
  PRIVACY_HAS_PASSWORD: 'privacy_has_password',

  // 服务器管理
  SERVER_SWITCH: 'server_switch',
  SERVER_GENERATE_URL: 'server_generate_url',
  SERVER_ADD: 'server_add',
  SERVER_ADD_BULK: 'server_add_bulk', // 批量添加自建节点（本地导入，一次 loadConfig→saveConfig）
  LOCAL_IMPORT_PARSE: 'local_import_parse', // 本地导入：解析文件/文本 → 预览（节点 + 订阅 + 统计）；不可识别格式 throw
  LOCAL_IMPORT_PICK_FILE: 'local_import_pick_file', // 本地导入：弹系统原生文件对话框选配置文件 + 读内容回传（替代 HTML input，避开 Chromium 英文文案、对话框天然跟随系统语言）
  SERVER_UPDATE: 'server_update',
  SERVER_DELETE: 'server_delete',
  SERVER_DELETE_BATCH: 'server_delete_batch',
  SERVER_GET_ALL: 'server_get_all',
  SERVER_SPEED_TEST: 'server_speed_test',
  WARP_REGISTER: 'warp_register', // Cloudflare WARP 设备注册 → 生成 WireGuard 草稿
  WARP_APPLY_LICENSE: 'warp_apply_license', // 对已注册 WARP 节点原地应用 WARP+ license（升级免重建）
  TAILSCALE_LOGIN: 'tailscale_login', // 按需瞬态登录核：拉起登录专用 sing-box 取交互登录 URL（Phase 2）
  TAILSCALE_LOGIN_CANCEL: 'tailscale_login_cancel', // 取消某节点在飞的瞬态登录核（用户手动取消）
  TAILSCALE_LOGOUT: 'tailscale_logout', // 退出登录：清该节点 state 目录（持久会话）；保留节点配置/authKey
  TAILSCALE_STATE_EXISTS: 'tailscale_state_exists', // 批量查 TS 节点 state 目录存在性（不起核判「登录过没」）：代理关时登录态缓存未命中的兜底
  TAILSCALE_GET_STATUS: 'tailscale_get_status', // L2：主动拉各 TS 节点状态末帧(self IP/peers) + 新鲜度(connected)。治本「状态流 push-only 无 pull、渲染端错过推送即陈旧」
  // ── Taildrop 收件箱（sing-box 1.14.0-beta.15）。核无条件收件，故这几条是「看得见 + 清得掉」的最低要求 ──
  TAILDROP_LIST: 'taildrop_list', // 读一次收件箱（首帧快照，不留订阅）
  TAILDROP_MARK_READ: 'taildrop_mark_read', // 清未读角标，**不删文件**
  TAILDROP_DELETE: 'taildrop_delete', // 删一个已落盘的文件
  TAILDROP_CANCEL: 'taildrop_cancel', // 取消一个接收中的文件（senderID + name 定位）
  TAILDROP_SAVE: 'taildrop_save', // 取件：开原生保存框 + 写盘

  // 订阅管理
  SUBSCRIPTION_ADD: 'subscription_add',
  SUBSCRIPTION_UPDATE: 'subscription_update',
  SUBSCRIPTION_DELETE: 'subscription_delete',
  SUBSCRIPTION_UPDATE_SERVERS: 'subscription_update_servers',
  SUBSCRIPTION_PREVIEW: 'subscription_preview', // 新增订阅前预检：拉取+解析 URL 但不写 config，返回节点数或分类错误

  // 路由规则管理
  RULES_GET_ALL: 'rules_get_all',
  RULES_ADD: 'rules_add',
  RULES_UPDATE: 'rules_update',
  RULES_DELETE: 'rules_delete',
  RULES_REORDER: 'rules_reorder',

  // 应用分流预设（内置 16 条，Rust 是单一真值 → 启动时一次 invoke 拉取入 store）
  APP_PRESETS_LIST: 'app_presets_list',

  // 自定义应用图标本地缓存（设定即下载到 <userData>/icons/，返回 polaris-icon://c/<file> ref；渲染零出站）
  CACHE_APP_ICON: 'cache_app_icon',

  // 规则资源管理（.srs 下载/管理）
  RULE_RESOURCES_LIST: 'rule_resources_list',
  RULE_RESOURCES_DOWNLOAD: 'rule_resources_download',
  RULE_RESOURCES_REDOWNLOAD: 'rule_resources_redownload',
  RULE_RESOURCES_CANCEL: 'rule_resources_cancel', // 中止在途下载（原型 res-cancel :5376）
  RULE_RESOURCES_DELETE: 'rule_resources_delete',
  RULE_RESOURCES_GET_CATALOG: 'rule_resources_get_catalog',
  RULE_RESOURCES_REFRESH_CATALOG: 'rule_resources_refresh_catalog',
  RULE_RESOURCES_GET_CACHED_CATALOG: 'rule_resources_get_cached_catalog', // 零出站回读上次刷新落盘的全量清单
  RULE_RESOURCES_SET_AUTO_UPDATE: 'rule_resources_set_auto_update',
  RULE_RESOURCES_UPDATE_ALL: 'rule_resources_update_all',
  RULE_RESOURCES_RESET_BUILTIN: 'rule_resources_reset_builtin',
  RULE_RESOURCES_UPDATE_BUILTIN: 'rule_resources_update_builtin', // 单个内置 geo 拉上游最新版
  RULE_RESOURCES_ICON_GALLERIES: 'rule_resources_icon_galleries', // 图标库拉取（经 update-in，Phase 1b）
  // 强制刷新图标库：清单内存缓存 + 图标本体磁盘浏览缓存两层一起作废后重拉（用户点「刷新」）
  RULE_RESOURCES_REFRESH_ICON_GALLERIES: 'rule_resources_refresh_icon_galleries',

  // 日志管理
  LOGS_GET: 'logs_get',
  LOGS_SEARCH: 'logs_search',
  LOGS_UNSUBSCRIBE: 'logs_unsubscribe',
  LOGS_CLEAR: 'logs_clear',
  LOGS_EXPORT: 'logs_export', // 纯日志导出（节点身份打码，无配置块/密钥脱敏）——与 DIAGNOSTIC_EXPORT 是两个不同产物
  LOGS_OPEN_DIR: 'logs_open_dir', // 文件管理器打开日志目录（后端一步做完：解析路径 + shell.open）
  LOGS_LEGACY_INFO: 'logs_legacy_info', // W26 前无界 singbox.log：只读大小/路径，不自动删除
  LOGS_ARCHIVE_LEGACY: 'logs_archive_legacy', // 用户选目标后事务式归档，成功才删原文件
  // 核**此刻实际**在用的日志级别（管理 API gRPC `GetDefaultLogLevel`）。与 `config.logLevel`
  // 不是同一件事：后者是「我写下的意图」，隐私锁抬级 / 暂存未落盘时两者会分叉，且渲染端无从补偿。
  LOGS_RUNTIME_LEVEL: 'logs_runtime_level',
  // 当前进程临时诊断态：只抬实时日志门槛，不写 config；应用重启后自动恢复常规级别。
  LOGS_DIAGNOSTIC_STATE: 'logs_diagnostic_state',
  LOGS_SET_DIAGNOSTIC: 'logs_set_diagnostic',

  // 渲染端就绪信号（renderer -> Rust 主进程，单向 fire-and-forget）：App 成功 render+commit（或根级 ErrorBoundary
  // fallback 挂上）后发一次，供主进程 mount 健康门确认「webview 活着且 DOM 真的挂上了」。C 类白屏（进程活着但
  // DOM 空）不发任何主进程事件，此信号缺席（超时）是唯一侦测手段。
  RENDERER_READY: 'renderer_ready',
  // 渲染端日志转发（renderer -> Rust 主进程，单向 fire-and-forget）：前端 console 级别 + 文本落主进程日志文件，
  // 使白屏/早期崩溃这类「devtools 打不开」的现场仍有记录。
  RENDERER_LOG: 'renderer_log',
  // 终局错误页「重新加载」按钮（renderer -> Rust 主进程）：主进程复位 mount 门 + 对本窗重新加载真实应用
  // （错误页自身 location.reload 恢复不了应用，需主进程驱动重载）。
  FATAL_RETRY: 'fatal_retry',

  // 托盘独立窗（tray webview -> Rust 主进程）。窗体显隐与尺寸由主进程持有（浮层要贴托盘图标定位、
  // 失焦即隐），故这几条都是「渲染端请求、主进程执行」的单向命令，没有对应的 api-client 封装。
  TRAY_RENDERER_READY: 'tray_renderer_ready', // React commit 后携冷建代次回执；后端只展示当前代窗口
  TRAY_HIDE: 'tray_hide',
  TRAY_RESIZE: 'tray_resize', // 内容高度变化后请求主进程调窗高（浮层自适应，避免留白/截断）
  TRAY_SHOW_MAIN: 'tray_show_main', // 唤出主窗；可选 screen 参数经 EVENT_TRAY_OPEN_SCREEN 落到目标屏
  TRAY_TAKE_PENDING_SCREEN: 'tray_take_pending_screen', // 取货式读「主窗起来后该去哪一屏」，消费后主进程即清
  TRAY_CHECK_UPDATE: 'tray_check_update', // 整条链（check → hasUpdate → 弹 mini 提醒窗）收在后端
  TRAY_ENTER_LIGHTWEIGHT: 'tray_enter_lightweight', // 主动进轻量态（销毁主窗 webview，留托盘）
  TRAY_QUIT: 'tray_quit',

  // 自启动管理
  AUTO_START_SET: 'auto_start_set',
  AUTO_START_GET_STATUS: 'auto_start_get_status',

  // 统计信息（batch3 §3.7：订阅驱动数据面。renderer 按 topic 声明订阅，main 据订阅集派生 worker demand + 精确 relay）
  STATS_SUBSCRIBE: 'stats_subscribe', // 订阅某 topic（stats|aggregate|detail|closed）
  STATS_UNSUBSCRIBE: 'stats_unsubscribe', // 退订某 topic（unmount/窗口隐藏/暂停）：无订阅者 → worker 逐级停机
  STATS_PROJECT_TOPOLOGY: 'stats_project_topology', // 完整活动表先过滤，再按首页实际高度投影主要/最近目标
  STATS_CLOSED_CLEAR: 'stats_closed_clear', // 清空独立的已结束连接历史
  CONNECTIONS_CLOSE: 'connections_close', // 关单条连接（main 经 9090 DELETE /connections/{id}）
  CONNECTIONS_CLOSE_ALL: 'connections_close_all', // 关全部连接（main 经 9090 DELETE /connections，触发 ResetNetwork）

  // 出口 IP 信息（本地直连出口 / 代理出口）
  IP_INFO_GET: 'ipinfo_get',

  // 解锁检测（AI/流媒体，经当前代理出口）：run 触发一轮检测（force 绕 TTL）；get 纯读最近快照（水合）
  UNLOCK_RUN: 'unlock_run',
  UNLOCK_GET: 'unlock_get',

  // 系统进程枚举（路由规则的进程快速选择器）
  SYSTEM_LIST_PROCESSES: 'system_list_processes',

  // 版本信息
  VERSION_GET_INFO: 'version_get_info',

  // 更新管理
  UPDATE_CHECK: 'update_check',
  UPDATE_DOWNLOAD: 'update_download',
  UPDATE_INSTALL: 'update_install',
  UPDATE_SKIP: 'update_skip',
  UPDATE_OPEN_RELEASES: 'update_open_releases',
  // App 更新弹窗（独立 mini 更新窗）：主进程 → 弹窗推状态载荷；弹窗 → 主进程回传按钮/关闭动作。
  UPDATE_POPUP_STATE: 'update_popup_state',
  UPDATE_POPUP_ACTION: 'update_popup_action',

  // 核心管理
  CORE_UPDATE_CHECK: 'core_update_check',
  CORE_UPDATE_RUN: 'core_update_run',
  CORE_GET_VERSION_INFO: 'core_get_version_info',
  CORE_ROLLBACK: 'core_rollback',
  CORE_REPLACE_MANUAL: 'core_replace_manual',
  CORE_UPDATE_GET_AUTO_STATUS: 'core_update_get_auto_status', // 内核自动更新状态（lastCheckAt/staged/跨带提示）
  CORE_UPDATE_APPLY_STAGED: 'core_update_apply_staged', // 用户点「立即应用」：停代理→换核→重启（唯一允许主动断流）
  CORE_UPDATE_ACK_VERSION_CHANGE: 'core_update_ack_version_change', // banner 展示版本变更通知后 ack 清除 pendingChangeNotice（show→ack，弹一次非每启）
  CORE_RESET_FACTORY: 'core_reset_factory', // B6：把内核恢复为随 App 出厂的版本
  APP_UNINSTALL_ALL: 'app_uninstall_all', // B6：完全卸载 Polaris（提权 helper / 受保护目录内核 / 用户配置 / 应用本体）

  // Shell 操作
  SHELL_OPEN_EXTERNAL: 'shell_open_external',
  // 打开 sing-box 官方面板（dashboard #55）：主进程开窗加载运行期 /dashboard/，预写 localStorage
  // `sing-box-dashboard.server`（一键直连，免手填后端）。渲染端构造不出动态端口故经此 IPC。
  OPEN_SINGBOX_DASHBOARD: 'open_singbox_dashboard',
  // 刷新 sing-box 官方面板资源：清本地缓存目录（<userData>/singbox-dashboard），使核下次启动重拉新 zip。供 UI 手动刷新。
  REFRESH_SINGBOX_DASHBOARD: 'refresh_singbox_dashboard',
  // dashboard #55：取面板连接信息（URL + secret）供「复制连接信息」按钮。secret 取自 main config，不长驻渲染端 store。
  GET_SINGBOX_DASHBOARD_CONNECTION: 'get_singbox_dashboard_connection',

  // 更新事件 (主进程 -> 渲染进程)
  EVENT_UPDATE_PROGRESS: 'update:progress',

  // macOS 提权 helper（免提权启停 sing-box）
  HELPER_GET_STATUS: 'helper_get_status',
  HELPER_INSTALL: 'helper_install',
  HELPER_UNINSTALL: 'helper_uninstall',

  // 系统代理：用户主动清理残留设置（TUN 残留提示的一键恢复动作）
  SYSTEM_PROXY_DISABLE: 'system_proxy_disable',
  // 系统代理活态查询：当前 OS 代理是否仍指向本进程 mixed 入站（只读；连接态 systemProxy 分支的判据）
  SYSTEM_PROXY_GET_STATUS: 'system_proxy_get_status',

  // 事件 (主进程 -> 渲染进程)
  EVENT_PROXY_STARTED: 'event:proxyStarted',
  EVENT_PROXY_STOPPED: 'event:proxyStopped',
  EVENT_PROXY_ERROR: 'event:proxyError',
  // runtime 生命周期结局（`{phase:'ready'|'stopped'|'failed', errorCode?, message?}`）。
  //
  // **与上面两条的分工**：那两条的发射点在后端**命令层**，后端自驱的核起停（去抖重启 /
  // 「立即应用」/ drain 排空 / 崩溃自愈）一个都不发 —— 这正是「点了立即应用，核真重启了、
  // 条上仍显示立即应用」的成因。本通道由 `runtime/proxy.rs` 在**真状态跃迁点**发，
  // 覆盖全部路径。上面两条**保留不动**（后端两个 scheduler 仍听它们：订阅补更会真联网、
  // 换核会排队，收口过去等于每次内部重启都多跑一遍）。
  EVENT_PROXY_LIFECYCLE: 'event:proxyLifecycle',
  // 无载荷：payload 恒为 {}，本文件三个订阅方（App.tsx / TrayMenu.tsx / useConfig.ts）全部丢弃，
  // 详见后端 commands/config.rs::broadcast_config_changed_with 与其调用点守卫。
  EVENT_CONFIG_CHANGED: 'event:configChanged',
  // 订阅后台自动更新结果（scheduler 发；渲染端仅失败态 toast，成功静默——对齐 上游 后台更新只入日志）。
  // 手动刷新走 SUBSCRIPTION_UPDATE_SERVERS 命令的三态 toast，不经本通道。payload={subscriptionId,name,success,error?,addedServers,updatedServers,deletedServers,unchanged}
  EVENT_SUBSCRIPTION_AUTOUPDATE: 'event:subscriptionAutoUpdate',
  // 单订阅更新的**逐阶段进度**（payload={subscriptionId, phase, ...}）。与上面那条的分工：
  // 上面是「后台腿的结局」（scheduler 专属），本通道是「任一腿的过程」——手动刷新与 scheduler
  // 共用后端 `perform_subscription_update`，一个发射点覆盖两条腿，故订阅信息栏无论谁发起都亮。
  // 载荷形状 + 各 phase 语义见 contracts/subscription-progress.ts。
  EVENT_SUBSCRIPTION_UPDATE_PROGRESS: 'event:subscriptionUpdateProgress',
  // 日志批处理广播（T1，issue #225）：~150ms 窗口 coalesce 多条日志为单次数组 IPC（取代旧逐条 event:logReceived，
  // 已删），削平 sing-box 启动期日志洪流对主线程的 IPC 冲击（每条一次 send → 撞 Windows 拖动 move 循环致拖动卡顿）。
  EVENT_LOG_RECEIVED_BATCH: 'event:logReceivedBatch',
  EVENT_STATS_UPDATED: 'event:statsUpdated',
  // 连接导航的有界排名聚合；首页流向另按实测画布槽位通过 command 投影。
  // 取代旧 EVENT_CONNECTIONS_UPDATED（每秒全量 ConnectionEntry[] relay）——连接风暴下渲染端被全量明细拖死（issue #227）。
  // batch3 §3.7：兼作 aggregate topic 的初始帧 + 增量 push 通道（订阅即回缓存帧，之后 seq 变更才推）。
  EVENT_CONNECTIONS_AGGREGATE: 'event:connectionsAggregate',
  // 完整活动表流向字段变化信号：常态/检索都据此重查后端完整表。
  EVENT_CONNECTIONS_TOPOLOGY_CHANGED: 'event:connectionsTopologyChanged',
  // 活动连接 detail：reset 帧给 generation 基线，常态只推 upsert/累计计数/删除 id；sequence 拒绝乱序。
  // 仅连接页活动视图订阅期流动，无订阅者时中继保留下一位订阅者所需的 reset 基线。
  EVENT_CONNECTIONS_DETAIL: 'event:connectionsDetail',
  // 已结束连接独立历史环（最多 1000 条）；仅 closed topic 订阅期推送。
  EVENT_CONNECTIONS_CLOSED: 'event:connectionsClosed',
  // 托盘「打开设置」的**窄**跨窗导航通道（A1）。⚠️ 不是已删除的通用路由 `navigate` 的复活：
  // 值域由 Rust 侧白名单 `tray::normalize_tray_screen` 钉死（当前仅 'settings'）、单播主窗、
  // 唯一发射点是 `tray_show_main` 与托盘原生菜单的「打开设置」。消费点在 `store/nav-store.ts`。
  // 想加第二个目标屏必须先改 Rust 白名单——别在这儿加字符串就以为能跳。
  EVENT_TRAY_OPEN_SCREEN: 'event:trayOpenScreen',
  EVENT_ENTER_PRIVACY_MODE: 'event:enterPrivacyMode',
  EVENT_EXIT_PRIVACY_MODE: 'event:exitPrivacyMode', // 退出隐私模式（解锁/idle 计时复位）
  EVENT_CORE_VERSION_CHANGED: 'event:coreVersionChanged',
  EVENT_CORE_AUTO_UPDATE_STATUS: 'event:coreAutoUpdateStatus', // 内核自动更新状态变更（staged 待生效 / 跨带提示）
  EVENT_CORE_BASELINE_WARNING: 'event:coreBaselineWarning', // 非官方核 ≤ 随包基线：启动 reconcile 发兼容风险提醒
  EVENT_HELPER_UPGRADEABLE: 'event:helperUpgradeable', // 提权 helper proto < 期望（如属主根治 v6）：启动后发，渲染端 toast 引导升级
  EVENT_AUTO_NODE_SWITCHED: 'event:autoNodeSwitched', // 自动换节点成功通知
  EVENT_PROXY_PENDING_CHANGES: 'event:proxyPendingChanges', // R2 待应用差集 PUSH：switch_mode 末尾推 {added, modified, removed}（与 pull 同构）；待应用操作条数据源
  EVENT_PROXY_INVALID_NODES: 'proxy:invalid-nodes', // 启动 gate 剔除的非法节点（空数组=清陈旧标灰）
  EVENT_IP_INFO_UPDATED: 'event:ipInfoUpdated',
  EVENT_UNLOCK_PROGRESS: 'event:unlockProgress', // 解锁检测：单个服务 settle 逐个点亮
  EVENT_UNLOCK_INVALIDATED: 'event:unlockInvalidated', // 解锁检测：切节点/起停代理 → 缓存失效，渲染端复位重跑
  EVENT_UNLOCK_UPDATED: 'event:unlockUpdated', // 解锁检测：一轮完成的完整终态快照（issue 2：渲染端 store 跨组件卸载持有 checkedAt/egress）
  EVENT_RULE_RESOURCE_PROGRESS: 'event:ruleResourceProgress',
  // 测速全量结果列表通道（上游 `speedTestResult`）已删——架构化石，非漏接：上游的原生 Tray 菜单没法自己
  // 刷新，主进程只能把结果数组整推给渲染端；Polaris 托盘是同源 webview 浮层，与主窗共用 use-latency-store
  // 并各自直订下面这条逐节点通道，store 写入本就是合并语义 ⇒ 契约「托盘结果合并非替换」已被结构性满足。
  // 详见 src-tauri/src/events.rs 同处注释。
  EVENT_SPEED_TEST_RESULT: 'event:speedTestResult', // 测速单个节点完成（流式增量显示，payload={serverId,latency}）
  EVENT_SPEED_TEST_PROGRESS: 'event:speedTestProgress', // 测速进度（已测/成功/总数）
  // 一轮测速的终态（payload={outcome,tested,total,pending}）。**广播**，故托盘发起的那轮主窗也收得到
  // ——invoke 返回值只有发起方那个 JS 堆拿得到，这正是「断开后进度 toast 还要等十几秒才转中断」的根因。
  // `pending` = 本轮没拿到值的节点 id，中断态 toast 的「继续」按它续测。详见 contracts/speed-test.ts。
  EVENT_SPEED_TEST_DONE: 'event:speedTestDone',
  EVENT_TAILSCALE_AUTH_URL: 'event:tailscaleAuthUrl', // Tailscale 节点需交互登录：核日志抓出的登录 URL（瞬态核路径）
  EVENT_TAILSCALE_STATUS: 'event:tailscaleStatus', // sing-box 1.14 管理 API 推送的 Tailscale 节点真实态（backendState/loggedIn/authURL/IP/过期）
  EVENT_TAILSCALE_STATE_CLEARED: 'event:tailscaleStateCleared', // 启动前属主归一删掉某节点 root 残留 state（登录态已失效）→ 渲染端清缓存，避免陈旧 loggedIn=true 与已清空 state 撕裂（payload={serverId}）
  EVENT_MESH_LOGIN_FALLBACK: 'event:meshLoginFallback', // 缺陷1 登录期出口让位：选中 TS 出口未就绪→默认路由让位直连(engaged=true)/就绪后切回(engaged=false)，渲染端据此提示（payload={engaged,serverName?}）
  EVENT_SYSTEM_PROXY_RESIDUAL: 'event:systemProxyResidual', // TUN 启动后检测到无 marker 的系统代理残留（非 Polaris 设的）→ 一次性提示

  // 界面语言不再经此反向同步：改走 config.language 单一真值源（主进程直接读 config，见 config-engine）。

  // 窗口控制（Linux 嵌入式标题栏自绘 min/max/close；Mac 原生红绿灯 / Win titleBarOverlay 系统按钮无需）
  WINDOW_MINIMIZE: 'window_minimize',
  WINDOW_MAXIMIZE_TOGGLE: 'window_maximize_toggle',
  WINDOW_CLOSE: 'window_close',
  WINDOW_IS_MAXIMIZED: 'window_is_maximized',
  // 最大化态变更（main -> 渲染）：标题栏 max/restore 图标跟随 WM 双击/拖顶等非按钮操作同步
  EVENT_WINDOW_MAXIMIZE_CHANGED: 'event:windowMaximizeChanged',
  // 重启应用本体（U-7 第三类重启）：改了「进程启动期才读」的设置后，用户在确认弹窗里点「立即重启」。
  // 后端走 `request_restart()` 而非 `restart()`，确保先经 ExitRequested 停核 + 清系统代理（否则留孤儿核）。
  APP_RESTART: 'app_restart',
  // U-7 判据基线：本次进程**启动时**读到的那三个键的生效值。只读、进程内不变。
  // 拿磁盘现值当基线会在「改走又改回」时误报一次重启（而重启会断代理）。
  APP_STARTUP_CONFIG_FLAGS: 'app_startup_config_flags',
  // spec §2.5 Q1-b 清除时机 ④：上次进程是不是正常退出的？**读即清**（每进程只第一次为真）。
  // 真 ⇒ 渲染端在 hydrate 之前清掉持久化的暂存；假 ⇒ 强杀/崩溃，或进程压根没退（自愈重载 /
  // 轻量模式销毁重建）⇒ 照常恢复。判据只有主进程知道，故不能在渲染端自行推断。
  APP_TAKE_CLEAN_EXIT_FLAG: 'app_take_clean_exit_flag',

  // 数据备份与恢复
  BACKUP_EXPORT: 'backup_export', // 选择性导出：接 { categories }，pickCategories 只导出勾选类
  BACKUP_IMPORT_PICK: 'backup_import_pick', // 选择性导入①：弹文件框 + 解析 → 返回备份含哪些类 + 各类数量（不 apply）
  BACKUP_IMPORT_APPLY: 'backup_import_apply', // 选择性导入②：按所选类整类替换 + 空跳过 + sanitize + 保存
  BACKUP_GET_INFO: 'backup_get_info',

  // 诊断报告导出（单 Markdown，脱敏）
  DIAGNOSTIC_EXPORT: 'diagnostic_export',
  // 此处曾有 DIAGNOSTIC_CAPTURE_START / _STOP（「诊断采集」= 临时把内核提级到 debug 再还原）。
  // 整条机制已删除：内核日志改由管理 API 的 SubscribeLog 全级别送来、级别筛在客户端，
  // 把日志页级别拨到 DEBUG 即刻生效，既不落盘也不重启内核 —— 采集会话没有存在理由了。
} as const;

export type IpcChannel = (typeof IPC_CHANNELS)[keyof typeof IPC_CHANNELS];

/**
 * stats 订阅 topic（batch3 §3.7 订阅驱动数据面）：renderer 按 topic 精确声明订阅，main 据订阅集派生 worker
 * demand（aggregate|topology|detail|closed → 同一条上游 Connections 流）并只 relay 给对应 topic 的订阅者。
 *
 * `aggregate` 与 `topology` **是两条需求不是一条**：前者是连接导航排名页要的 Top-N 聚合载荷（后端每次
 * emit 付一次 O(n log n) 全表聚合 + 载荷序列化 + 跨进程搬运），后者只是首页流向图要的一声「完整活动表
 * 变了」（一个时间戳），拿到后首页自己按画布槽位去拉**有界**投影。首页只订 `topology`：订成 `aggregate`
 * 会让首页在场时那次聚合永远白做。两者都算连接流的需求方，全部归零后端才停流。
 */
export type StatsTopic = 'stats' | 'aggregate' | 'topology' | 'detail' | 'closed';

/**
 * topic → 事件推送通道（订阅即回的初始帧 + 后续增量 push 共用同一通道）。主/渲两侧订阅与 relay 的单一真值，
 * 防裸字符串通道名两处漂移（tsc 不查字符串值相等，见 IPC 通道收敛陷阱教训）。
 */
export const STATS_TOPIC_EVENT: Record<StatsTopic, IpcChannel> = {
  stats: IPC_CHANNELS.EVENT_STATS_UPDATED,
  aggregate: IPC_CHANNELS.EVENT_CONNECTIONS_AGGREGATE,
  topology: IPC_CHANNELS.EVENT_CONNECTIONS_TOPOLOGY_CHANGED,
  detail: IPC_CHANNELS.EVENT_CONNECTIONS_DETAIL,
  closed: IPC_CHANNELS.EVENT_CONNECTIONS_CLOSED,
};

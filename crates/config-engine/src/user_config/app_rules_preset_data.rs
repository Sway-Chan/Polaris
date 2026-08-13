// 内置应用分流预设表 —— **单一真值（SoT）**。16 条，行是维护单元。
//
// 曾经的形态（已终结）：本表自称「来源 = src/shared/app-rules-preset.ts（1:1 提取）」，即 TS 是真源、
// 本表是手抄投影 —— 同一份数据两处维护，必然漂移。现在反过来：**本表是唯一数据源**，前端经
// `app_presets_list` command 一次 invoke 拉取（`ui/src/shared/app-rules-preset.ts` 已无表）。
//
// 为何 UI 列（labelKey/emoji/iconUrl）也在这里 —— 不违背「UI 层不进 config-engine」：
//   ① 它们是**表内数据**不是 UI 逻辑（先例：`category` 早已在此，且注释自认「后端不消费」）。
//   ② **按列切表 = 加一条预设要改两个语言的两个文件**，正是本表要消灭的漂移形态。
//   ③ 真实边界由**类型**守住，不由文件守住：路由生成消费的 `AppPreset` **不含** UI 列
//      （`all_presets()` 投影），UI 列只走 `all_presets_dto()` → `AppPresetDto`。builder 零污染。
// 详见 ~/docs/polaris/design/polaris-dialog-layer-and-governance.md §3.2。

/// Qure Color 彩色图标集基址。上游 `QURE_BASE`。
///
/// 用 `macro_rules!` + `concat!` 而非 `const`：`concat!` 只接受字面量，无法拼 const；
/// 而 `format!` 不是 const 上下文。宏展开后仍是 `&'static str`，零运行时开销、零依赖。
macro_rules! qure {
    ($file:literal) => {
        concat!(
            "https://fastly.jsdelivr.net/gh/Koolson/Qure/IconSet/Color/",
            $file
        )
    };
}

/// 预设数据行（扁平 &'static str 数组，运行时投影为 `AppPreset` / `AppPresetDto`）。
struct PresetRaw {
    id: &'static str,
    /// i18n key，对应 `rules.apps.XXX`（UI 列）。
    label_key: &'static str,
    /// iconUrl 加载失败时的兜底图标（UI 列）。
    emoji: &'static str,
    /// 彩色图标集 URL（UI 列）。
    icon_url: Option<&'static str>,
    geosite_tags: &'static [&'static str],
    geoip_tags: &'static [&'static str],
    process_names: &'static [&'static str],
    category: &'static str,
}

const PRESETS_RAW: &[PresetRaw] = &[
    // ── 视频 ──────────────────────────────────────────
    PresetRaw {
        id: "youtube",
        label_key: "youtube",
        emoji: "▶️",
        icon_url: Some(qure!("YouTube.png")),
        geosite_tags: &["youtube"],
        geoip_tags: &[],
        process_names: &[],
        category: "video",
    },
    PresetRaw {
        id: "netflix",
        label_key: "netflix",
        emoji: "🎬",
        icon_url: Some(qure!("Netflix.png")),
        geosite_tags: &["netflix"],
        // geoip-netflix 覆盖 Netflix 的 CDN 直连 IP（如 AWS/Akamai 上的 Netflix 专属 IP 段）
        geoip_tags: &["netflix"],
        process_names: &["Netflix", "Netflix.exe"],
        category: "video",
    },
    PresetRaw {
        id: "tiktok",
        label_key: "tiktok",
        emoji: "🎵",
        icon_url: Some(qure!("TikTok.png")),
        geosite_tags: &["tiktok"],
        geoip_tags: &[],
        process_names: &["TikTok", "TikTok.exe"],
        category: "video",
    },
    // ── 社交 ──────────────────────────────────────────
    PresetRaw {
        id: "telegram",
        label_key: "telegram",
        emoji: "✈️",
        icon_url: Some(qure!("Telegram.png")),
        geosite_tags: &["telegram"],
        // geoip-telegram 覆盖 Telegram DC（数据中心）的 IP 段，确保 DC IP 直连也走代理
        geoip_tags: &["telegram"],
        process_names: &["Telegram", "Telegram.exe", "Telegram Desktop"],
        category: "social",
    },
    PresetRaw {
        id: "twitter",
        label_key: "twitter",
        emoji: "🐦",
        icon_url: Some(qure!("X.png")),
        geosite_tags: &["twitter"],
        // geoip-twitter 覆盖 Twitter/X 使用 QUIC 协议时直连的 IP 段
        // 这是修复 Twitter 在系统代理模式下 UDP 流量不走代理的关键
        geoip_tags: &["twitter"],
        process_names: &["Twitter", "X", "Twitter.exe"],
        category: "social",
    },
    PresetRaw {
        id: "instagram",
        label_key: "instagram",
        emoji: "📷",
        icon_url: Some(qure!("Instagram.png")),
        geosite_tags: &["instagram"],
        geoip_tags: &[],
        process_names: &["Instagram", "Instagram.exe"],
        category: "social",
    },
    // ── AI ────────────────────────────────────────────
    PresetRaw {
        id: "openai",
        label_key: "openai",
        emoji: "🤖",
        icon_url: Some(qure!("ChatGPT.png")),
        geosite_tags: &["openai"],
        geoip_tags: &[],
        process_names: &["ChatGPT", "ChatGPT.exe"],
        category: "ai",
    },
    PresetRaw {
        id: "anthropic",
        label_key: "anthropic",
        emoji: "🧠",
        icon_url: Some(
            "https://raw.githubusercontent.com/lige47/QuanX-icon-rule/main/icon/04ProxySoft/claude.png",
        ),
        // 注意：SagerNet geosite 数据库中没有独立的 geosite-anthropic.srs
        // 使用 geosite-category-ai 兜底（包含 OpenAI/Anthropic/Gemini 等主流 AI 服务）
        // 同时加入 claude.ai 域名通过自定义规则覆盖
        geosite_tags: &["anthropic", "category-ai"],
        geoip_tags: &[],
        process_names: &["Claude", "Claude.exe"],
        category: "ai",
    },
    PresetRaw {
        id: "gemini",
        label_key: "gemini",
        emoji: "✨",
        icon_url: Some(
            "https://raw.githubusercontent.com/lige47/QuanX-icon-rule/main/icon/04ProxySoft/gemini.png",
        ),
        // 使用 geosite-google 同时覆盖 Gemini 所有相关域名（gemini.google.com 等）
        // 注意：如果用户同时启用了 Google 应用分流，两者共享同一个 rule_set 不会冲突
        // sing-box 会自动去重，不会重复下载
        geosite_tags: &["google"],
        geoip_tags: &[],
        process_names: &[],
        category: "ai",
    },
    // ── 工具 ──────────────────────────────────────────
    PresetRaw {
        id: "github",
        label_key: "github",
        emoji: "🐙",
        icon_url: Some(qure!("GitHub.png")),
        geosite_tags: &["github"],
        geoip_tags: &[],
        process_names: &["GitHub Desktop", "GitHubDesktop.exe", "git", "git.exe", "GitHub"],
        category: "tools",
    },
    PresetRaw {
        id: "google",
        label_key: "google",
        emoji: "🔍",
        icon_url: Some(qure!("Google_Search.png")),
        geosite_tags: &["google"],
        geoip_tags: &[],
        process_names: &[],
        category: "tools",
    },
    PresetRaw {
        id: "spotify",
        label_key: "spotify",
        emoji: "🎧",
        icon_url: Some(qure!("Spotify.png")),
        geosite_tags: &["spotify"],
        geoip_tags: &[],
        process_names: &["Spotify", "Spotify.exe"],
        category: "tools",
    },
    // ── 游戏 ──────────────────────────────────────────
    PresetRaw {
        id: "steam",
        label_key: "steam",
        emoji: "🎮",
        icon_url: Some(qure!("Steam.png")),
        geosite_tags: &["steam"],
        geoip_tags: &[],
        // 包含 Steam 客户端及常见热门游戏的进程名
        // 游戏通过 Steam 启动后是独立进程，仅匹配 "Steam" 无法覆盖游戏本体的 UDP 流量
        process_names: &[
            // Steam 客户端及辅助进程（steam_osx = macOS Steam binary）
            "Steam", "steam.exe", "steamwebhelper", "steamwebhelper.exe",
            "GameOverlayUI.exe", "GameOverlayUI", "steam_osx",
            // 热门 FPS / 竞技游戏（CS2 / Dota 2 / PUBG / Apex / Tarkov / Fortnite）
            "cs2", "cs2.exe", "dota2", "dota2.exe",
            "TslGame.exe", "TslGame", "r5apex.exe", "r5apex",
            "EscapeFromTarkov.exe", "FortniteClient-Win64-Shipping.exe",
            // MOBA / RPG（炉石 / 原神 / 星穹铁道 / 绝区零）
            "Hearthstone.exe", "Hearthstone",
            "GenshinImpact.exe", "YuanShen.exe", "StarRail.exe", "ZenlessZoneZero.exe",
            // 生存 / 沙盒（Valheim / Rust / 幻兽帕鲁）
            "valheim.exe", "valheim", "RustClient.exe", "Palworld-Win64-Shipping.exe",
        ],
        category: "game",
    },
    PresetRaw {
        id: "epic",
        label_key: "epic",
        emoji: "🎮",
        icon_url: Some(
            "https://raw.githubusercontent.com/lige47/QuanX-icon-rule/main/icon/07Game/epicgames.png",
        ),
        geosite_tags: &["epicgames"],
        geoip_tags: &[],
        process_names: &[
            "EpicGamesLauncher", "EpicGamesLauncher.exe", "EpicWebHelper.exe",
            "FortniteClient-Win64-Shipping.exe", "UnrealEngineLauncher.exe",
        ],
        category: "game",
    },
    PresetRaw {
        id: "riot",
        label_key: "riot",
        emoji: "⚔️",
        icon_url: Some(
            "https://raw.githubusercontent.com/lige47/QuanX-icon-rule/main/icon/07Game/riot.png",
        ),
        geosite_tags: &["riot"],
        geoip_tags: &[],
        process_names: &[
            "RiotClientServices.exe", "RiotClientServices",
            // 英雄联盟客户端 / 游戏本体 / Valorant / Legends of Runeterra
            "LeagueClient.exe", "LeagueClient",
            "League of Legends.exe", "League of Legends",
            "VALORANT-Win64-Shipping.exe", "LoR.exe",
        ],
        category: "game",
    },
    PresetRaw {
        id: "disney",
        label_key: "disney",
        emoji: "🏰",
        icon_url: Some(qure!("Disney.png")),
        geosite_tags: &["disney"],
        geoip_tags: &[],
        process_names: &["Disney+"],
        category: "video",
    },
];

/// 路由生成投影：`Vec<AppPreset>`（**不含 UI 列** —— builder 消费面零污染）。
pub(crate) fn all_presets() -> Vec<AppPreset> {
    PRESETS_RAW
        .iter()
        .map(|r| AppPreset {
            id: r.id.to_string(),
            geosite_tags: r.geosite_tags.iter().map(|s| s.to_string()).collect(),
            geoip_tags: r.geoip_tags.iter().map(|s| s.to_string()).collect(),
            process_names: r.process_names.iter().map(|s| s.to_string()).collect(),
            category: r.category.to_string(),
        })
        .collect()
}

/// 全列投影：`Vec<AppPresetDto>`（含 UI 列）—— `app_presets_list` command 下发前端。
pub fn all_presets_dto() -> Vec<AppPresetDto> {
    PRESETS_RAW
        .iter()
        .map(|r| AppPresetDto {
            id: r.id.to_string(),
            label_key: r.label_key.to_string(),
            emoji: r.emoji.to_string(),
            icon_url: r.icon_url.map(str::to_string),
            geosite_tags: r.geosite_tags.iter().map(|s| s.to_string()).collect(),
            geoip_tags: r.geoip_tags.iter().map(|s| s.to_string()).collect(),
            process_names: r.process_names.iter().map(|s| s.to_string()).collect(),
            category: r.category.to_string(),
        })
        .collect()
}

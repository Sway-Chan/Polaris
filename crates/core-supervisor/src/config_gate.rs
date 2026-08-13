//! 起核前的**内核闸门**：把即将下发的那份 config 真交给 `sing-box check`，让内核自己点名它拒收的
//! 是哪一个节点，从而把「一个坏节点炸掉整份配置」变成「剔掉那个节点、其余照常起」。
//!
//! # 为什么这道门必须存在
//!
//! custom 节点的内核有效性此前**只由「测试内核兼容性」按钮**（`commands/proxy.rs::kernel_probe_outbound`）
//! 把关，而那是一个用户可以不点的自愿动作；生成侧的 `build_outbounds` 会把**所有**节点写进最终配置。
//! 两件事叠在一起 ⇒ 一个内核拒收的节点让整份配置 FATAL、**全局起不来**，用户看到的只有「启动失败」。
//! 触发面在两处修复后被放大：① custom 出站改成真透传（用户 raw JSON 原样进配置）；② 本机导入把未映射
//! 类型自动包成 custom 节点。二者都是对的修复，但都让「坏节点进配置」更容易发生。
//!
//! # 为什么判据是 `sing-box check` 而不是我方白名单
//!
//! 已定口径：**不在导入侧或生成侧加协议白名单** —— 那会把逃生舱重新变成白名单，且必与内核版本漂移
//! （随包核换一次，白名单就错一次）。「这个 type 内核认不认」只有内核自己有资格回答，故本模块把它
//! 原样问给内核，与 C10 probe 按钮同一个权威来源（见 `commands/proxy.rs` 的「为何不复用 C3」段）。
//!
//! # 策略：整份 check 一次 + 按内核给的下标剥离，**不是**逐节点 check、**也不是**二分
//!
//! 随包核 `resources/linux/sing-box`（自报 1.14.0-beta.7）实测（本机墙钟，×10~12 次取全量样本）：
//!
//! | 配置形态                                              | 单次 `check` 墙钟 |
//! |-------------------------------------------------------|-------------------|
//! | 最小 probe 形状（2 outbound，无 rule_set）            | 13–14 ms          |
//! | 生产形状（TUN + mixed + 23 个本地 `.srs` + DNS），4 出站 | 25–29 ms          |
//! | 同上但 **119 个节点**（真机实际规模）                  | 28–32 ms          |
//!
//! 关键事实是**边际成本近似为零**：4 → 119 个节点只多 ~3ms，26ms 的地板是 Go runtime 启动 + 23 个
//! `.srs` 装载。于是：
//!
//! - **逐节点 check** = 119 × 13ms ≈ **1.7 s**（还得为每个节点另组一份最小 config），每次起核都付
//!   ——为一个绝大多数时候什么都不会发现的检查，给起核加 1.7 秒，不可接受。
//! - **二分定位** = 每个坏节点 log₂(119) ≈ 7 次 check ≈ 200ms，且要造 7 份中间配置。
//! - **本模块所用**：内核的诊断行**自带下标**（`outbounds[7]` / `initialize outbound[3]`，见
//!   [`parse_kernel_rejection`] 的实测语法表），一次 check 就直接点名了坏节点，二分是在重新求解一个
//!   内核已经免费告诉我们的答案 ⇒ **严格劣于**本方案，故不采用。
//!
//! 代价因此是：健康路径 **恒 1 次 check ≈ 30ms**；有 K 个坏节点时 K+1 次。
//!
//! # `check` 不碰网络（这条是硬约束，实测取证而非假定）
//!
//! `strace -f -e trace=socket,bind,connect,listen` 跑生产形状（含 TUN inbound + 管理 API + 119 节点）
//! 的 `check`：**socket/bind/connect/listen 计数为 0**，且非 root 可跑。正向对照同一 strace 表达式能
//! 抓到 loopback `connect`（`connect(3, {AF_INET, 127.0.0.1:9}) = -1 ECONNREFUSED`）⇒ 不是 strace 没抓到，
//! 是真的一个都没有。这也意味着本门**不会**去抢 mixed 口/管理口 —— check 只解析与构造，不 Start。
//!
//! # 门的边界（说清楚它抓不到什么，比夸大它抓得到什么重要）
//!
//! `check` 只跑 **decode（JSON→结构）+ initialize（构造各组件）** 两阶段，**不跑 Start**。故：
//! - **抓得到**：未知 type、未知子键、字段类型冲突、非法枚举值、缺 TLS 之类的构造期语义校验。
//! - **抓不到**：Start 期才解析的依赖引用。实测 `selector.outbounds` 指向不存在的 tag，`check` **rc=0**，
//!   而真起核时是 `dependency[X] not found`（即 `start_inner` 里 DESIGN-REVIEW(fx-proxy-a-runstart-retry-partial)
//!   记的那条幽灵引用腿）。本门不冒充覆盖那一类。

use std::path::Path;
use std::time::Duration;

/// 单次 `sing-box check` 的超时。
///
/// 实测生产形状 26–29ms（见模块头注表），5s ≈ 170× 余量，留给冷启首次读 79MB 核二进制、
/// 慢盘、以及 Windows 上杀软对新进程的扫描。**超时不阻断起核**（→ [`ConfigCheckVerdict::Unavailable`]
/// → fail-open），故这个上限只决定「最坏情况给起核多加多久」，不决定正确性。
///
/// 刻意**短于** `commands/proxy.rs::PROBE_CHECK_TIMEOUT`（8s）：那是用户主动点按钮、只等一个结果的
/// 交互动作，等 8s 尚可；这里挂在**每次起核**的关键路径上，8s 的停顿会被当成「连接卡死」。
pub const CONFIG_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// 整个剥离循环的**墙钟预算**（不是轮数上限）。
///
/// # 为什么预算用时间而不是「最多剥 N 个」
///
/// 用户真正会痛的是「点了连接之后要多等多久」，而每轮的成本是**机器相关**的（本机 ~29ms，慢盘/杀软
/// 环境可能十倍）。写死轮数会在快机上白白放弃本可剔掉的节点、在慢机上把起核拖成十几秒；写成时间预算
/// 则两头自适应：本机约合 50 轮，一台慢 10 倍的机器自动收敛到 5 轮，**用户感受到的上限不变**。
///
/// 循环的**终止**不靠这个预算，靠「每轮必须剥掉一个**新** id」这条推进不变式（见
/// [`PeelStep`]）—— 归因不到、或归因到已剥过的 id，立刻停。预算只封顶延迟。
///
/// 1.5s 的取法：健康路径根本走不到（恒 1 轮 ~30ms），只有「一堆节点逐个被内核拒收」的病态配置才会逼近
/// 它；此时多花 1.5s 换来「其余节点能用」，比 0.03s 换来「一个都用不了」值。
pub const PEEL_TIME_BUDGET: Duration = Duration::from_millis(1500);

/// 被内核闸门剔除的节点在 `InvalidNodeInfo.reason` 上的判别符。
///
/// 与 `config-engine` 的 `INVALID_REASON_DETOUR_CASCADE` / `INVALID_REASON_CUSTOM_MALFORMED` 同族、
/// 走**同一条**上报通道（`InvalidNode` → `EVENT_PROXY_INVALID_NODES` → 节点卡标灰 + tooltip），
/// 不是新造的机制。
///
/// **为什么 token 定义在这里而不是和另外两个并排放在 `config-engine/builder/outbounds.rs`**：那两个由
/// **生成期**的静态判据产生，而本 token 由**起核期**跑真内核产生，产生点根本不在 config-engine 里；
/// 把常量放在产生点之外，就会重演「reason 写死在 generate.rs、成因多于一种后 tooltip 报无关成因」
/// 那个已经修过一次的坑（见 `OutboundsDeps::gate_invalid_nodes` 字段文档）。
///
/// **为什么不复用 `custom-outbound-malformed`**：那条的语义是「outbound JSON 不是带 string `type` 的
/// 对象」——一个我方**静态**判据；本条是「形状合法、但这个内核不认」。混用会让 tooltip 对用户说谎，
/// 而 tooltip 的那半句恰恰是唯一说明「为什么」的部分。
pub const INVALID_REASON_KERNEL_REJECTED: &str = "kernel-rejected";

/// 内核点名的是哪个数组。**`outbounds[]` 与 `endpoints[]` 各自独立编号**，下标不可跨数组解释
/// （实测：同一份配置里 `outbounds[7]` 与 `endpoints[1]` 指的是两个数组各自的第 7/第 1 项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectedArray {
    Outbounds,
    Endpoints,
}

/// 内核对某一个下标的拒收。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelRejection {
    pub array: RejectedArray,
    /// 该数组内的下标（与我方序列化出去的数组顺序同一个坐标系 —— 内核解析的就是我方写的那份 JSON）。
    pub index: usize,
    /// 内核原话，从数组 token 起到行尾（如 `outbounds[0].obfs: unknown obfs type: bogus`）。
    /// **不翻译**：第三方内核吐的英文诊断，翻了反而对不上用户搜到的 issue（同
    /// `commands/proxy.rs::ProbeDiagnostic::message` 的口径）。
    pub detail: String,
}

/// 一次 `sing-box check` 的判定。
///
/// 三态而非布尔，理由与 `commands/proxy.rs::ProbeCheck` 完全一致：**「核不可用」不等于「配置无效」**。
/// 把二者合成一个 bool，就必然要在「核缺失时判所有节点无效」和「核缺失时判所有节点有效」之间二选一，
/// 而正确答案是「不知道」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCheckVerdict {
    /// rc=0 —— 这份配置内核收下了。
    Accepted,
    /// rc≠0 且诊断行拆得出数组下标 —— 内核点名了某一项。
    Rejected(KernelRejection),
    /// rc≠0 但**拆不出下标**：route 层错（`route.rules[0]: ...`）、tag 重复
    /// （`duplicate outbound/endpoint tag: d`）、`initialize router: ...`、陌生格式。
    /// 携带内核原话，**绝不归因到任何节点**（乱剥一个好节点比不剥更坏）。
    Unattributable(String),
    /// 核不存在 / spawn 失败 / 超时 —— **无法判定**（failOpen）。
    Unavailable(String),
}

/// 剥离循环每一轮的决策（**纯函数** [`decide_peel`] 的产物，便于无进程无内核地单测整条控制流）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeelStep {
    /// 放行 —— 把这份配置交给 spawn。
    Proceed,
    /// 剥掉这个下标指向的节点，重新生成后再来一轮。
    Peel(KernelRejection),
    /// 不再剥，按现状放行（fail-open）。携带停下来的原因，供日志诚实说明「门这次没起作用」。
    Stop(String),
}

/// 一轮剥离的决策。**纯函数**：不碰进程、不碰 FS、不读时钟（`elapsed` 由调用方注入）。
///
/// # fail-open 还是 fail-closed
///
/// 除 [`ConfigCheckVerdict::Rejected`] 外的每一条腿都 **fail-open**（放行到 spawn，让内核自己去报错），
/// 这与本仓既有的两处口径对齐、且方向相反的那一处也对得上：
///
/// - **同向（fail-open）**：`commands/proxy.rs::run_probe_check` —— 「核缺失/慢盘 ≠ 不兼容」，
///   spawn 失败与超时都判 `Indeterminate` 而非「不支持」。
/// - **反向（fail-closed）**：`route.rs` 对缺失 `.srs` 的剪枝（`pruned_rule_set_tags`）——那里**必须**
///   关，因为「规则集悄悄没了」的后果是**流量静默直连**，用户以为在走代理。
///
/// 分界线是**失败模式是否静默**：规则集缺失会静默地改变分流结果，故关；而本门失手的后果是内核照旧
/// FATAL —— 一个**响亮**的、已经有专门真因收集（`start_inner` 的 `CoreFatalSlot` / `classify_core_fatal_line`）
/// 在盯着的失败。此时 fail-closed 只会把「一个坏节点起不来」升级成「核临时读不到就一个节点都用不了」，
/// 那是拿一个更大的故障去换一个更小的。
pub fn decide_peel(verdict: &ConfigCheckVerdict, elapsed: Duration, budget: Duration) -> PeelStep {
    match verdict {
        ConfigCheckVerdict::Accepted => PeelStep::Proceed,
        ConfigCheckVerdict::Unavailable(why) => {
            PeelStep::Stop(format!("内核不可用，闸门跳过（failOpen）：{why}"))
        }
        ConfigCheckVerdict::Unattributable(why) => {
            PeelStep::Stop(format!("内核拒收但无法归因到具体节点，照原样下发：{why}"))
        }
        // 预算判定放在 Rejected 之后：只有「确实还要再剥一轮」时超预算才有意义。前面三条腿都是
        // 单轮终态，拿预算去拦它们只会把「本可以放行」变成「日志里多一句吓人的超预算」。
        ConfigCheckVerdict::Rejected(_) if elapsed >= budget => PeelStep::Stop(format!(
            "剥离预算 {}ms 已用尽（已耗 {}ms），剩余坏节点交给内核自己报错",
            budget.as_millis(),
            elapsed.as_millis()
        )),
        ConfigCheckVerdict::Rejected(r) => PeelStep::Peel(r.clone()),
    }
}

/// 从 `sing-box check` 的原始输出里拆出「内核点名的是哪个数组的第几项」。**纯函数**，不碰进程/IO。
///
/// # 实测语法（随包 `resources/linux/sing-box`，逐条真跑构造的坏 config）
///
/// 首次实测于自报 1.14.0-beta.7 的随包核。**抬核到 1.14.0-beta.12 时逐条重测过**：下列六种形态
/// （含 decode 用复数 `outbounds[]` / initialize 用单数 `outbound[]` 这个本函数依赖的区分）**逐字未变**。
///
/// 这条重测不能省：本模块的单测走的是桩探针 `src/bin/check_probe.rs` 那套固定样本，
/// **不跑真核** ⇒ 换核后它的绿对「真核还是不是这个形状」零信息量。换核必须手工重测本节。
///
/// 可归因（本函数返 `Some`）：
///
/// ```text
/// FATAL[0000] decode config at t.json: outbounds[7]: unknown outbound type: nonexistent-proto
/// FATAL[0000] decode config at t.json: outbounds[0].obfs: unknown obfs type: bogus
/// FATAL[0000] decode config at t.json: outbounds[0].unknown_key: json: unknown field "unknown_key"
/// FATAL[0000] decode config at t.json: endpoints[1]: unknown endpoint type: nope
/// FATAL[0000] initialize outbound[3]: unknown method: bad-a
/// FATAL[0000] initialize endpoint[1]: decode private key: illegal base64 data at input byte 3
/// ```
///
/// 不可归因（本函数返 `None`）：
///
/// ```text
/// FATAL[0000] decode config at t.json: duplicate outbound/endpoint tag: d
/// FATAL[0000] decode config at t.json: route.rules[0]: unknown rule action: nope
/// FATAL[0000] initialize router: parse rule-set[0]: open …: no such file or directory
/// ```
///
/// 两阶段的用词不同（decode 用复数 `outbounds[]`、initialize 用单数 `outbound[]`），故两种写法都认；
/// 数组身份取自词干而非取自分支，这样万一哪个版本把用词统一了也不会漏。
///
/// # 为什么用 `strip_prefix` 精确锚定，而不是在整行里搜 `outbounds[`
///
/// 满行子串搜索会把**消息正文更深处**碰巧出现的同名 token 当成路径。最要紧的现场是
/// `initialize router: parse rule-set[0]: open <path>: …`（实测样本）里的 `<path>` —— 规则文件名与
/// 数据目录都含用户控制的成分，一个叫 `outbounds[1].srs` 的自定义规则文件就足以让松匹配读出下标 1，
/// 把一个毫不相干、本来能用的节点静默剥掉。
///
/// 判据共三处、须同时成立：marker 之后**紧接着**就是数组 token、token 内是纯十进制、`]` 之后紧跟
/// `.`/`:`。三者**有重叠但不冗余**：`duplicate outbound/endpoint tag: outbounds[9]` 这类回显由第三处
/// 挡下，上述 rule-set 路径那类只有第一处挡得住（两条各有变异锁，见
/// `array_token_deeper_in_the_message_is_not_a_key_path` 与 `malformed_index_tokens_are_rejected`）。
///
/// **宁可归因不到（fail-open，最坏是回到今天的行为），也绝不错误归因**（那会剥掉一个本来能用的
/// 节点，且用户完全无从察觉）。
///
/// # 不做 ANSI 剥离
///
/// 本模块给 `check` 传 `--disable-color`（实测该 flag 在 `check` 子命令上被接受，stderr 逐字节确认
/// 无 `\x1b[` 序列）。`commands/proxy.rs` 那边的 `strip_ansi` 是在没传这个 flag 的前提下必需的；
/// 这里从源头关掉，就没有需要剥的东西。
///
/// # 多行输出取哪一条
///
/// 取**最后一条非空行**，理由同 `parse_probe_diagnostic`：Go `log.Fatal` 记一行即 `os.Exit`，真正终止
/// 进程的诊断永远是最后一行；若未来版本在 FATAL 前加了前置噪声，取最后一行仍然对。
#[must_use]
pub fn parse_kernel_rejection(raw: &str) -> Option<KernelRejection> {
    const DECODE_MARKER: &str = "decode config at ";
    const INIT_MARKER: &str = "initialize ";

    let line = raw.lines().rev().find(|l| !l.trim().is_empty())?.trim();

    // marker 用 `find` 而非 `strip_prefix`：日志框架前缀 `FATAL[0000] ` 挡在前面，锚定行首会全数错过。
    if let Some(i) = line.find(DECODE_MARKER) {
        let after_marker = &line[i + DECODE_MARKER.len()..];
        // after_marker = "<配置文件路径>: <路径段>: <消息>"。用第一个 `": "` 跳过文件名 —— 分隔符恒是
        // 冒号+空格，而路径里的冒号（Windows 盘符 `C:\`）后面跟的是反斜杠不是空格（这条实测依据见
        // `commands/proxy.rs::parse_probe_diagnostic` 的「文件路径含冒号」小节，此处不重复取证）。
        // 万一真切错了，切出来的东西不会以数组 token 开头 ⇒ 落到 None ⇒ fail-open，不会错剥。
        if let Some((_file, rest)) = after_marker.split_once(": ") {
            if let Some(r) = match_array_segment(rest) {
                return Some(r);
            }
        }
    }
    if let Some(i) = line.find(INIT_MARKER) {
        if let Some(r) = match_array_segment(&line[i + INIT_MARKER.len()..]) {
            return Some(r);
        }
    }
    None
}

/// `s` 是否以 `outbound(s)[N]` / `endpoint(s)[N]` 开头；是则拆出数组与下标。
fn match_array_segment(s: &str) -> Option<KernelRejection> {
    // 复数在前：`strip_prefix("outbound[")` 对 `"outbounds[7]"` 本就不成立（第 9 字节是 's' 不是 '['），
    // 故顺序不构成歧义；并列只是为了同时认下 decode 的复数与 initialize 的单数两种用词。
    const PREFIXES: &[(&str, RejectedArray)] = &[
        ("outbounds[", RejectedArray::Outbounds),
        ("outbound[", RejectedArray::Outbounds),
        ("endpoints[", RejectedArray::Endpoints),
        ("endpoint[", RejectedArray::Endpoints),
    ];
    for (prefix, array) in PREFIXES {
        let Some(body) = s.strip_prefix(prefix) else {
            continue;
        };
        let close = body.find(']')?;
        let digits = &body[..close];
        // **显式要求全 ASCII 数字且非空**，不能只靠 `usize::from_str` —— 实测（本模块单测抓到的）
        // `usize::from_str("+1")` 返回的是 `Ok(1)` 而不是 Err，即标准库接受前导 `+`。放任它意味着
        // 「`outbound[+1]` 也算键路径」，而 sing-box 从不会吐这种下标 ⇒ 一旦出现就说明格式已经变了，
        // 此时正确动作是 fail-open（返 None），不是拿一个猜出来的下标去剥节点。
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let index: usize = digits.parse().ok()?;
        // token 之后必须紧跟 `.`（子键路径）或 `:`（消息分隔）。这一条是防「消息正文里恰好出现
        // `outbound[` 字样」的最后一道闸 —— 少了它，一句英文散文也可能被当成键路径。
        let tail = &body[close + 1..];
        if !tail.starts_with('.') && !tail.starts_with(':') {
            return None;
        }
        return Some(KernelRejection {
            array: *array,
            index,
            detail: s.to_string(),
        });
    }
    None
}

/// 真跑 `sing-box <bin> --disable-color check -c <config_path>` 并映射三态。
///
/// **不碰网络**（实测取证见模块头注）：`check` 只做 decode + initialize，不 Start，故不建 socket、
/// 不绑端口 —— 这是它能安全地插在 spawn **之前**的前提（若它会绑 mixed 口，就会和随后的真核抢端口）。
///
/// `stdin` 置 null：`check` 不读标准输入，不置 null 会在无终端的 GUI 进程树里挂住。
pub async fn run_config_check(binary: &Path, config_path: &Path) -> ConfigCheckVerdict {
    run_config_check_within(binary, config_path, CONFIG_CHECK_TIMEOUT).await
}

/// [`run_config_check`] 的可注入超时版本 —— **只为让超时腿可测**。
///
/// 生产恒走 [`CONFIG_CHECK_TIMEOUT`]；测试传 100ms 配上探针的 400ms 睡眠，就能在毫秒级里
/// 既验判决、又验「超时之后子进程真的被杀掉」。没有这个参数的话，唯一的测法是让测试真等 5 秒。
pub async fn run_config_check_within(
    binary: &Path,
    config_path: &Path,
    timeout: Duration,
) -> ConfigCheckVerdict {
    let mut builder = tokio::process::Command::new(binary);
    builder
        // 全局 flag 位（`check` 子命令位亦可，两处实测等效）；不加则 stderr 恒带 ANSI 彩色码，
        // 且 sing-box **不看 stdout/stderr 是否为 tty**，管道里照样上色。
        .arg("--disable-color")
        .arg("check")
        .arg("-c")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // 🔴 超时腿会把 `output()` 的 future 直接丢掉，而 `tokio::process::Child` 的
        // `kill_on_drop` **默认是 false** ⇒ 不置这一行就会留下一个游离的 `sing-box check`。
        // 本门挂在**每次起核**（含每条重试腿）上，不是用户手点一次的一次性动作，
        // 泄漏会随重试累积。见 `times_out_and_kills_the_child` 那条门（带正向对照）。
        .kill_on_drop(true);
    // Windows：宿主是 GUI 子系统进程，起 console 程序（sing-box）会新分配控制台窗口。
    // 本门挂在**每次起核**上 ⇒ 不加就是每次连接闪一次黑框。tokio 无隐含默认，须显式给。
    #[cfg(windows)]
    builder.creation_flags(0x0800_0000);
    let fut = builder.output();
    let raw = match tokio::time::timeout(timeout, fut).await {
        Err(_) => RawCheck::TimedOut {
            after_secs: timeout.as_secs_f32(),
        },
        // spawn 失败（核缺失 ENOENT / 无执行权限 EACCES）→ 无法判定，**不是**配置无效。
        Ok(Err(e)) => RawCheck::SpawnFailed(e.to_string()),
        Ok(Ok(out)) => RawCheck::Done {
            success: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        },
    };
    decide_verdict(raw)
}

/// [`decide_verdict`] 的输入 —— 把「子进程跑成什么样」从 tokio 类型里剥出来。
///
/// 存在的唯一理由是**让每条腿都可测**：`tokio::time::error::Elapsed` 没有公开构造器，
/// 直接对 `Result<Result<Output, io::Error>, Elapsed>` 写判定就意味着超时腿只能靠真等来覆盖，
/// 于是它事实上一条门都没有 —— 实测把超时腿改成返回 `Accepted`、或改成
/// `Rejected(outbounds[0])` 去剥一个无辜好节点，整套门都保持全绿。
#[derive(Debug)]
enum RawCheck {
    TimedOut {
        after_secs: f32,
    },
    SpawnFailed(String),
    Done {
        success: bool,
        stderr: String,
        stdout: String,
    },
}

/// 三态映射。**纯函数**，无 I/O。
///
/// fail-open 口径：只有「非零退出 + 能归因到某个下标」才是 `Rejected`；超时与 spawn 失败一律
/// `Unavailable`（= 闸门无从判定 ⇒ 放行到 spawn，由内核自己报错）。分界线是失败模式是否静默 ——
/// 本门失手的后果是内核照旧 FATAL，一个响亮的、已有 `CoreFatalSlot` 盯着的失败；
/// 而 fail-closed 只会把「一个坏节点起不来」升级成「核临时读不到就一个节点都用不了」。
fn decide_verdict(raw: RawCheck) -> ConfigCheckVerdict {
    let (success, stderr, stdout) = match raw {
        RawCheck::TimedOut { after_secs } => {
            return ConfigCheckVerdict::Unavailable(format!("check 超时（>{after_secs}s）"))
        }
        RawCheck::SpawnFailed(e) => {
            return ConfigCheckVerdict::Unavailable(format!("check 启动失败: {e}"))
        }
        RawCheck::Done {
            success,
            stderr,
            stdout,
        } => (success, stderr, stdout),
    };
    if success {
        return ConfigCheckVerdict::Accepted;
    }
    // stderr 优先、为空才落回 stdout：实测 1.14.0-beta.7 的 FATAL 恒走 stderr（stdout 全空），
    // 留 stdout 兜底给理论上把日志导向 stdout 的变体/未来版本（同 `run_probe_check` 的处理）。
    let raw = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    match parse_kernel_rejection(raw) {
        Some(r) => ConfigCheckVerdict::Rejected(r),
        None => ConfigCheckVerdict::Unattributable(if raw.is_empty() {
            // check 非零退出但双流全空的病态腿：不能报空串，否则日志里只剩一句没有内容的「拒收」。
            "check 非零退出但无任何输出".to_string()
        } else {
            raw.to_string()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rej(raw: &str) -> Option<KernelRejection> {
        parse_kernel_rejection(raw)
    }

    /// 🔴 **变异锁：decode 阶段的下标必须解出来**。
    ///
    /// 样本逐字取自随包 `resources/linux/sing-box`（1.14.0-beta.7）对构造坏 config 的真实 stderr
    /// （`--disable-color`，故无 ANSI）。变异：把 `match_array_segment` 的 `"outbounds["` 改成别的串，
    /// 或让 `parse_kernel_rejection` 恒返 None ⇒ 本条全断。
    #[test]
    fn decode_phase_rejection_carries_index() {
        let r = rej("FATAL[0000] decode config at t.json: outbounds[7]: unknown outbound type: nonexistent-proto")
            .expect("decode 期诊断必须解出下标");
        assert_eq!(r.array, RejectedArray::Outbounds);
        assert_eq!(r.index, 7);
        assert!(
            r.detail.starts_with("outbounds[7]:"),
            "detail 从数组 token 起"
        );

        let r =
            rej("FATAL[0000] decode config at t.json: endpoints[1]: unknown endpoint type: nope")
                .expect("endpoints 腿");
        assert_eq!(r.array, RejectedArray::Endpoints);
        assert_eq!(r.index, 1);
    }

    /// 🔴 **变异锁：子键路径（`outbounds[0].obfs`）不得因为 token 后面不是 `:` 就被丢掉**。
    ///
    /// 这类是 custom 透传最常见的坏法（用户 JSON 里某个子块的枚举值写错）。变异：把 `tail.starts_with('.')`
    /// 那一支删掉 ⇒ 本条断在 `expect`。
    #[test]
    fn decode_phase_subkey_path_still_attributes_to_the_node() {
        for (raw, idx) in [
            ("FATAL[0000] decode config at t.json: outbounds[0].obfs: unknown obfs type: bogus", 0),
            ("FATAL[0000] decode config at t.json: outbounds[12].transport: unknown transport type: nope", 12),
            (r#"FATAL[0000] decode config at t.json: outbounds[3].unknown_key: json: unknown field "unknown_key""#, 3),
        ] {
            let r = rej(raw).unwrap_or_else(|| panic!("子键路径也必须归因：{raw}"));
            assert_eq!(r.array, RejectedArray::Outbounds);
            assert_eq!(r.index, idx);
        }
    }

    /// 🔴 **变异锁：initialize 阶段用的是单数 `outbound[N]`，两种用词都得认**。
    ///
    /// 变异：只留复数前缀 ⇒ 本条断。这条腿覆盖的是「JSON 结构合法但构造期语义不过」的一大类
    /// （`unknown method` / `TLS required` / `decode private key` 实测均走此格式）。
    #[test]
    fn initialize_phase_uses_singular_form() {
        let r = rej("FATAL[0000] initialize outbound[3]: unknown method: bad-a")
            .expect("单数 outbound");
        assert_eq!((r.array, r.index), (RejectedArray::Outbounds, 3));
        let r = rej("FATAL[0000] initialize endpoint[1]: decode private key: illegal base64 data at input byte 3")
            .expect("单数 endpoint");
        assert_eq!((r.array, r.index), (RejectedArray::Endpoints, 1));
        let r = rej("FATAL[0000] initialize outbound[0]: TLS required")
            .expect("hysteria2 缺 TLS 实测样本");
        assert_eq!(r.index, 0);
    }

    /// 🔴 **变异锁：拆不出下标的三类真实诊断，一个都不许被归因**。
    ///
    /// 错误归因会剥掉一个本来能用的节点，且用户无从察觉 —— 比不归因坏得多。
    #[test]
    fn non_node_diagnostics_are_never_attributed() {
        for raw in [
            "FATAL[0000] decode config at t.json: duplicate outbound/endpoint tag: d",
            "FATAL[0000] decode config at t.json: route.rules[0]: unknown rule action: nope",
            "FATAL[0000] initialize router: parse rule-set[0]: open /x/geosite-cn.srs: no such file or directory",
            "FATAL[0000] decode config at t.json: json: cannot unmarshal string into Go value",
            "",
            "   ",
        ] {
            assert_eq!(rej(raw), None, "不得归因：{raw}");
        }
    }

    /// 🔴 **变异锁：路径必须从 marker 处紧邻锚定，不能在整行里搜同名 token**。
    ///
    /// # 这条锁的是 `strip_prefix`，且它**独立于**上面那道 `tail` 判据
    ///
    /// 两道守卫有重叠：`duplicate outbound/endpoint tag: outbounds[9]` 这类回显，`tail` 判据
    /// （token 后须紧跟 `.`/`:`）就已经挡住了。真正只有 `strip_prefix` 挡得住的，是**同名 token
    /// 后面恰好也跟着 `.` 或 `:`** 的那一类 —— 下面两条即是（实测：改成 `find` 后它们分别被解成
    /// 下标 1 与 5，而正确答案是「这行说的根本不是某个 outbound」）。
    ///
    /// # 第一条现场是用户可达的，不是凑的
    ///
    /// `initialize router: parse rule-set[0]: open <path>: no such file or directory` 是**实测样本**
    /// （只是路径不同）。而那个 `<path>` 里有用户控制的成分：自定义规则文件名由用户起
    /// （`custom_rules_dir` 下），数据目录路径含用户名。于是只要有一个规则文件叫 `outbounds[1].srs`，
    /// 松匹配就会从**文件名**里读出下标 1，然后把第 1 个 outbound —— 一个毫不相干、本来能用的节点
    /// —— 静默剥掉。用户看到「我的节点无缘无故没了」，而日志里说的是规则文件的事。
    ///
    /// 第二条（`inbound[0]: … outbound[5]: …` 嵌套主语）是**构造形状**，未在 1.14.0-beta.7 观测到，
    /// 按已验证的分隔符规律手工构造，作防御性覆盖（同 `commands/proxy.rs::parse_probe_diagnostic`
    /// 对 Windows 路径那一段的取法）。
    #[test]
    fn array_token_deeper_in_the_message_is_not_a_key_path() {
        for raw in [
            "FATAL[0000] initialize router: parse rule-set[0]: open outbounds[1].srs: no such file or directory",
            "FATAL[0000] initialize inbound[0]: bad outbound[5]: nope",
        ] {
            assert_eq!(
                rej(raw),
                None,
                "非紧邻 marker 的同名 token 不是键路径，据它剥节点会剥掉一个无关的好节点：{raw}"
            );
        }
    }

    /// 🔴 **变异锁：token 后面必须紧跟 `]` + `.`/`:`，散文里的同名字样不算**。
    ///
    /// 变异：删掉 `tail.starts_with` 那道判据，或把 `body[..close].parse()` 换成宽松解析 ⇒ 本条断。
    #[test]
    fn malformed_index_tokens_are_rejected() {
        for raw in [
            // 下标非数字 / 为空 / 带符号
            "FATAL[0000] initialize outbound[abc]: x",
            "FATAL[0000] initialize outbound[]: x",
            "FATAL[0000] initialize outbound[+1]: x",
            "FATAL[0000] initialize outbound[ 1]: x",
            // 有下标但后面既不是 `.` 也不是 `:`（= 这不是键路径，是一句话）
            "FATAL[0000] initialize outbound[1] failed for some reason",
            // 没有闭合括号
            "FATAL[0000] initialize outbound[1 : x",
        ] {
            assert_eq!(rej(raw), None, "不得归因：{raw}");
        }
    }

    /// 取最后一条非空行（Go `log.Fatal` 语义：终止进程的那行永远在最后）。
    #[test]
    fn picks_last_non_empty_line() {
        let raw = "WARN[0000] something noisy\n\nFATAL[0000] initialize outbound[9]: unknown method: x\n\n";
        assert_eq!(rej(raw).expect("取最后一条非空行").index, 9);
    }

    /// 🔴 **变异锁：健康路径放行；三条非 Rejected 腿全部 fail-open（不阻断起核）**。
    ///
    /// 变异：把 `Unavailable`/`Unattributable` 任一支改成阻断（返回 Peel 或让调用方 Err）⇒ 本条断。
    /// 这一条钉的是「核临时读不到 ≠ 一个节点都不能用」这个口径本身。
    #[test]
    fn only_attributable_rejection_peels_everything_else_fails_open() {
        let zero = Duration::ZERO;
        let b = PEEL_TIME_BUDGET;
        assert_eq!(
            decide_peel(&ConfigCheckVerdict::Accepted, zero, b),
            PeelStep::Proceed
        );
        assert!(matches!(
            decide_peel(&ConfigCheckVerdict::Unavailable("核不存在".into()), zero, b),
            PeelStep::Stop(_)
        ));
        assert!(matches!(
            decide_peel(
                &ConfigCheckVerdict::Unattributable("route.rules[0]".into()),
                zero,
                b
            ),
            PeelStep::Stop(_)
        ));
        let r = KernelRejection {
            array: RejectedArray::Outbounds,
            index: 2,
            detail: "outbounds[2]: unknown outbound type: zzz".into(),
        };
        assert_eq!(
            decide_peel(&ConfigCheckVerdict::Rejected(r.clone()), zero, b),
            PeelStep::Peel(r)
        );
    }

    /// 🔴 **变异锁：预算耗尽必须停下来放行，而不是继续剥到天荒地老**。
    ///
    /// 变异：删掉 `elapsed >= budget` 那一支 ⇒ 本条断（会返回 Peel）。
    /// 同时钉住「预算只拦 Rejected」：Accepted 即使超预算也照样放行（超预算不是失败）。
    #[test]
    fn budget_exhaustion_stops_peeling_but_never_blocks_a_healthy_config() {
        let r = KernelRejection {
            array: RejectedArray::Outbounds,
            index: 0,
            detail: "outbounds[0]: x".into(),
        };
        let budget = Duration::from_millis(100);
        assert!(matches!(
            decide_peel(
                &ConfigCheckVerdict::Rejected(r),
                Duration::from_millis(100),
                budget
            ),
            PeelStep::Stop(_)
        ));
        assert_eq!(
            decide_peel(
                &ConfigCheckVerdict::Accepted,
                Duration::from_secs(99),
                budget
            ),
            PeelStep::Proceed
        );
    }
}

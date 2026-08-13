//! vendored proto ⇄ 随包内核 wire 契约门（开发机侧）。
//!
//! # 与 `mock_server.rs` 的分工
//!
//! `mock_server.rs` 用**同一份 vendored 类型**同时当 client 和 server —— 它只能证明「本仓自己跟自己
//! 一致」，对「本仓跟真核一致吗」结构上给不出任何信息。2026-08-05 的故障正落在它的盲区：上游在
//! `TailscaleEndpointStatus` 的 f3 插了 `stateText`，其后字段号全部 +1，mock 全绿而真机上整条
//! Tailscale STATUS 流静默死掉（详见 `proto/started_service.proto` 内的段落）。本文件补的就是那一格。
//!
//! # 各测试的信息量边界（重要，别把 skip 当绿）
//!
//! | 测试 | 需要真核 | CI（无核）跑吗 | 它能证明什么 |
//! |---|---|---|---|
//! | `machinery_detects_field_number_drift` | 否 | **跑** | 解析器 + 对拍器本身是活的：字段号被改一位必被抓出 |
//! | `machinery_detects_enum_value_drift` | 否 | **跑** | 枚举那条腿同样是活的（含「值号 0 合法」这一格） |
//! | `machinery_detects_nested_message_drift` | 否 | **跑** | 嵌套那条腿（`Log.Message`，descriptor 侧 nested_type + vendored 侧带点路径）是活的 |
//! | `message_and_enum_tables_do_not_cross_contaminate` | 否 | **跑** | message 表与 enum 表各走各的，不串味 |
//! | `vendored_proto_matches_recorded_core_layout` | 否 | **跑** | vendored proto 与实测记录的 beta.7 布局一致 |
//! | `every_checked_symbol_has_a_recorded_layout` | 否 | **跑** | 进对拍表的符号都留了「核对过真核」的书面证据 |
//! | `vendored_proto_matches_every_bundled_core` | 是 | 跳过（**静默**） | vendored proto 与**盘上真核**一致 |
//!
//! 只有最后一条依赖真核，而 `ci.yml` 不拉核（只造 `.keep` 占位目录）。故 CI 上它恒跳过 ——
//! 这不是把门做空：真核那条腿的牙在 `build.rs` 的 release-only 断言上（`package.yml` 构建前四平台
//! 全拉核），其余几条则保证「门自己没坏」和「proto 没被改坏」在每一次 CI 都被验一遍。
//!
//! # 换核之后会发生什么（这是设计意图，不是维护负担）
//!
//! 升级随包核且上游改了字段号 → `vendored_proto_matches_every_bundled_core` 在开发机先红；改完
//! proto 后 `vendored_proto_matches_recorded_core_layout` 也会红，逼你把下面 `RECORDED_*` 那张表
//! 一并更新。**两处一起红是刻意的**：那张表是「我们当时确实去核对过真核」的书面证据，让它跟着改，
//! 等于让每次换核都必须重新做一次核对，而不是顺手把 proto 改绿了事。

include!("../proto_wire_check.rs");

use std::collections::BTreeMap;

const CHECKED_MESSAGE: &str = "TailscaleEndpointStatus";

// 符号表与 vendored proto 原文取自 `proto_wire_check`（三个消费点共用一份，
// 此前 build.rs 与本文件各存一份、靠注释互相提醒「一处漏加，另一处就白守」）。
// 表里每一项仍必须有下面 `RECORDED_LAYOUTS` 里的实测记录作书面证据 ——
// 由 `every_checked_symbol_has_a_recorded_layout` 守着。
use proto_wire_check::{SymbolKind, CHECKED_SYMBOLS, PROTO_SRC};

/// sing-box **1.14.0-beta.7** 的 `TailscaleEndpointStatus` 真实字段号。
///
/// 取证方式（2026-08-05）：从 `resources/linux/sing-box` 与 `resources/mac-arm64/sing-box` 两份二进制里
/// 抠 protoc-gen-go 嵌入的 `FileDescriptorProto`，两份一致。对照组 `1.14.0-alpha.40`（/opt/上游 随包核）
/// 为 `authURL=3 … self=6 userGroups=7 exitNode=8 keyAuth=9` —— 上游正是在 f3 插入 `stateText`
/// 把其后全部顶掉一位，那次漂移就是本目录这道门存在的原因。
const RECORDED_BETA7_LAYOUT: &[(&str, u32)] = &[
    ("endpointTag", 1),
    ("backendState", 2),
    ("stateText", 3),
    ("authURL", 4),
    ("networkName", 5),
    ("magicDNSSuffix", 6),
    ("self", 7),
    ("userGroups", 8),
    ("exitNode", 9),
    ("keyAuth", 10),
];

/// sing-box **1.14.0-beta.7** 的 `DefaultLogLevel`（`GetDefaultLogLevel` 的响应）字段号。
///
/// 取证方式（2026-08-08）：读 `sing-box@v1.14.0-beta.7/daemon/started_service.proto` 上游源码
/// —— 随包核二进制里的 `FileDescriptorProto` 正是由这份源码编译嵌入的，两者同源。
/// 有核的机器上由 `vendored_proto_matches_every_bundled_core` 直接对二进制再验一遍。
const RECORDED_BETA7_DEFAULT_LOG_LEVEL: &[(&str, u32)] = &[("level", 1)];

/// sing-box **1.14.0-beta.7** 的 `LogLevel` 枚举值号。取证方式同上。
///
/// **注意序向**：0 = PANIC 最严重、6 = TRACE 最不严重，与本仓
/// `config-engine::user_config::LogLevel`（debug=0 … fatal=4，严重度**升序**）方向相反且档数不同。
/// 把两者当同一个枚举混用，症状就是级别显示得头尾颠倒 —— 这张表存在的意义即在于此。
const RECORDED_BETA7_LOG_LEVEL: &[(&str, u32)] = &[
    ("PANIC", 0),
    ("FATAL", 1),
    ("ERROR", 2),
    ("WARN", 3),
    ("INFO", 4),
    ("DEBUG", 5),
    ("TRACE", 6),
];

/// sing-box **1.14.0-beta.7** 的 `Log`（`SubscribeLog` 的帧）字段号。取证方式同上
/// （读 `sing-box@v1.14.0-beta.7/daemon/started_service.proto`）。
///
/// `reset` 是**语义开关而非可选装饰**：它一旦与 `messages` 撞号，历史帧就会被当成增量整份重放。
const RECORDED_BETA7_LOG: &[(&str, u32)] = &[("messages", 1), ("reset", 2)];

/// sing-box **1.14.0-beta.7** 的 `Log.Message`（嵌套消息）字段号。取证方式同上。
///
/// 两个字段一个是枚举（varint）一个是字符串（length-delimited）—— 互换即 `UnexpectedWireType`，
/// prost 零容忍整帧丢弃，故这层与外层同等重要，不能因为它是嵌套的就不进表。
const RECORDED_BETA7_LOG_MESSAGE: &[(&str, u32)] = &[("level", 1), ("message", 2)];

/// `CHECKED_SYMBOLS` 每一项对应的实测记录。两张表**必须逐项对上**（由
/// `every_checked_symbol_has_a_recorded_layout` 守）：只加进对拍表却不留实测记录，等于把
/// 「我们确实核对过真核」这条书面证据跳过了。
type RecordedLayout = (SymbolKind, &'static str, &'static [(&'static str, u32)]);
const RECORDED_LAYOUTS: &[RecordedLayout] = &[
    (SymbolKind::Message, CHECKED_MESSAGE, RECORDED_BETA7_LAYOUT),
    (
        SymbolKind::Message,
        "DefaultLogLevel",
        RECORDED_BETA7_DEFAULT_LOG_LEVEL,
    ),
    (SymbolKind::Enum, "LogLevel", RECORDED_BETA7_LOG_LEVEL),
    (SymbolKind::Message, "Log", RECORDED_BETA7_LOG),
    (
        SymbolKind::Message,
        "Log.Message",
        RECORDED_BETA7_LOG_MESSAGE,
    ),
];

fn recorded_layout() -> BTreeMap<String, u32> {
    to_map(RECORDED_BETA7_LAYOUT)
}

fn to_map(rows: &[(&str, u32)]) -> BTreeMap<String, u32> {
    rows.iter().map(|(n, v)| ((*n).to_string(), *v)).collect()
}

// ── 最小 protobuf 编码器（只为造合成 descriptor；解码侧在 proto_wire_check.rs）──────────────

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

fn len_delim(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut o = varint((u64::from(field) << 3) | 2);
    o.extend(varint(payload.len() as u64));
    o.extend_from_slice(payload);
    o
}

fn var_field(field: u32, v: u64) -> Vec<u8> {
    let mut o = varint(u64::from(field) << 3);
    o.extend(varint(v));
    o
}

/// `FieldDescriptorProto`：name=1, number=3。
fn field_desc(name: &str, num: u32) -> Vec<u8> {
    let mut o = len_delim(1, name.as_bytes());
    o.extend(var_field(3, u64::from(num)));
    o
}

/// `DescriptorProto`：name=1, field=2(repeated)。
fn message_desc(name: &str, fields: &BTreeMap<String, u32>) -> Vec<u8> {
    let mut o = len_delim(1, name.as_bytes());
    for (n, num) in fields {
        o.extend(len_delim(2, &field_desc(n, *num)));
    }
    o
}

/// `DescriptorProto` + 一层 `nested_type`（field 3）。`Log.Message` 那种嵌套消息在真 rawDesc 里
/// 就是这么放的，合成样本必须同形，否则测的不是真核那条码路。
fn message_desc_nested(
    name: &str,
    fields: &BTreeMap<String, u32>,
    nested: &[SynthMessage<'_>],
) -> Vec<u8> {
    let mut o = message_desc(name, fields);
    for (n, f) in nested {
        o.extend(len_delim(3, &message_desc(n, f)));
    }
    o
}

/// `EnumValueDescriptorProto`：name=1, number=2。
///
/// 值号**显式写出，包括 0**（`PANIC = 0`）—— protoc 对 proto2 的 optional 字段带显式存在性，
/// rawDesc 里确实会写出 `number: 0`；合成样本必须同形，否则测的就不是真核那条码路。
fn enum_value_desc(name: &str, num: u32) -> Vec<u8> {
    let mut o = len_delim(1, name.as_bytes());
    o.extend(var_field(2, u64::from(num)));
    o
}

/// `EnumDescriptorProto`：name=1, value=2(repeated)。
fn enum_desc(name: &str, values: &BTreeMap<String, u32>) -> Vec<u8> {
    let mut o = len_delim(1, name.as_bytes());
    for (n, num) in values {
        o.extend(len_delim(2, &enum_value_desc(n, *num)));
    }
    o
}

/// 造一个「看起来像 Go 二进制」的字节串：前后垫料 + `FileDescriptorProto`
/// （name=1, message_type=4, enum_type=5）。
/// 尾部 `0x00`（field 0）让贪心解析器干净停下，模拟 rawDesc 之后的无关数据。
fn synthetic_core(messages: &[SynthMessage<'_>], enums: &[SynthMessage<'_>]) -> Vec<u8> {
    synthetic_core_with_nested(messages, enums, &[])
}

/// 一个 message 的合成描述：`(名字, 本层字段表)`。
type SynthMessage<'a> = (&'a str, &'a BTreeMap<String, u32>);
/// 带一层嵌套的 message：`(名字, 本层字段表, 嵌套 message 列表)`。
type SynthNestedOwner<'a> = (&'a str, &'a BTreeMap<String, u32>, &'a [SynthMessage<'a>]);

/// 同上，外加带 `nested_type` 的顶层 message。
fn synthetic_core_with_nested(
    messages: &[SynthMessage<'_>],
    enums: &[SynthMessage<'_>],
    nested_owners: &[SynthNestedOwner<'_>],
) -> Vec<u8> {
    let mut desc = len_delim(1, b"daemon/started_service.proto");
    for (name, fields) in messages {
        desc.extend(len_delim(4, &message_desc(name, fields)));
    }
    for (name, fields, nested) in nested_owners {
        desc.extend(len_delim(4, &message_desc_nested(name, fields, nested)));
    }
    for (name, values) in enums {
        desc.extend(len_delim(5, &enum_desc(name, values)));
    }

    let mut bin = vec![0x5Au8; 4096]; // 前垫料：确保锚点不在 offset 0，逼真一点
    bin.extend_from_slice(&desc);
    bin.push(0x00);
    bin.extend_from_slice(&[0x7Fu8; 4096]); // 后垫料
    bin
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

/// **运行期换核前置检查的判据本体**：`verdict_for_core_bytes` 的三态分档。
///
/// 这是 `build.rs` 那道 release 硬门够不着的那一格 —— 它的取材面只有 `resources/*/sing-box`，
/// 而在线换核 / 用户自带 fork 换上来的核从不经过那四条路径。
///
/// 四条断言各锁一个方向：
/// - 与 vendored proto 完全一致 ⇒ `Match`（不得误拦用户自选的核）；
/// - **只把一个字段号顶掉一位**（= 2026-08-05 那次事故的形状）⇒ `Mismatch`（必须拦）；
/// - 抠不出 descriptor ⇒ `Unobservable` 而**非** `Mismatch` —— 据一次读失败拦下换核，
///   是把「没观测到」当成「观测到有问题」，那会剥夺用户装自己那份核的能力；
/// - 表里的符号在该核里根本不存在 ⇒ 同样 `Unobservable`：该 rpc 会直接 Unimplemented/空表，
///   失败是**响亮**的，与「字段号错位导致整帧静默解不开」不是一档风险。
///
/// **变异探针**：把 `verdict_for_core_bytes` 里两处 `Unobservable` 改成 `Mismatch` ⇒ 后两条转红；
/// 把 `bad.is_empty()` 改成恒 true ⇒ 第二条转红。
#[test]
fn runtime_verdict_blocks_number_drift_but_never_a_blind_spot() {
    use std::collections::BTreeMap;
    let sym = |k, n: &str| {
        proto_wire_check::symbol_from_proto_src(PROTO_SRC, k, n)
            .unwrap_or_else(|e| panic!("vendored proto 里取不到 {n}：{e}"))
    };
    let tse = sym(SymbolKind::Message, "TailscaleEndpointStatus");
    let dll = sym(SymbolKind::Message, "DefaultLogLevel");
    let log = sym(SymbolKind::Message, "Log");
    let log_msg = sym(SymbolKind::Message, "Log.Message");
    let lvl = sym(SymbolKind::Enum, "LogLevel");

    let build = |inner: &BTreeMap<String, u32>| {
        synthetic_core_with_nested(
            &[("TailscaleEndpointStatus", &tse), ("DefaultLogLevel", &dll)],
            &[("LogLevel", &lvl)],
            &[("Log", &log, &[("Message", inner)])],
        )
    };

    assert_eq!(
        proto_wire_check::verdict_for_core_bytes(&build(&log_msg)),
        proto_wire_check::WireVerdict::Match,
        "与 vendored proto 逐字段一致的核不得被拦"
    );

    let mut drifted = log_msg.clone();
    let victim = drifted
        .keys()
        .next()
        .expect("Log.Message 至少一个字段")
        .clone();
    *drifted.get_mut(&victim).expect("刚取的键") += 1;
    assert!(
        matches!(
            proto_wire_check::verdict_for_core_bytes(&build(&drifted)),
            proto_wire_check::WireVerdict::Mismatch(_)
        ),
        "字段号顶掉一位必须判 Mismatch —— 这正是 2026-08-05 那次静默故障的形状"
    );

    assert!(
        matches!(
            proto_wire_check::verdict_for_core_bytes(b"this is not a go binary"),
            proto_wire_check::WireVerdict::Unobservable(_)
        ),
        "抠不出 descriptor ⇒ Unobservable（没观测到 ≠ 观测到没问题）"
    );

    let partial = synthetic_core(&[("TailscaleEndpointStatus", &tse)], &[]);
    assert!(
        matches!(
            proto_wire_check::verdict_for_core_bytes(&partial),
            proto_wire_check::WireVerdict::Unobservable(_)
        ),
        "表里符号缺席 ⇒ Unobservable（失败是响亮的，不是静默错解）"
    );
}

/// **变异验证（门自己有没有牙）**：合成一份「真核」descriptor，再拿一张只错一位的 vendored 表去对拍
/// —— 必须被抓出，且报告里要指名道姓是哪个字段、两边各是多少。
///
/// 变异选的是 `self: 7 → 6`，即 2026-08-05 真实故障的那一位：它当时把 `magicDNSSuffix`(string) 喂给
/// `TailscalePeer`(message) 解，触发 `unexpected end group tag`。用真实事故做变异样本，而不是随便挑
/// 一个字段改，这样门红时的报告长得跟真出事时一模一样。
#[test]
fn machinery_detects_field_number_drift() {
    let real = recorded_layout();
    let core = synthetic_core(&[(CHECKED_MESSAGE, &real)], &[]);

    // 正向对照：解析器能从合成二进制里把这个 message 原样抠回来。
    let parsed = proto_wire_check::descriptor_from_core(&core).expect("合成 descriptor 应可解析");
    let got = parsed
        .messages
        .get(CHECKED_MESSAGE)
        .unwrap_or_else(|| panic!("解析结果里应含 {CHECKED_MESSAGE}"));
    assert_eq!(got, &real, "解析器必须无损还原字段表");

    // 未变异 → 无差异。
    assert!(
        proto_wire_check::diff(&real, got).is_empty(),
        "未变异时不得报差异（否则门会天天误报，很快就被忽略）"
    );

    // 变异 → 必须转红。
    let mut mutated = real.clone();
    mutated.insert("self".into(), 6);
    let bad = proto_wire_check::diff(&mutated, got);
    assert_eq!(bad.len(), 1, "只错一位就应只报一条，实际：{bad:?}");
    assert!(
        bad[0].contains("self") && bad[0].contains('6') && bad[0].contains('7'),
        "报告必须指名字段与两侧取值，实际：{}",
        bad[0]
    );

    // 成片漂移（插一个字段导致其后全 +1）→ 必须全部报出，不能只报第一条。
    let mut shifted = real.clone();
    for (name, num) in real.iter() {
        if *num >= 3 {
            shifted.insert(name.clone(), num + 1);
        }
    }
    let bad = proto_wire_check::diff(&shifted, got);
    assert_eq!(
        bad.len(),
        RECORDED_BETA7_LAYOUT
            .iter()
            .filter(|(_, n)| *n >= 3)
            .count(),
        "成片漂移必须逐条报出（只报第一条会被误读成孤立笔误），实际：{bad:?}"
    );
}

/// **变异验证（enum 那条腿有没有牙）**：枚举值号与字段号走的是 descriptor 里两条不同的路
/// （message_type=4 / enum_type=5，值号在 field 2 而非 3），故不能靠上面那条测试顺带覆盖。
///
/// 变异选的是 `WARN: 3 → 4`（= INFO 的号）：这正是 `GetDefaultLogLevel` 出错时的真实形态 ——
/// 核在 warn 上跑，自证栏却写着 INFO。一处显示错误真值的自证，比没有这处自证更糟。
///
/// 另有一条正向对照落在 `PANIC = 0` 上：**值号 0 合法**，解析器若沿用字段号那套「0 = 没读到就丢掉」
/// 的写法，PANIC 会静默从表里消失，而 `diff` 只查 vendored ⊆ real，缺项在真核侧不会被发现。
#[test]
fn machinery_detects_enum_value_drift() {
    let real = to_map(RECORDED_BETA7_LOG_LEVEL);
    // 带上引用它的那个 message：真 rawDesc 里 enum 从不单独出现，
    // 且 `descriptor_from_core` 对「一个 message 都没解出来」判定为解析策略失效。
    let msg = to_map(RECORDED_BETA7_DEFAULT_LOG_LEVEL);
    let core = synthetic_core(&[("DefaultLogLevel", &msg)], &[("LogLevel", &real)]);

    let parsed = proto_wire_check::descriptor_from_core(&core).expect("合成 descriptor 应可解析");
    let got = parsed
        .enums
        .get("LogLevel")
        .expect("解析结果里应含 enum LogLevel");
    assert_eq!(got, &real, "解析器必须无损还原枚举值表（含 PANIC = 0）");
    assert_eq!(
        got.get("PANIC"),
        Some(&0),
        "值号 0 不得被当成「没读到」丢掉"
    );

    assert!(
        proto_wire_check::diff(&real, got).is_empty(),
        "未变异时不得报差异"
    );

    let mut mutated = real.clone();
    mutated.insert("WARN".into(), 4);
    let bad = proto_wire_check::diff(&mutated, got);
    assert_eq!(bad.len(), 1, "只错一位就应只报一条，实际：{bad:?}");
    assert!(
        bad[0].contains("WARN") && bad[0].contains('4') && bad[0].contains('3'),
        "报告必须指名枚举值与两侧取值，实际：{}",
        bad[0]
    );
}

/// **变异验证（嵌套那条腿有没有牙）**：`Log.Message` 两侧的取证路径都与顶层 message 不同 ——
/// 真核侧走 `DescriptorProto.nested_type`（field 3）并以 `Log.Message` 为全名，vendored 侧走
/// `.proto` 文本里的**带点路径**下钻。两条都是本批新加的码路，顶层那条测试一格都覆盖不到。
///
/// 变异选 `message: 2 → 1`（撞上 `level`）：那是本消息**最坏**的一种错位 —— `level` 是枚举(varint)、
/// `message` 是字符串(length-delimited)，撞号即 `UnexpectedWireType`，prost 整帧丢弃，
/// `ReconnectingStream` 再把它当断线无限重连 ⇒ 日志页一行核日志都没有且零报错。
#[test]
fn machinery_detects_nested_message_drift() {
    let outer = to_map(RECORDED_BETA7_LOG);
    let inner = to_map(RECORDED_BETA7_LOG_MESSAGE);
    let core = synthetic_core_with_nested(&[], &[], &[("Log", &outer, &[("Message", &inner)])]);

    let parsed = proto_wire_check::descriptor_from_core(&core).expect("合成 descriptor 应可解析");
    let got = parsed
        .messages
        .get("Log.Message")
        .expect("嵌套 message 必须以 `Log.Message` 全名进表（点名不到它 = 那格覆盖面是空的）");
    assert_eq!(got, &inner, "解析器必须无损还原嵌套字段表");
    assert!(
        parsed.messages.contains_key("Log"),
        "外层 message 不得因为带了 nested_type 就丢掉"
    );

    // vendored 侧：带点路径必须能从 .proto 文本里下钻到嵌套块，且**只收本层字段**
    // （外层若把 `messages`/`reset` 混进来，或内层把嵌套块的行当字段解析，都在这里现形）。
    let vendored_inner =
        proto_wire_check::symbol_from_proto_src(PROTO_SRC, SymbolKind::Message, "Log.Message")
            .expect("vendored proto 应可解析 Log.Message");
    assert_eq!(vendored_inner, inner);
    let vendored_outer =
        proto_wire_check::symbol_from_proto_src(PROTO_SRC, SymbolKind::Message, "Log")
            .expect("vendored proto 应可解析 Log");
    assert_eq!(
        vendored_outer, outer,
        "外层只收本层字段：嵌套块整段跳过，不得把 level/message 也算进 Log"
    );

    assert!(
        proto_wire_check::diff(&inner, got).is_empty(),
        "未变异时不得报差异"
    );
    let mut mutated = inner.clone();
    mutated.insert("message".into(), 1);
    let bad = proto_wire_check::diff(&mutated, got);
    assert_eq!(bad.len(), 1, "只错一位就应只报一条，实际：{bad:?}");
    assert!(
        bad[0].contains("message") && bad[0].contains('1') && bad[0].contains('2'),
        "报告必须指名字段与两侧取值，实际：{}",
        bad[0]
    );
}

/// `descriptor_from_core` 里 message 与 enum 两条路必须**各走各的**：把 enum 的号拿去当 message 的
/// 号对拍（或反之）必须报「无此项」，不能因为两张表在同一个结构体里就串味。
#[test]
fn message_and_enum_tables_do_not_cross_contaminate() {
    let msg = to_map(RECORDED_BETA7_DEFAULT_LOG_LEVEL);
    let en = to_map(RECORDED_BETA7_LOG_LEVEL);
    let core = synthetic_core(&[("DefaultLogLevel", &msg)], &[("LogLevel", &en)]);
    let parsed = proto_wire_check::descriptor_from_core(&core).expect("合成 descriptor 应可解析");

    assert!(
        !parsed.messages.contains_key("LogLevel"),
        "enum 不得混进 message 表"
    );
    assert!(
        !parsed.enums.contains_key("DefaultLogLevel"),
        "message 不得混进 enum 表"
    );
}

/// vendored `.proto` 文本 ⇄ 实测记录的 beta.7 布局（对拍表里**每一个**符号）。
/// **不需要真核**，故 CI 上也跑。
///
/// 它守的是「proto 被改坏」这一侧：任何人手改字段号 / 枚举值号（含把它改回 alpha 期布局）都会在
/// 这里转红，无需等到有核的机器上。
#[test]
fn vendored_proto_matches_recorded_core_layout() {
    for (kind, name, rows) in RECORDED_LAYOUTS {
        let vendored = proto_wire_check::symbol_from_proto_src(PROTO_SRC, *kind, name)
            .unwrap_or_else(|e| panic!("vendored proto 应可解析 {name}：{e}"));
        let recorded = to_map(rows);

        let bad = proto_wire_check::diff(&vendored, &recorded);
        assert!(
            bad.is_empty(),
            "vendored proto 的 `{name}` 与实测记录的 sing-box 1.14.0-beta.7 布局不符：\n{}\n\
             若这是因为升级了随包核：先用 `vendored_proto_matches_every_bundled_core` 对着新核确认真实\
             号，再同步更新本文件的 RECORDED_* 表（那些表是核对过真核的书面证据）。",
            bad.join("\n")
        );

        // 反向：记录表里的项 vendored 必须全都声明了。少声明不会解崩，但会让门覆盖面悄悄缩水。
        let missing: Vec<&str> = rows
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !vendored.contains_key(*n))
            .collect();
        assert!(
            missing.is_empty(),
            "vendored proto 的 `{name}` 漏声明了这些项（门的覆盖面会随之缩水）：{missing:?}"
        );
    }
}

/// 对拍表与实测记录表**逐项对上**：往 `CHECKED_SYMBOLS` 里加一个符号却不留 `RECORDED_*` 记录，
/// 等于跳过了「我们确实核对过真核」这一步 —— 那正是这道门当初建起来要防的事。
#[test]
fn every_checked_symbol_has_a_recorded_layout() {
    for (kind, name) in CHECKED_SYMBOLS {
        assert!(
            RECORDED_LAYOUTS
                .iter()
                .any(|(k, n, _)| k == kind && n == name),
            "`{name}` 进了 CHECKED_SYMBOLS 却没有 RECORDED_* 实测记录"
        );
    }
    for (kind, name, _) in RECORDED_LAYOUTS {
        assert!(
            CHECKED_SYMBOLS.iter().any(|(k, n)| k == kind && n == name),
            "`{name}` 有实测记录却没进 CHECKED_SYMBOLS（真核那条腿不会去对它）"
        );
    }
}

/// vendored `.proto` ⇄ **盘上真核**（对拍表里每一个符号）。需要 `node scripts/fetch-core.mjs` 拉过核。
///
/// 无核 → 跳过。🔴 **「跳过」是静默的，别把它读成会自曝**：下面那句 `eprintln!` 归 libtest 捕获，
/// 只在测试失败时才回放（2026-08-07 实测更正，同 `config-engine/tests/kernel_accepts_outbounds.rs`）。
/// ⇒ CI ubuntu 腿（不拉核）上这条绿只说明「编得过」，没有比对过任何东西。真核那条腿的牙在 release
/// 构型下的 `build.rs`，见该文件 `assert_proto_matches_bundled_core` 的文档。
#[test]
fn vendored_proto_matches_every_bundled_core() {
    let cores = proto_wire_check::bundled_cores();
    if cores.is_empty() {
        eprintln!(
            "[skip] 随包内核不在盘上（resources/*/sing-box），本条跳过。\n\
             \x20      这是 CI 的常态（ci.yml 不拉核，只造 .keep 占位目录），**不代表契约已验证**。\n\
             \x20      本机要跑它：node scripts/fetch-core.mjs --platform=linux\n\
             \x20      出包腿的硬门在 crates/singbox-grpc/build.rs（release-only）。"
        );
        return;
    }
    for core in &cores {
        for (kind, name) in CHECKED_SYMBOLS {
            proto_wire_check::check_core_against_proto(core, PROTO_SRC, *kind, name)
                .unwrap_or_else(|report| panic!("{report}"));
        }
        eprintln!("[ok] {} 与 vendored proto 一致", core.display());
    }
}

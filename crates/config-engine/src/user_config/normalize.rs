//! 枚举/token 读取归一（落地要求 R3/R4）。
//!
//! **为什么存在**：存量/订阅来的字段值大小写、空白不受控（`"TLS"` / `"Chrome"` / `"tls "`）。
//! 严格比较（`== Some("tls")`）会静默不命中分支 → 最坏形态是 TLS 不启用且无报错
//! （上游 #297「枚举读取归一」/ #298「vless/ss 指纹大小写归一」已真实发生并修复）。
//!
//! 归一只做**边界一次**：serde 反序列化入口（[`de_opt_token`]）+ 生成侧消费点。
//! 取值集闭合的字段（`security`）不用本模块，改由 [`SecurityMode`] 类型化根治 ——
//! 类型系统保证大小写变体不可表示，比"记得调归一函数"可靠。
//!
//! [`SecurityMode`]: crate::user_config::server_config::SecurityMode

use serde::{Deserialize, Deserializer};

/// 归一为小写 token：trim + ASCII 小写；空/纯空白 → `None`。
///
/// 空 → `None` 与 `normalize_duration` 的空值约定一致（未设置 ≠ 空串）。
/// 仅 ASCII 小写：目标取值集（uTLS fingerprint / XTLS flow / vmess security / 传输 network）
/// 全为 ASCII，`to_ascii_lowercase` 无 Unicode 特例风险（如土耳其语 I）且不改变非 ASCII 脏值。
pub fn normalize_token(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.to_ascii_lowercase())
}

/// `Option<String>` 字段的归一反序列化钩子。
///
/// 用法：`#[serde(default, deserialize_with = "de_opt_token")]`。
/// **`default` 不可省** —— 一旦指定 `deserialize_with`，serde 会丢掉 `Option<T>`
/// "缺键即 None"的隐式行为，缺键将报错。
pub fn de_opt_token<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(de)?.and_then(|s| normalize_token(&s)))
}

/// 传输层别名归一：Xray/v2ray 生态词汇 → `ServerConfig.network` 规范值。**单一真值**。
///
/// **为什么是一份而不是三份**：上游 把同一张别名表写了三遍（`ProtocolParser.parseTransportSettings`
/// 的 URI `type=` 分支、`ProtocolParser.parseVmess` 的 base64-JSON `net` 分支、`xray-import.ts`
/// 的 `streamSettings.network` 分支），三处入参形态不同（URLSearchParams / JSON / JSON）但**别名映射
/// 完全同构**。issue #263 的事故正是「一处漏 case，一船节点全灭」——三份表意味着三倍漏的机会。
/// 此处只归一 token→规范值；各入参形态的**字段抽取**（path/host 从哪取）天然不同，留在各调用点，
/// 那不是重复。
///
/// **返回 `None` = 未知传输**，由调用方决定处置。分享链接族一律**整节点拒绝**：xhttp/splithttp/kcp/quic
/// 是 Xray 专属或 sing-box 无能力的传输，入库也连不上，静默降级只会产出「看得见连不上」的假节点
/// （issue #263 根因）。调用方错误消息须保留 `不支持的传输层类型` 字样——用户定案靠日志搜此关键词。
///
/// 别名依据：
/// - `h2` → `http`：builder 的 `generate_transport_config` 口径。
/// - `raw`/`none` → `tcp`：Xray 1.8.24+ 把 `tcp` 更名 `raw`，二者在野共存。
/// - `httpupgrade`：sing-box 原生支持，复用 ws 形态的 path/host 承载（非 ws 别名，是独立传输）。
///
/// 大小写/空白由 [`normalize_token`] 吃掉（`"WS"` → `ws`），与 R4 边界归一同口径。
pub fn normalize_transport(raw: &str) -> Option<&'static str> {
    match normalize_token(raw)?.as_str() {
        "ws" => Some("ws"),
        "httpupgrade" => Some("httpupgrade"),
        "grpc" => Some("grpc"),
        "h2" | "http" => Some("http"),
        "tcp" | "raw" | "none" => Some("tcp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_lowercases_and_trims() {
        assert_eq!(normalize_token("Chrome").as_deref(), Some("chrome"));
        assert_eq!(normalize_token("  FIREFOX  ").as_deref(), Some("firefox"));
        assert_eq!(
            normalize_token("XTLS-RPRX-Vision").as_deref(),
            Some("xtls-rprx-vision")
        );
    }

    #[test]
    fn token_already_canonical_is_identity() {
        assert_eq!(normalize_token("chrome").as_deref(), Some("chrome"));
    }

    #[test]
    fn token_empty_is_none() {
        assert_eq!(normalize_token(""), None);
        assert_eq!(normalize_token("   "), None);
    }

    #[test]
    fn token_non_ascii_preserved() {
        // 非 ASCII 脏值不被 mangle，原样（trim 后）留给下游 FATAL，不静默改写。
        assert_eq!(normalize_token(" 指纹 ").as_deref(), Some("指纹"));
    }

    #[derive(Debug, serde::Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "de_opt_token")]
        fp: Option<String>,
    }

    #[test]
    fn de_hook_normalizes() {
        let h: Holder = serde_json::from_str(r#"{"fp":"Chrome"}"#).unwrap();
        assert_eq!(h.fp.as_deref(), Some("chrome"));
    }

    #[test]
    fn de_hook_missing_key_is_none() {
        // 回归：deserialize_with 会吃掉 Option 的隐式缺键行为，必须靠 `default` 兜住。
        let h: Holder = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(h.fp, None);
    }

    #[test]
    fn de_hook_null_and_empty_are_none() {
        let h: Holder = serde_json::from_str(r#"{"fp":null}"#).unwrap();
        assert_eq!(h.fp, None);
        let h: Holder = serde_json::from_str(r#"{"fp":"  "}"#).unwrap();
        assert_eq!(h.fp, None);
    }

    // ── 传输层别名归一（收敛 上游 三份的单一真值）────────────────────────────
    // 全表锁死：issue #263 的事故形态就是别名表漏 case → 整船节点被 default 分支丢弃。

    #[test]
    fn transport_alias_table_is_exhaustive() {
        // 逐条锁死别名 → 规范值。新增传输须在此登记，否则 default 拒绝。
        for (raw, want) in [
            ("ws", "ws"),
            ("httpupgrade", "httpupgrade"),
            ("grpc", "grpc"),
            ("h2", "http"),
            ("http", "http"),
            ("tcp", "tcp"),
            ("raw", "tcp"),
            ("none", "tcp"),
        ] {
            assert_eq!(
                normalize_transport(raw),
                Some(want),
                "{raw:?} 必须归一为 {want:?}"
            );
        }
    }

    #[test]
    fn transport_alias_is_case_and_space_insensitive() {
        // "WS" 未归一时会走到 generate_transport_config 的 `_ => None` 静默丢传输层。
        for raw in ["WS", "Ws", " ws ", "\tWS\n"] {
            assert_eq!(normalize_transport(raw), Some("ws"), "{raw:?}");
        }
        assert_eq!(normalize_transport("HttpUpgrade"), Some("httpupgrade"));
        assert_eq!(normalize_transport("RAW"), Some("tcp"));
        assert_eq!(normalize_transport("H2"), Some("http"));
    }

    #[test]
    fn transport_unknown_is_none_not_silently_tcp() {
        // 关键不变式：未知传输**不得**静默落 tcp（那正是 上游 xray-import 的做法 → 假节点）。
        // 返回 None 让调用方整节点拒绝。
        for raw in ["xhttp", "splithttp", "kcp", "quic", "mkcp", "bogus"] {
            assert_eq!(normalize_transport(raw), None, "{raw:?} 必须判未知");
        }
    }

    #[test]
    fn transport_empty_is_none() {
        // 空/纯空白 → None（未设置，调用方按缺省 tcp 处理，不进拒绝分支）。
        assert_eq!(normalize_transport(""), None);
        assert_eq!(normalize_transport("   "), None);
    }
}

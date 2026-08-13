//! ipip.net `myip.ipip.net/json` 解析 —— direct 腿**专用**本地直连出口探测。
//!
//! 1:1 移植自 上游 `IpInfoService.parseJson`（ipip 分支）+ `ccFromIpipLocation`。
//! direct 腿**只用国内** ipip：旁路由/软路由透明分流会把国外端点（cloudflare/ip-api/ipify）劫持走代理
//! 出口 → 本地直连出口被误标为境外节点 IP。ipip 走真实大陆出口，是这类环境下唯一测得对本地直连出口的
//! 办法。与 [`crate::trace`]（cloudflare cdn-cgi/trace，仅 proxy 腿）互斥。

use serde_json::Value;

/// 解析后的 ipip 出口信息（对齐渲染端 `IpInfo`：ip 必有，country/countryCode 可缺）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpipInfo {
    pub ip: String,
    /// 地区展示串（location 各非空段以空格连接）；无 location / 全空 → `None`。
    pub country: Option<String>,
    /// ISO alpha-2 国别码（ipip 库无 ISO 码，仅中国系派生）；见 [`cc_from_ipip_location`]。
    pub country_code: Option<String>,
}

/// ipip location → countryCode：中国 → cn（港澳台细分 hk/mo/tw），其余 `None`（渲染端 Globe 兜底）。
///
/// 1:1 移植 上游 `ccFromIpipLocation`：只认 `loc[0] == "中国"`，据 `loc[1]` 细分港澳台。
pub fn cc_from_ipip_location(loc: &[String]) -> Option<String> {
    if loc.first().map(String::as_str) != Some("中国") {
        return None;
    }
    match loc.get(1).map(String::as_str) {
        Some("香港") => Some("hk".to_string()),
        Some("澳门") => Some("mo".to_string()),
        Some("台湾") => Some("tw".to_string()),
        _ => Some("cn".to_string()),
    }
}

/// 解析 `myip.ipip.net/json` body。对应 上游 `parseJson` 的 ipip 分支。
///
/// 期望形态 `{ret:"ok", data:{ip, location:[国,省,市,区,ISP]}}`；缺 `ret=="ok"` / `data` / `data.ip`
/// 或非法 JSON → `None`（劫持页/HTML/截断响应解析失败即弃，防假数据污染直连出口）。
///
/// `country` = location **非空段**以空格连接（对齐 上游 `parts.join(' ')`）；`countryCode` 用**含空段**的
/// 原始 location 派生（对齐 上游 `ccFromIpipLocation(raw)`，其判据只看 `loc[0]`/`loc[1]` 位置）。
pub fn parse_ipip(body: &str) -> Option<IpipInfo> {
    let j: Value = serde_json::from_str(body).ok()?;
    if j.get("ret").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    let data = j.get("data")?;
    let ip = data.get("ip").and_then(Value::as_str)?.to_string();
    // raw：location 里全部字符串段（含空串，对齐 上游 `raw`）；派生 countryCode 用它（判位置）。
    let raw: Vec<String> = data
        .get("location")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // parts：非空段（对齐 上游 `parts`）；拼展示串。
    let parts: Vec<&str> = raw
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    let country = if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    };
    let country_code = cc_from_ipip_location(&raw);
    Some(IpipInfo {
        ip,
        country,
        country_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_china_ipip() {
        // location 含一个空段（区县缺）：country 拼接跳过空段，countryCode 仍据 loc[0]/loc[1] 得 cn。
        let body =
            r#"{"ret":"ok","data":{"ip":"1.2.3.4","location":["中国","北京","北京","","电信"]}}"#;
        let info = parse_ipip(body).unwrap();
        assert_eq!(info.ip, "1.2.3.4");
        assert_eq!(info.country.as_deref(), Some("中国 北京 北京 电信"));
        assert_eq!(info.country_code.as_deref(), Some("cn"));
    }

    #[test]
    fn parse_hongkong_maps_hk() {
        let body = r#"{"ret":"ok","data":{"ip":"1.2.3.4","location":["中国","香港","","",""]}}"#;
        assert_eq!(
            parse_ipip(body).unwrap().country_code.as_deref(),
            Some("hk")
        );
    }

    #[test]
    fn parse_foreign_no_country_code() {
        // 境外直连出口：ipip 库无 ISO 码 → countryCode None，但 country 展示串仍在（渲染端 Globe 兜底）。
        let body =
            r#"{"ret":"ok","data":{"ip":"8.8.8.8","location":["美国","加利福尼亚","","",""]}}"#;
        let info = parse_ipip(body).unwrap();
        assert_eq!(info.country_code, None);
        assert_eq!(info.country.as_deref(), Some("美国 加利福尼亚"));
    }

    #[test]
    fn parse_no_location_country_none() {
        // data.ip 有、location 缺 → ip 保留，country/countryCode 均 None（对齐 上游 parts.length? : undefined）。
        let body = r#"{"ret":"ok","data":{"ip":"1.2.3.4"}}"#;
        let info = parse_ipip(body).unwrap();
        assert_eq!(info.ip, "1.2.3.4");
        assert_eq!(info.country, None);
        assert_eq!(info.country_code, None);
    }

    #[test]
    fn parse_missing_ip_returns_none() {
        // data 无 ip（缺字段）→ None（对齐 上游：d.ip 非 string 即弃）。
        let body = r#"{"ret":"ok","data":{"location":["中国"]}}"#;
        assert!(parse_ipip(body).is_none());
    }

    #[test]
    fn parse_missing_data_returns_none() {
        let body = r#"{"ret":"ok"}"#;
        assert!(parse_ipip(body).is_none());
    }

    #[test]
    fn parse_ret_not_ok_returns_none() {
        let body = r#"{"ret":"err","data":{"ip":"1.2.3.4"}}"#;
        assert!(parse_ipip(body).is_none());
    }

    #[test]
    fn parse_invalid_or_empty_body_returns_none() {
        // 劫持页/portal HTML / 截断响应 → 非法 JSON → None（direct 出口不被假数据污染）。
        assert!(parse_ipip("<html>portal</html>").is_none());
        assert!(parse_ipip("").is_none());
    }

    #[test]
    fn cc_from_location_macau_and_taiwan() {
        assert_eq!(
            cc_from_ipip_location(&["中国".to_string(), "澳门".to_string()]).as_deref(),
            Some("mo")
        );
        assert_eq!(
            cc_from_ipip_location(&["中国".to_string(), "台湾".to_string()]).as_deref(),
            Some("tw")
        );
    }

    #[test]
    fn cc_from_location_empty_or_foreign_is_none() {
        assert_eq!(cc_from_ipip_location(&[]), None);
        assert_eq!(cc_from_ipip_location(&["美国".to_string()]), None);
    }
}

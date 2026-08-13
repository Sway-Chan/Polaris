//! 代理模式（上游 `shared/types.ts:115-116`）。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 分流模式：global=真全局 / smart=智能分流（含自定义规则）/ direct=全直连。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    Global,
    #[default]
    Smart,
    Direct,
}

impl ProxyMode {
    /// 小写字符串表示（对齐 Polaris proxyMode JSON 值）。configGenerationNorm 等用。
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyMode::Global => "global",
            ProxyMode::Smart => "smart",
            ProxyMode::Direct => "direct",
        }
    }
}

/// 代理类型：systemProxy=系统代理 / tun=TUN 接管 / manual=手动（不接管）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProxyModeType {
    #[default]
    SystemProxy,
    Tun,
    Manual,
}

impl ProxyModeType {
    /// 是否 TUN 模式（buildLogConfig output 写文件谓词）。
    pub fn is_tun(self) -> bool {
        matches!(self, ProxyModeType::Tun)
    }
}

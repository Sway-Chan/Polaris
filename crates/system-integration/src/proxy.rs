//! 系统代理状态 + ProxyMarker（marker 崩溃恢复，维度7 #8 纯逻辑）+ stripSelf / restorePlan。
//!
//! 1:1 移植自 上游 `SystemProxyManager.ts` 的基类 `SystemProxyBase`（marker IO + 防自指 + 恢复计划）。
//! FS 抽象为 [`MarkerFs`] trait —— marker 写/读/清是纯逻辑，测试用内存 mock，生产用真实文件系统。
//!
//! 维度7 #8（marker 崩溃恢复）覆盖在 [`proxy_ops::SystemProxyController::recover_from_marker`]：
//! 崩溃后 marker 残留 → 重启读 marker → 清除残留代理（防死端口断网）。本模块提供 marker 读写真值。

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// 系统代理状态。上游 `SystemProxyStatus`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemProxyStatus {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub http_proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub https_proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub socks_proxy: Option<String>,
    /// macOS：该网络服务原有的「绕过代理的主机与域名」清单（`-getproxybypassdomains`）。
    ///
    /// `None` = **没捕获过**（非 mac 平台 / 旧 marker / 读失败），此时 restore **不碰** bypass；
    /// `Some(vec![])` = 捕获到「一条都没有」，restore 需写 `Empty` 哨兵清空。两者必须可分辨 ——
    /// 混同会把「没读到」当成「用户本来就是空的」，反而把人家的清单清掉。
    ///
    /// 为什么需要它：enable 会对**每个**网络服务下发 `-setproxybypassdomains`，而该子命令是
    /// **整表覆盖**（`networksetup(8)` 措辞 "Set ... **to** <domain1> [domain2]..."，另给 `Empty`
    /// 哨兵专用于清空 ⇒ 若是追加，需要的是 remove 动词）。不捕获就没法还原，用户自定义的
    /// 内网域名会被 Polaris 的默认清单永久替换掉。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bypass_domains: Option<Vec<String>>,
}

impl SystemProxyStatus {
    /// 任一代理协议非空即视为有实际代理服务器配置（用于恢复决策 / 状态判空）。
    pub fn has_any_proxy(&self) -> bool {
        self.http_proxy.is_some() || self.https_proxy.is_some() || self.socks_proxy.is_some()
    }
}

/// marker 文件落地结构。上游 `writeMarker` 写入的 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyMarkerData {
    /// 我们的代理 `address:port`（恢复/检测自指用）。
    pub our_host_port: String,
    /// 写入时间戳（ms，上游 `Date.now()`）。用于诊断，不参与判定。
    #[serde(default)]
    pub at: u64,
    /// enable 前的原始代理快照（关机跨会话恢复用；Linux 写入，Win/macOS 可选）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub original_settings: Option<SystemProxyStatus>,
}

/// FS 抽象：marker 文件读写。生产用真实文件系统，测试用内存 mock。
/// 接口语义对齐 Polaris：read 在文件不存在 / 内容损坏时返回 None（不抛）；write/rm 失败仅告警。
pub trait MarkerFs {
    /// 写 marker 文件（覆盖）。失败返回 Err（调用方降级为告警，绝不抛出影响代理结果）。
    fn write_marker(&self, path: &str, data: &str) -> std::io::Result<()>;
    /// 读 marker 文件全文。不存在 → Ok(None)；读失败 → Ok(None)（Polaris catch → null）。
    fn read_marker(&self, path: &str) -> Option<String>;
    /// 删 marker 文件（force：不存在不报错）。失败返回 Err。
    fn remove_marker(&self, path: &str) -> std::io::Result<()>;
}

/// [`MarkerFs`] 的生产实现：真实文件系统（同步 API，文件极小）。
///
/// 语义逐条对齐上游 `SystemProxyBase` 的 marker IO：
/// - `write`：同步覆盖写（上游 `fs.writeFileSync`）。父目录不存在 → 先建（userData 目录首次运行可能未建）。
/// - `read`：不存在 / 读失败 → `None`（上游 catch → null；ENOENT 与 JSON 损坏一视同仁）。
/// - `remove`：**不存在不报错**（上游 `fs.rmSync(path, { force: true })`）—— 这是
///   [`crate::proxy_ops::SystemProxyController::ensure_cleared`] 幂等性的地基：重复清理必须静默成功。
///
/// **同步 API 是有意的**：marker 清理要能用在退出/崩溃兜底这类同步路径上（上游注释明写
/// 「可安全用于 process 'exit' 等同步退出路径」）。
#[derive(Debug, Clone, Copy, Default)]
pub struct StdMarkerFs;

impl MarkerFs for StdMarkerFs {
    fn write_marker(&self, path: &str, data: &str) -> std::io::Result<()> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            // userData 目录首次运行可能不存在；已存在则 no-op。
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, data)
    }

    fn read_marker(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn remove_marker(&self, path: &str) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            // force 语义：不存在视为已清除（幂等）。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

/// ProxyMarker：marker 写/读/清 + 防自指判定。纯逻辑（FS + marker 路径注入）。
/// 上游 `SystemProxyBase` 的 marker 部分抽离。
pub struct ProxyMarker<Fs: MarkerFs> {
    fs: Fs,
    path: String,
}

impl<Fs: MarkerFs> ProxyMarker<Fs> {
    pub fn new(fs: Fs, path: impl Into<String>) -> Self {
        Self {
            fs,
            path: path.into(),
        }
    }

    /// marker 文件路径。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 写 marker（`enableProxy` 成功后 / 前置 intent 调用）。
    /// 失败仅告警（返回 Err），绝不影响代理设置结果（Polaris writeMarker `try/catch` → warn）。
    pub fn write(&self, our_host_port: &str, original: Option<&SystemProxyStatus>) {
        let data = ProxyMarkerData {
            our_host_port: our_host_port.to_string(),
            at: now_ms(),
            original_settings: original.cloned(),
        };
        // 序列化失败理论不会发生（纯数据结构），但仍降级为不抛。
        let Ok(json) = serde_json::to_string(&data) else {
            return;
        };
        let _ = self.fs.write_marker(&self.path, &json);
    }

    /// 删 marker（`disableProxy` 成功后 / 启动恢复清理失效 marker 调用）。
    pub fn clear(&self) {
        let _ = self.fs.remove_marker(&self.path);
    }

    /// 读 marker；文件不存在 / 损坏 / 结构非法 → None（Polaris readMarker `catch → null`）。
    pub fn read(&self) -> Option<ProxyMarkerData> {
        let raw = self.fs.read_marker(&self.path)?;
        let data: ProxyMarkerData = serde_json::from_str(&raw).ok()?;
        if data.our_host_port.is_empty() {
            return None;
        }
        Some(data)
    }

    /// 读 marker 里的 our_host_port（stripSelf / 自指检测用）。无 marker → None。
    pub fn read_our_host_port(&self) -> Option<String> {
        self.read().map(|d| d.our_host_port)
    }

    /// 是否存在有效 marker。
    pub fn exists(&self) -> bool {
        self.read().is_some()
    }
}

/// 防自指：若 status 已指向我们自己的代理（`address:httpPort` 或 marker 记录的 our_host_port），
/// 返回 None（视为无原始）—— 杜绝把自身代理当原始保存、disable 后恢复死端口致断网。
/// 上游 `SystemProxyBase.stripSelf`。
pub fn strip_self(
    status: Option<&SystemProxyStatus>,
    address: &str,
    http_port: u16,
    marker_our_host_port: Option<&str>,
) -> Option<SystemProxyStatus> {
    let status = status?;
    if !status.enabled {
        return Some(status.clone());
    }
    let ours = format!("{address}:{http_port}");
    let points_to_us = |p: &Option<String>| -> bool {
        match p {
            Some(proxy) => proxy == &ours || matches!(marker_our_host_port, Some(m) if proxy == m),
            None => false,
        }
    };
    if points_to_us(&status.http_proxy)
        || points_to_us(&status.https_proxy)
        || points_to_us(&status.socks_proxy)
    {
        return None;
    }
    Some(status.clone())
}

/// Linux gsettings 三 schema 恢复计划条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlanEntry {
    pub schema: &'static str, // "http" | "https" | "socks"
    pub hp: Option<HostPort>,
}

/// 解析出的 `host:port`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

/// 从 `host:port` 健壮拆分（用最后一个冒号作端口分隔符，兼容裸 IPv6 如 `::1:8080`）。
/// 缺端口 / 端口非数字 / 越界 → None。上游 `LinuxSystemProxy.splitHostPort`。
pub fn split_host_port(proxy: Option<&str>) -> Option<HostPort> {
    let proxy = proxy?;
    let idx = proxy.rfind(':')?;
    if idx == 0 {
        return None; // 以 ':' 开头（无 host）
    }
    let host = &proxy[..idx];
    let port_str = &proxy[idx + 1..];
    if host.is_empty() {
        return None;
    }
    let port: u32 = port_str.parse().ok()?;
    if port == 0 || port > 65535 {
        return None;
    }
    Some(HostPort {
        host: host.to_string(),
        port: port as u16,
    })
}

/// Linux gsettings 三 schema 的恢复计划（capture-three）：hp 非空 = 回写该快照值；
/// None = 该 schema 原本未设，须清空（撤销 enable 期对它的写入）。
/// 上游 `LinuxSystemProxy.restorePlan`。
pub fn restore_plan(snap: Option<&SystemProxyStatus>) -> [RestorePlanEntry; 3] {
    let s = snap;
    [
        RestorePlanEntry {
            schema: "http",
            hp: split_host_port(s.and_then(|x| x.http_proxy.as_deref())),
        },
        RestorePlanEntry {
            schema: "https",
            hp: split_host_port(s.and_then(|x| x.https_proxy.as_deref())),
        },
        RestorePlanEntry {
            schema: "socks",
            hp: split_host_port(s.and_then(|x| x.socks_proxy.as_deref())),
        },
    ]
}

/// ms 时间戳（上游 `Date.now()`）。测试可注入，生产用系统时间。
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 测试辅助：跨模块共享的内存 FS mock（marker 崩溃恢复测试用）。
#[cfg(test)]
pub mod proxy_tests_helpers {
    use super::MarkerFs;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 内存 FS mock（单文件，模拟 marker 文件读写）。内部状态共享，可 Clone 跨「进程」会话。
    #[derive(Clone)]
    pub struct MemFs {
        inner: Rc<MemFsInner>,
    }
    struct MemFsInner {
        file: RefCell<Option<String>>,
        read_calls: RefCell<u32>,
    }
    impl MemFs {
        pub fn new() -> Self {
            Self {
                inner: Rc::new(MemFsInner {
                    file: RefCell::new(None),
                    read_calls: RefCell::new(0),
                }),
            }
        }
        pub fn read_calls(&self) -> u32 {
            *self.inner.read_calls.borrow()
        }
    }
    impl Default for MemFs {
        fn default() -> Self {
            Self::new()
        }
    }
    impl MarkerFs for MemFs {
        fn write_marker(&self, _path: &str, data: &str) -> std::io::Result<()> {
            *self.inner.file.borrow_mut() = Some(data.to_string());
            Ok(())
        }
        fn read_marker(&self, _path: &str) -> Option<String> {
            *self.inner.read_calls.borrow_mut() += 1;
            self.inner.file.borrow().clone()
        }
        fn remove_marker(&self, _path: &str) -> std::io::Result<()> {
            *self.inner.file.borrow_mut() = None;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::proxy_tests_helpers::MemFs;

    #[test]
    fn marker_write_read_roundtrip() {
        let fs = MemFs::new();
        let marker = ProxyMarker::new(fs, "/marker.json");
        assert!(!marker.exists());

        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.lan:3128".into()),
            ..Default::default()
        };
        marker.write("127.0.0.1:8080", Some(&original));
        assert!(marker.exists());

        let data = marker.read().expect("marker present");
        assert_eq!(data.our_host_port, "127.0.0.1:8080");
        let orig = data.original_settings.expect("original saved");
        assert_eq!(orig.http_proxy.as_deref(), Some("proxy.lan:3128"));
    }

    #[test]
    fn marker_write_without_original() {
        let fs = MemFs::new();
        let marker = ProxyMarker::new(fs, "/m");
        marker.write("127.0.0.1:8080", None);
        let data = marker.read().unwrap();
        assert_eq!(data.our_host_port, "127.0.0.1:8080");
        assert!(data.original_settings.is_none());
    }

    #[test]
    fn marker_clear_removes() {
        let fs = MemFs::new();
        let marker = ProxyMarker::new(fs, "/m");
        marker.write("127.0.0.1:8080", None);
        assert!(marker.exists());
        marker.clear();
        assert!(!marker.exists());
        assert!(marker.read().is_none());
    }

    #[test]
    fn marker_read_returns_none_for_corrupt_json() {
        struct CorruptFs;
        impl MarkerFs for CorruptFs {
            fn write_marker(&self, _p: &str, _d: &str) -> std::io::Result<()> {
                Ok(())
            }
            fn read_marker(&self, _p: &str) -> Option<String> {
                Some("{not json".into())
            }
            fn remove_marker(&self, _p: &str) -> std::io::Result<()> {
                Ok(())
            }
        }
        let marker = ProxyMarker::new(CorruptFs, "/m");
        assert!(marker.read().is_none());
        assert!(!marker.exists());
    }

    #[test]
    fn marker_read_returns_none_for_empty_our_host_port() {
        struct EmptyFs;
        impl MarkerFs for EmptyFs {
            fn write_marker(&self, _p: &str, _d: &str) -> std::io::Result<()> {
                Ok(())
            }
            fn read_marker(&self, _p: &str) -> Option<String> {
                Some(r#"{"our_host_port":"","at":0}"#.into())
            }
            fn remove_marker(&self, _p: &str) -> std::io::Result<()> {
                Ok(())
            }
        }
        let marker = ProxyMarker::new(EmptyFs, "/m");
        assert!(marker.read().is_none());
    }

    // ── strip_self 防自指（维度7 死端口断网防护）──

    #[test]
    fn strip_self_returns_none_when_points_to_our_address() {
        let status = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("127.0.0.1:8080".into()),
            ..Default::default()
        };
        // 当前代理正是我们自己的 → 视为无原始（否则 disable 会恢复死端口）。
        let r = strip_self(Some(&status), "127.0.0.1", 8080, None);
        assert!(r.is_none());
    }

    #[test]
    fn strip_self_returns_none_when_points_to_marker_host() {
        let status = SystemProxyStatus {
            enabled: true,
            https_proxy: Some("127.0.0.1:8080".into()),
            ..Default::default()
        };
        // 指向 marker 记录的 our_host_port → 自指。
        let r = strip_self(Some(&status), "0.0.0.0", 9999, Some("127.0.0.1:8080"));
        assert!(r.is_none());
    }

    #[test]
    fn strip_self_preserves_real_external_proxy() {
        let status = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            ..Default::default()
        };
        // 真正的第三方代理 → 保留为原始。
        let r = strip_self(Some(&status), "127.0.0.1", 8080, None);
        assert_eq!(r.unwrap().http_proxy.as_deref(), Some("proxy.corp:3128"));
    }

    #[test]
    fn strip_self_preserves_disabled_status() {
        let status = SystemProxyStatus {
            enabled: false,
            ..Default::default()
        };
        // enabled=false → 不判自指，原样返回。
        let r = strip_self(Some(&status), "127.0.0.1", 8080, None);
        assert!(r.is_some());
    }

    #[test]
    fn strip_self_none_when_status_none() {
        assert!(strip_self(None, "127.0.0.1", 8080, None).is_none());
    }

    // ── restore_plan / split_host_port（Linux gsettings 恢复）──

    #[test]
    fn split_host_port_plain() {
        let hp = split_host_port(Some("proxy.lan:3128")).unwrap();
        assert_eq!(hp.host, "proxy.lan");
        assert_eq!(hp.port, 3128);
    }

    #[test]
    fn split_host_port_bare_ipv6() {
        // 裸 IPv6 ::1:8080 → host=::1, port=8080（lastIndexOf ':'）
        let hp = split_host_port(Some("::1:8080")).unwrap();
        assert_eq!(hp.host, "::1");
        assert_eq!(hp.port, 8080);
    }

    #[test]
    fn split_host_port_none_when_no_port() {
        assert!(split_host_port(Some("proxy")).is_none());
        assert!(split_host_port(None).is_none());
        assert!(split_host_port(Some(":8080")).is_none()); // 无 host
    }

    #[test]
    fn split_host_port_none_when_port_out_of_range() {
        assert!(split_host_port(Some("h:0")).is_none());
        assert!(split_host_port(Some("h:65536")).is_none());
        assert!(split_host_port(Some("h:abc")).is_none());
    }

    #[test]
    fn restore_plan_capture_three() {
        let snap = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("h:80".into()),
            https_proxy: Some("h2:443".into()),
            socks_proxy: None, // 原本未设
            bypass_domains: None,
        };
        let plan = restore_plan(Some(&snap));
        assert_eq!(plan[0].schema, "http");
        assert_eq!(plan[0].hp.as_ref().unwrap().port, 80);
        assert_eq!(plan[1].hp.as_ref().unwrap().host, "h2");
        assert!(plan[2].hp.is_none()); // socks 原本未设 → None（清空）
    }

    #[test]
    fn restore_plan_all_none_when_no_snap() {
        let plan = restore_plan(None);
        assert!(plan.iter().all(|e| e.hp.is_none()));
    }

    // ── 维度7 #8：marker 崩溃恢复场景编排（read_calls 验证读路径触发）──

    #[test]
    fn crash_recovery_marker_survives_and_is_readable() {
        // 模拟：enable 写 marker → 进程崩溃（marker 残留）→ 重启读 marker 判定有残留代理 → 清除。
        let fs = MemFs::new();
        let fs_clone = fs.clone();
        let marker = ProxyMarker::new(fs, "/m");

        // 会话1：enable 写 marker（intent），尚未 disable 即崩溃。
        marker.write("127.0.0.1:8080", None);
        assert!(marker.exists());

        // 重启（新会话）：marker 文件仍在磁盘 → 读到 → 判定需恢复。
        let recovered = marker.read().expect("marker survived crash");
        assert_eq!(recovered.our_host_port, "127.0.0.1:8080");
        // 确认确实读了 FS（崩溃恢复路径真触发了读取）。
        assert!(fs_clone.read_calls() >= 1);

        // 恢复成功后清 marker，下次启动不再误恢复。
        marker.clear();
        assert!(!marker.exists());
    }

    // ── 生产 MarkerFs（真实 FS；tempfile 隔离，不碰用户数据目录）──

    #[test]
    fn std_marker_fs_roundtrip_write_read_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system-proxy.marker.json");
        let p = path.to_str().unwrap();
        let fs = StdMarkerFs;

        assert_eq!(fs.read_marker(p), None, "未写入 → None");
        fs.write_marker(p, r#"{"ourHostPort":"127.0.0.1:8080"}"#)
            .unwrap();
        assert_eq!(
            fs.read_marker(p).as_deref(),
            Some(r#"{"ourHostPort":"127.0.0.1:8080"}"#)
        );
        fs.remove_marker(p).unwrap();
        assert_eq!(fs.read_marker(p), None, "删后 → None");
    }

    /// `force` 语义：删不存在的 marker 必须 Ok —— 这是 `ensure_cleared` 幂等性的地基。
    /// 若此处返 Err，重复清理会把错误一路冒到终态点。
    #[test]
    fn std_marker_fs_remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("never-existed.json");
        let p = p.to_str().unwrap();
        StdMarkerFs
            .remove_marker(p)
            .expect("不存在不得报错（force 语义）");
        StdMarkerFs.remove_marker(p).expect("重复删仍 Ok");
    }

    #[test]
    fn std_marker_fs_creates_missing_parent_dir() {
        // userData 目录首次运行可能不存在 → 写 marker 不得因此失败。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/sub/dir/marker.json");
        let p = path.to_str().unwrap();
        StdMarkerFs.write_marker(p, "{}").expect("须自动建父目录");
        assert_eq!(StdMarkerFs.read_marker(p).as_deref(), Some("{}"));
    }

    #[test]
    fn std_marker_fs_corrupt_content_yields_none_marker() {
        // 损坏内容 → read_marker 返回原文，但 ProxyMarker::read 解析失败 → None（不崩）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        std::fs::write(&path, "{not json").unwrap();
        let marker = ProxyMarker::new(StdMarkerFs, path.to_str().unwrap());
        assert!(marker.read().is_none(), "损坏 marker → None，不得 panic");
        assert!(!marker.exists());
    }

    /// 端到端：生产 FS 上的 marker 跨「进程」存活（崩溃恢复的真实载体）。
    #[test]
    fn std_marker_fs_survives_across_marker_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system-proxy.marker.json");
        let p = path.to_str().unwrap();

        // 会话1：写 marker 后「崩溃」。
        let original = SystemProxyStatus {
            enabled: true,
            http_proxy: Some("proxy.corp:3128".into()),
            ..Default::default()
        };
        ProxyMarker::new(StdMarkerFs, p).write("127.0.0.1:8080", Some(&original));

        // 会话2（「重启」）：读回同一磁盘文件，含原始快照。
        let m2 = ProxyMarker::new(StdMarkerFs, p);
        let data = m2.read().expect("marker 须跨会话存活");
        assert_eq!(data.our_host_port, "127.0.0.1:8080");
        assert_eq!(
            data.original_settings.unwrap().http_proxy,
            Some("proxy.corp:3128".to_string())
        );
        m2.clear();
        assert!(m2.read().is_none());
    }
}

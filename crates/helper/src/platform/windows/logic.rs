//! 协议层纯逻辑（跨平台，移植自 `helper-win/helper.go` + `helper-win/winproc.go` 的纯函数部分）。
//!
//! 这里集中所有**无副作用、无 syscall** 的判定与解析逻辑 —— 它们在 Linux 上可单测，生产侧（Windows FFI）
//! 与协议分派（[`crate::platform::windows::helper`]）共用。与 [`polaris_helper_proto::codec`] 的安全约束（单一真值）互补：
//! - iface / cidr / sha256 白名单 → 复用 `helper-proto`（不重复定义，消灭 drift）。
//! - winproc 特有的端口解析、镜像匹配、basename、路径归一化 → 本模块（Go `winproc.go` 对应）。

use polaris_helper_proto::codec;

/// Windows 接口白名单（移植自 `helper.go:50-60` 的 `ifaceAllowed`）。
///
/// 仅允许 `polaris-` 前缀 + 小写字母/数字/连字符，长度 ≤ 24，杜绝任意接口名注入 route/netsh 命令。
///
/// 本函数是 [`codec::is_win_iface_allowed`] 的薄封装（单一真值在 helper-proto），保留本入口便于
/// helper-win 内部按 Go 源命名就近调用 + 未来加 Windows 特定加固点。
#[must_use]
pub fn iface_allowed(s: &str) -> bool {
    codec::is_win_iface_allowed(s)
}

/// cfg 路径白名单（移植自 `helper.go:86-93` 的 `cfgAllowed`）。
///
/// cfg 必须位于 `conf_dir` 内（清洗后前缀匹配，Windows 路径大小写不敏感），防止越权指定任意路径作 SYSTEM 配置。
///
/// `conf_dir` 为空 → 允许一切（对应 Go `confDir == ""` 的安装期未配置兜底，测试/dev 用）。
///
/// # Windows 路径大小写不敏感
///
/// Go 源用 `strings.HasPrefix(strings.ToLower(clean), strings.ToLower(base))` —— 本实现等价：
/// 清洗（`normalize_path`）+ 小写前缀比对。
#[must_use]
pub fn cfg_allowed(cfg: &str, conf_dir: &str) -> bool {
    if conf_dir.is_empty() {
        return true;
    }
    let clean = normalize_path(cfg);
    // base = cleaned conf_dir + 分隔符（Go: filepath.Clean(confDir) + string(os.PathSeparator)）
    let mut base = normalize_path(conf_dir);
    if !base.ends_with('\\') && !base.ends_with('/') {
        base.push('\\');
    }
    let clean_l = clean.to_ascii_lowercase();
    let base_l = base.to_ascii_lowercase();
    clean_l.starts_with(&base_l)
}

/// 解析 freeport 的端口行（移植自 `helper.go:298-307` 的 port 校验）。
///
/// Go 源：`port == "" || strings.IndexFunc(port, 非数字) >= 0` → bad-port；`pnum <= 0 || pnum > 65535` → bad-port。
/// 本函数返回 `Some(u16)` 仅当 port 是纯 ASCII 数字且 ∈ (0, 65535]，否则 `None`（对应 `ERR bad-port`）。
#[must_use]
pub fn parse_port(port: &str) -> Option<u16> {
    let trimmed = port.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Go 源：strings.IndexFunc(port, c < '0' || c > '9') >= 0 → 非「全数字」即拒。
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Go 源 strconv.Atoi + pnum <= 0 || pnum > 65535。u16 解析天然拒绝 >65535。
    let n: u32 = trimmed.parse().ok()?;
    if n == 0 || n > 65535 {
        return None;
    }
    Some(n as u16)
}

/// 判映像路径是否为安装时锁定的 sing-box 二进制（移植自 `winproc.go:173-178` 的 `isLockedSingbox`）。
///
/// Windows 路径大小写不敏感 → Go `strings.EqualFold`。镜像 macOS「只杀锁定二进制」：客户端不可指定，
/// 杜绝误杀外部同名进程。
#[must_use]
pub fn is_locked_singbox(image: &str, singbox_bin: &str) -> bool {
    if image.is_empty() || singbox_bin.is_empty() {
        return false;
    }
    eq_ignore_ascii_case_path(image, singbox_bin)
}

/// Windows 路径大小写不敏感比对（移植自 Go `strings.EqualFold`）。
///
/// 用于 `is_locked_singbox` / `kill_all_singbox` 的 basename 粗筛。比 `eq_ignore_ascii_case` 更贴近 Go 语义
///（Go EqualFold 处理 ASCII + 部分 Unicode，本协议路径仅 ASCII，ascii 足够）。
#[must_use]
pub fn eq_ignore_ascii_case_path(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// 轻量 basename（移植自 `winproc.go:248-254` 的 `filepathBase`）。
///
/// 取路径末段（`/` 与 `\` 均作分隔符），用于 `kill_all_singbox` 按 ExeFile basename 粗筛。
/// 避免为取末段引 std::path（Windows 上 `\` 与 `/` 都是合法分隔，std::path 在 Linux 测试会只认 `/`）。
#[must_use]
pub fn filepath_base(p: &str) -> &str {
    let trimmed = p.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// 轻量 dirname（[`filepath_base`] 的对偶）。
///
/// 取路径除末段外的前缀（`/` 与 `\` 均作分隔符），用途：起核时把子进程 CWD 定到配置文件所在目录。
/// 不引 `std::path` 的理由同 [`filepath_base`] —— `std::path` 在 Linux 上只认 `/`，会把
/// `C:\ProgramData\Polaris\config.json` 整段当成一个文件名而返回「无父目录」，于是本机单测恒绿、
/// 生产恒错。
///
/// - 无分隔符（裸文件名）→ `None`，调用方保持「继承父进程 CWD」的旧行为
/// - 根形态（`C:\cfg.json` / `\cfg.json`）→ **保留尾分隔符**（`C:\` / `\`）。Win32 里 `"C:"` 表示
///   「C 盘的当前目录」而不是 C 盘根，丢掉这个反斜杠会把 CWD 指到完全不同的地方
#[must_use]
pub fn filepath_dir(p: &str) -> Option<&str> {
    let trimmed = p.trim_end_matches(['\\', '/']);
    let i = trimmed.rfind(['\\', '/'])?;
    // `i == 0` → `\file`（根相对）；前一字节是 `:` → `C:\file`（盘符根）。两者都要把分隔符留下。
    // 按字节比对安全：UTF-8 续字节恒 ≥ 0x80，不会等于 b':'，故不会误判多字节字符的中段。
    let end = if i == 0 || trimmed.as_bytes()[i - 1] == b':' {
        i + 1
    } else {
        i
    };
    Some(&trimmed[..end])
}

/// Windows 路径归一化（移植自 Go `filepath.Clean` 的最小子集，覆盖本协议用到的形态）。
///
/// Go `filepath.Clean` 做的事很多（`./` `../` 折叠、重复分隔符合并、去尾分隔符）。本协议 cfg 路径由客户端
///（Tauri 侧）下发，通常是绝对路径；本函数做最小必要清洗：合并重复分隔符 + 去尾分隔符，足以支撑
/// [`cfg_allowed`] 的前缀比对。不折叠 `..`（客户端不应下发，纵深由前缀比对兜底 —— `..` 会让前缀失配）。
#[must_use]
pub fn normalize_path(p: &str) -> String {
    // 把正斜杠统一成反斜杠（Windows 接受两者，但前缀比对须一致），合并连续分隔符，去尾分隔符。
    let mut out = String::with_capacity(p.len());
    let mut prev_sep = false;
    for c in p.chars() {
        let is_sep = c == '\\' || c == '/';
        if is_sep {
            if prev_sep {
                continue; // 合并连续分隔符
            }
            out.push('\\');
            prev_sep = true;
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    // 去尾分隔符，但保留盘符根 `X:\`（去之会让 `C:\` 退化成 `C:`，前缀比对失配）。
    // 判根：长度 3、形如 `<letter>:\`。Go Clean 同样保留盘符根的尾分隔符。
    let is_drive_root = out.len() == 3
        && out.as_bytes()[0].is_ascii_alphabetic()
        && out.as_bytes()[1] == b':'
        && out.as_bytes()[2] == b'\\';
    if !is_drive_root {
        while out.len() > 1 && out.ends_with('\\') {
            out.pop();
        }
    }
    out
}

/// 命名管道实例的 `dwOpenMode`：**只有第一个实例**才带 `FILE_FLAG_FIRST_PIPE_INSTANCE`。
///
/// # 为什么不能每个实例都带
///
/// Win32 契约：该 flag 下「若同名管道**已存在任何实例**，再建即 `ERROR_ACCESS_DENIED`」。
/// accept 循环是「建实例 → 阻塞等连接 → 把已连接实例交给工作线程 → 回头再建一个」，
/// 而工作线程处理期间那个实例仍然存在 ⇒ 带着 flag 回头建**必然失败**，循环只能
/// 每 200ms 重试一次并刷错误日志。后果是：**整个连接处理期间没有任何监听实例**
/// （起核那条要等 15s 就绪，就是 15s 无监听），期间任何并发 helper 命令直接连接失败；
/// 而客户端零重试（`helper-client` 的 `send` 默认不重试），失败会被上层读成
/// 「helper 未安装或未运行」⇒ 报 `needs_repair` 或「起核通信失败」。
///
/// 首实例保留该 flag 是有意义的：它防的是**另一个进程抢占同名管道**（服务重复启动 /
/// 冒名服务端），这正是 flag 的设计用途。被移植的 Go 侧 `winio.ListenPipe` 也是同一口径
/// —— 只给首个实例带。
///
/// # 位值为什么硬编码
///
/// 本模块在 Linux 上（`cfg(any(target_os = "windows", test))`）也编译，好让这类纯逻辑有门可跑；
/// 而 `windows-sys` 是 `[target.'cfg(windows)'.dependencies]`，Linux 上根本不在依赖图里。
/// 故此处用 winbase.h 的字面位值，并在 `service::win` 里加一条 **windows-only 的编译期断言**
/// 钉住「字面值 == `windows-sys` 常量」——两边一动就编不过。
#[must_use]
pub const fn pipe_open_mode(first: bool) -> u32 {
    /// winbase.h `PIPE_ACCESS_DUPLEX`。
    const ACCESS_DUPLEX: u32 = 0x0000_0003;
    /// winbase.h `FILE_FLAG_FIRST_PIPE_INSTANCE`。
    const FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    if first {
        ACCESS_DUPLEX | FIRST_PIPE_INSTANCE
    } else {
        ACCESS_DUPLEX
    }
}

/// GetExtendedTcpTable 的 LocalPort 网络字节序解析（移植自 `winproc.go:294-298` 的 `localPortFromNetOrder`）。
///
/// GetExtendedTcpTable 返回的 LocalPort 是「主机内存中按网络序排布的 32 位」，低 16 位是端口。
/// 端口 = 高字节<<8 | 次高字节（取低 16 位的两字节做 ntohs）。
///
/// # 示例
///
/// 端口 9090 = 0x2382。网络序两字节 = `[0x23, 0x82]`。GetExtendedTcpTable 存为 32 位 `0x82230000`？
/// —— 非，Go 源取 `byte(p)`（低字节 = 0x82）作网络序高位、`byte(p>>8)` 作网络序低位：
/// `port = 0x82 << 8 | 0x23 = 0x8223`？不对，应为 `0x2382 = 9090`。
/// 正解：LocalPort 字段把端口的网络序两字节存进 32 位低 16 位（小端主机），故 `byte(p)` = 网络序首字节（端口高位）。
#[must_use]
pub fn local_port_from_net_order(p: u32) -> u16 {
    let b0 = p as u8; // 低字节（网络序高位）
    let b1 = (p >> 8) as u8; // 次低字节（网络序低位）
    u16::from(b0) << 8 | u16::from(b1)
}

/// LISTEN 状态码（移植自 `winproc.go:284`，`MIB_TCP_STATE_LISTEN = 2`）。
pub const MIB_TCP_STATE_LISTEN: u32 = 2;

/// AF_INET（`winproc.go:283`，`AF_INET = 2`）。
pub const AF_INET: u32 = 2;

/// AF_INET6（`winproc.go:283`，`AF_INET6 = 23`）。
pub const AF_INET6: u32 = 23;

/// TCP_TABLE_OWNER_PID_LISTENER（`winproc.go:281`，`= 3`）。
pub const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;

/// 一条 LISTEN 持有者记录（IPv4 与 IPv6 合一，字段同 Go `collect` 的闭包返回）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenEntry {
    /// 持有者 pid。
    pub pid: u32,
    /// LISTEN 端口（主机序）。
    pub port: u16,
}

/// 从「原始 LISTEN 行（pid + 网络序 port + state）」中过滤出目标端口（移植自 Go `collect` 闭包的核心）。
///
/// Go 源 `collect` 对 IPv4/IPv6 表逐行检查 `state == MIB_TCP_STATE_LISTEN` + `port == target`。
/// 本函数抽出该纯逻辑（解耦 Windows 字节布局与过滤），供 [`crate::platform::windows::ops::NetTableOps`] 的生产实现与 mock 共用。
///
/// # 参数
///
/// - `rows`：原始行（pid、网络序 port、state）。
/// - `target`：目标端口（主机序）。
///
/// # 返回
///
/// 匹配的 pid 列表（去重，保序）。
#[must_use]
pub fn filter_listen_pids(rows: &[ListenEntry], target: u16) -> Vec<u32> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in rows {
        if e.port == target && !seen.contains(&e.pid) {
            seen.insert(e.pid);
            out.push(e.pid);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== iface_allowed（逐字对照 Go helper.go:50-60）=====

    #[test]
    fn iface_allowed_matches_go_source() {
        // Go ifaceAllowed：polaris- 前缀 + rest 每字符须 [a-z0-9-]，长度 ≤ 24
        assert!(iface_allowed("polaris-ts"));
        assert!(iface_allowed("polaris-wg"));
        assert!(iface_allowed("polaris-tun0"));
        assert!(iface_allowed("polaris-abc-123"));
        assert!(iface_allowed("polaris-")); // rest 空时 Go 源返回 true（for 循环不执行）
                                            // 拒绝
        assert!(!iface_allowed("polaris-ABC")); // 大写
        assert!(!iface_allowed("polaris_abc")); // 下划线
        assert!(!iface_allowed("en0"));
        assert!(!iface_allowed(""));
        // 超长拒绝（Go: len(s) > 24）
        assert!(!iface_allowed(&format!("polaris-{}", "a".repeat(30))));
    }

    // ===== cfg_allowed（对照 Go helper.go:86-93）=====

    #[test]
    fn cfg_allowed_empty_confdir_allows_all() {
        // Go: if confDir == "" { return true }（安装期未配置兜底）
        assert!(cfg_allowed("C:\\anything\\cfg.json", ""));
        assert!(cfg_allowed("/anywhere", ""));
    }

    #[test]
    fn cfg_allowed_inside_confdir() {
        let conf = r"C:\Users\polaris\AppData\Roaming\polaris\config";
        assert!(cfg_allowed(
            r"C:\Users\polaris\AppData\Roaming\polaris\config\c.json",
            conf
        ));
        // 注：cfg_allowed(conf, conf) 在 Go 源也返回 false —— base = Clean(conf)+"\\"，clean=Clean(conf)
        //（无尾 \），HasPrefix 失配。即「配置文件必须严格位于 confDir 之内，不能是 confDir 本身」。
        assert!(!cfg_allowed(conf, conf));
    }

    #[test]
    fn cfg_allowed_case_insensitive_windows_path() {
        // Go: strings.ToLower 前缀比对 —— Windows 路径大小写不敏感
        let conf = r"C:\Users\Polaris\config";
        assert!(cfg_allowed(r"c:\users\polaris\config\c.json", conf));
    }

    #[test]
    fn cfg_allowed_rejects_outside_confdir() {
        let conf = r"C:\Users\polaris\config";
        assert!(!cfg_allowed(r"C:\Windows\System32\evil.json", conf));
        assert!(!cfg_allowed(r"C:\Users\polaris\config-other\c.json", conf)); // 前缀相似但不匹配
    }

    #[test]
    fn cfg_allowed_handles_redundant_separators() {
        // normalize 合并连续分隔符 —— 防 `conf\\cfg` 与 `conf\cfg` 失配
        let conf = r"C:\polaris\config";
        assert!(cfg_allowed(r"C:\polaris\config\\c.json", conf));
        assert!(cfg_allowed(r"C:/polaris/config/c.json", conf)); // 正斜杠
    }

    // ===== parse_port（对照 Go helper.go:298-307）=====

    #[test]
    fn parse_port_valid() {
        assert_eq!(parse_port("9090"), Some(9090));
        assert_eq!(parse_port("1"), Some(1));
        assert_eq!(parse_port("65535"), Some(65535));
        assert_eq!(parse_port("  8080  "), Some(8080)); // Go: strings.TrimSpace(readLine(r))
    }

    #[test]
    fn parse_port_rejects_bad() {
        // Go: strings.IndexFunc(非数字) >= 0 → bad-port
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("abc"), None);
        assert_eq!(parse_port("80a"), None);
        assert_eq!(parse_port("12.5"), None);
        assert_eq!(parse_port("0"), None); // Go: pnum <= 0
        assert_eq!(parse_port("65536"), None); // Go: pnum > 65535
        assert_eq!(parse_port("-1"), None); // 负号非数字
    }

    // ===== is_locked_singbox / eq_ignore_ascii_case_path（对照 winproc.go:173-178）=====

    #[test]
    fn is_locked_singbox_case_insensitive() {
        let bin = r"C:\Program Files\Polaris\sing-box.exe";
        assert!(is_locked_singbox(bin, bin));
        assert!(is_locked_singbox(
            r"c:\program files\polaris\sing-box.exe",
            bin
        ));
        assert!(!is_locked_singbox(r"C:\other\evil.exe", bin));
        assert!(!is_locked_singbox("", bin));
        assert!(!is_locked_singbox(bin, ""));
    }

    // ===== filepath_base（对照 winproc.go:248-254）=====

    #[test]
    fn filepath_base_matches_go_source() {
        assert_eq!(filepath_base(r"C:\dir\file.exe"), "file.exe");
        assert_eq!(filepath_base(r"C:\dir\file.exe\"), "file.exe"); // 去尾分隔符
        assert_eq!(filepath_base("file.exe"), "file.exe");
        assert_eq!(filepath_base("/usr/bin/sing-box"), "sing-box");
        assert_eq!(filepath_base(r"a/b\c"), "c"); // 混合分隔符
    }

    // ===== filepath_dir（起核子进程 CWD 的推导）=====

    #[test]
    fn filepath_dir_takes_parent_of_windows_paths() {
        // 生产形态：cfg 在 conf_dir 里（cfg_allowed 已保证），父目录就是要设的 CWD。
        assert_eq!(
            filepath_dir(r"C:\ProgramData\Polaris\config.json"),
            Some(r"C:\ProgramData\Polaris")
        );
        assert_eq!(
            filepath_dir("/etc/polaris/config.json"),
            Some("/etc/polaris")
        );
        assert_eq!(filepath_dir(r"a/b\c.json"), Some("a/b")); // 混合分隔符
    }

    #[test]
    fn filepath_dir_keeps_the_separator_at_a_root() {
        // `"C:"` 在 Win32 = 「C 盘的当前目录」，不是 C 盘根 —— 尾分隔符丢不得。
        assert_eq!(filepath_dir(r"C:\config.json"), Some(r"C:\"));
        assert_eq!(filepath_dir(r"\config.json"), Some(r"\"));
        assert_eq!(filepath_dir("/config.json"), Some("/"));
    }

    #[test]
    fn filepath_dir_returns_none_without_a_separator() {
        // 无父目录 → 调用方保持继承父进程 CWD 的旧行为，而不是拿一个空串去 chdir。
        assert_eq!(filepath_dir("config.json"), None);
        assert_eq!(filepath_dir(""), None);
    }

    #[test]
    fn filepath_dir_does_not_split_multibyte_characters() {
        // 分隔符前一字节按 b':' 比对：中文目录名的 UTF-8 续字节恒 ≥ 0x80，不得被误判成盘符根。
        assert_eq!(
            filepath_dir(r"C:\用户\配置\config.json"),
            Some(r"C:\用户\配置")
        );
    }

    // ===== local_port_from_net_order（对照 winproc.go:294-298）=====

    #[test]
    fn local_port_from_net_order_known_ports() {
        // 端口 9090 = 0x2382。网络序首字节=0x23（存进 32 位低字节），次字节=0x82。
        // GetExtendedTcpTable 的 LocalPort u32 低 16 位 = 0x8223（小端：低字节0x23在前? 需对照 Go 源注释）。
        // Go 源：b0 = byte(p)（低字节）作网络序高位，b1 = byte(p>>8) 作网络序低位。
        // 故若想还原端口 9090（0x2382）：网络序两字节 [0x23, 0x82]，存进 u32 使 byte(p)=0x23 → p=0x23 + 0x82<<8=0x8223。
        let p: u32 = 0x8223; // byte(p)=0x23（网络序高位）, byte(p>>8)=0x82
        assert_eq!(local_port_from_net_order(p), 9090);
        // 端口 80 = 0x0050 → 网络序 [0x00, 0x50] → u32 = 0x5000
        assert_eq!(local_port_from_net_order(0x5000), 80);
        // 端口 443 = 0x01BB → 网络序 [0x01, 0xBB] → u32 = 0xBB01
        assert_eq!(local_port_from_net_order(0xBB01), 443);
    }

    // ===== filter_listen_pids（对照 winproc.go collect 闭包过滤逻辑）=====

    #[test]
    fn filter_listen_pids_dedupes() {
        let rows = vec![
            ListenEntry {
                pid: 100,
                port: 9090,
            },
            ListenEntry {
                pid: 200,
                port: 9090,
            },
            ListenEntry {
                pid: 100,
                port: 9090,
            }, // 重复 pid → 去重
            ListenEntry { pid: 300, port: 80 }, // 非目标端口
        ];
        assert_eq!(filter_listen_pids(&rows, 9090), vec![100, 200]);
        assert!(filter_listen_pids(&rows, 9999).is_empty());
    }

    // ===== normalize_path =====

    #[test]
    fn normalize_path_collapses_separators() {
        assert_eq!(normalize_path(r"C:\\dir\\file"), r"C:\dir\file");
        assert_eq!(normalize_path(r"C:/dir/file"), r"C:\dir\file");
        assert_eq!(normalize_path(r"C:\dir\file\\"), r"C:\dir\file");
        // 根保留（C:\ 不被去成 C:）
        assert_eq!(normalize_path(r"C:\\"), r"C:\");
    }

    /// 首实例带 `FILE_FLAG_FIRST_PIPE_INSTANCE`，后续实例不带 —— 位值层面。
    #[test]
    fn only_the_first_pipe_instance_claims_the_name() {
        const FIRST_BIT: u32 = 0x0008_0000;
        const DUPLEX: u32 = 0x0000_0003;
        assert_eq!(
            pipe_open_mode(true) & FIRST_BIT,
            FIRST_BIT,
            "首实例必须带独占 flag"
        );
        assert_eq!(
            pipe_open_mode(false) & FIRST_BIT,
            0,
            "后续实例带了独占 flag 就恒 ERROR_ACCESS_DENIED"
        );
        // 两条都必须保住双向：只读句柄写不了响应（W1 修过的老坑）。
        assert_eq!(pipe_open_mode(true) & DUPLEX, DUPLEX);
        assert_eq!(pipe_open_mode(false) & DUPLEX, DUPLEX);
        // 正向对照：两者确实不同，否则上面几条可能被一个「恒相同」的实现同时满足。
        assert_ne!(pipe_open_mode(true), pipe_open_mode(false));
    }

    /// accept 循环真的把 `first` 接进去了 —— 纯函数测不到接线。
    ///
    /// `service/win.rs` 整模块 `cfg(windows)`，Linux 上不编译，但 `include_str!` 读的是磁盘文本，
    /// 不受 cfg 影响 ⇒ 这道门在本机也能跑。
    #[test]
    fn the_accept_loop_only_claims_the_name_once() {
        let src = include_str!("service/win.rs");
        let at = src.find("fn serve<").expect("serve 消失，门失去判据");
        let end = src[at..]
            .find("\nfn create_pipe_instance")
            .map_or(src.len(), |i| at + i);
        let body = &src[at..end];
        assert!(
            body.contains("create_pipe_instance(&pipe_name_w, &sddl_w, first)"),
            "accept 循环没有把 first 传给 create_pipe_instance"
        );
        assert!(body.contains("let mut first = true;"), "缺少首实例标志");
        let clear = body
            .find("first = false;")
            .expect("first 从未被清掉 —— 每个实例都会带独占 flag");
        let connect = body
            .find("connect_pipe(h)")
            .expect("connect_pipe 调用点消失");
        assert!(
            clear < connect,
            "first 必须在 connect_pipe **之前**清掉：否则连接失败那条腿回头重建时仍带独占 flag"
        );
        // 判据自检：窗口里必须真有 CreateNamedPipe 之外的循环体，否则上面几条可能落在空串上。
        assert!(body.contains("STOP_REQUESTED"), "窗口没盖住 accept 循环");
    }

    /// SCM STOP 必须能**打断**阻塞中的 `ConnectNamedPipe`，而不只是置个标志。
    ///
    /// 与 [`the_accept_loop_only_claims_the_name_once`] 同款：`service/win.rs` 整模块
    /// `cfg(windows)` 不参与 Linux 编译，但 `include_str!` 读磁盘文本不受 cfg 影响。
    /// 这条只能是源码级 —— 行为要真机（组 E）验，编译由 CI 的 windows 交叉 check 兜。
    #[test]
    fn scm_stop_wakes_the_blocked_accept_loop() {
        let src = include_str!("service/win.rs");

        // ① ctrl_handler 的 STOP 分支：置标志之外必须唤醒 + 上报中间态。
        let at = src
            .find("extern \"system\" fn ctrl_handler(")
            .expect("ctrl_handler 消失");
        let end = src[at..]
            .find("\n/// 唤醒阻塞在")
            .map_or(src.len(), |i| at + i);
        let body = &src[at..end];
        assert!(
            body.contains("STOP_REQUESTED.store(true"),
            "STOP 分支不再置停止标志"
        );
        assert!(
            body.contains("wake_accept_loop()"),
            "STOP 只置了标志没唤醒 —— accept 循环仍阻塞在 ConnectNamedPipe，要等下一个客户端才退"
        );
        assert!(
            body.contains("SERVICE_STOP_PENDING"),
            "STOP 不上报中间态 —— SCM 记录仍是 RUNNING，`sc stop` 直接返回成功而进程还在跑"
        );

        // ② 唤醒实现必须真去连自己的管道（不是空函数 / 不是 sleep）。
        let wat = src
            .find("fn wake_accept_loop()")
            .expect("wake_accept_loop 消失");
        let wend = src[wat..].find("\n}").map_or(src.len(), |i| wat + i);
        let wbody = &src[wat..wend];
        assert!(wbody.contains("CreateFileW("), "唤醒没有真发起连接");
        assert!(wbody.contains("PIPE_NAME"), "唤醒连的不是本服务的管道名");
        assert!(
            wbody.contains("OPEN_EXISTING"),
            "应当只连已存在的管道，不得创建"
        );

        // ③ 次序：STATUS_HANDLE 必须在进 serve **之前**存好，否则 ctrl_handler 读到 0、
        //    上报那步静默跳过。
        let store = src
            .find("STATUS_HANDLE.store(")
            .expect("STATUS_HANDLE 没有被写入，ctrl_handler 永远拿不到句柄");
        let serve_at = src
            .find("let _ = serve(helper.clone());")
            .expect("serve 调用点消失");
        assert!(store < serve_at, "STATUS_HANDLE 存得比 serve 还晚");
    }
}

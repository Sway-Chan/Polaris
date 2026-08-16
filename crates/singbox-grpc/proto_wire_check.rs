// vendored proto ⇄ 随包内核 wire 契约对拍（纯 std，零依赖）。
//
// # 这个文件为什么存在
//
// `tests/mock_server.rs` 用**同一份 vendored 类型**同时当 client 和 server —— 它验证的是「本仓自己
// 跟自己一致」，对「本仓与真核一致吗」这个问题结构上无法给出任何信息。2026-08-05 那次故障正好落在
// 它的盲区里：上游在 `TailscaleEndpointStatus.stateText` 插了一个字段把后面全顶掉一位，mock 测试
// 全绿，真机上整条 Tailscale STATUS 流静默死掉（详见 `proto/started_service.proto` 的对应段落）。
//
// 本模块补的就是那一格：**从随包内核二进制里把 protoc-gen-go 嵌入的 `FileDescriptorProto` 抠出来，
// 与 vendored `.proto` 文本逐字段对账**。真值取自将要被执行的那个二进制本身，不是文档、不是记忆。
//
// # 为什么手写 protobuf 解析而不引 crate
//
// 只需要 descriptor 里的三层：文件 → message → (字段名, 字段号)。全部是 varint + length-delimited
// 两种 wire type，~80 行 std 足够；为一道构建期门引 `prost-types` / `protobuf-parse` 属反向依赖
// （门本身就是用来发现 proto 层出问题的，不该再依赖一层 proto 库）。
//
// # 被谁 include
//
// `build.rs`（release 构型硬门，拒绝出包）与 `tests/bundled_core_wire.rs`（开发机 / 有核时）各
// `include!` 一次。不做成 crate 内的 `mod`：build script 不能依赖它正在构建的那个 crate。
#[allow(dead_code)]
mod proto_wire_check {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// protoc-gen-go 把 `FileDescriptorProto` 原样嵌进 .rodata；它以 field 1(name) 起头，
    /// 故 `0x0A <len> "daemon/started_service.proto"` 这串字节就是它的锚点。
    const PROTO_FILE_NAME: &[u8] = b"daemon/started_service.proto";

    /// 随包核的四个平台目录（package.yml 一律全拉，故打包机上四份都在；
    /// 开发机可能只 `fetch:core --platform=linux` 拉一份，故按「存在即检」处理）。
    const CORE_RELATIVE_PATHS: [&str; 4] = [
        "resources/linux/sing-box",
        "resources/mac-arm64/sing-box",
        "resources/mac-x64/sing-box",
        "resources/win/sing-box.exe",
    ];

    /// 仓库根（本 crate 在 `crates/singbox-grpc/`，故上跳两级）。
    pub fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    /// 盘上真实存在的随包核。空 = 尚未 `node scripts/fetch-core.mjs`（裸 checkout / CI 常态）。
    pub fn bundled_cores() -> Vec<PathBuf> {
        let root = repo_root();
        CORE_RELATIVE_PATHS
            .iter()
            .map(|p| root.join(p))
            .filter(|p| p.is_file())
            .collect()
    }

    // ── 最小 protobuf wire 读取 ────────────────────────────────────────────────

    fn read_varint(b: &[u8], i: &mut usize) -> Option<u64> {
        let (mut out, mut shift) = (0u64, 0u32);
        loop {
            let c = *b.get(*i)?;
            *i += 1;
            out |= u64::from(c & 0x7F) << shift;
            if c & 0x80 == 0 {
                return Some(out);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }

    /// 逐字段扫一个完整 buffer，产出 `(字段号, wire type, 值切片/varint)`。
    /// 只需要 varint(0) 与 length-delimited(2)；descriptor 里不出现其它 wire type，遇到即判定越界停下。
    fn each_field(b: &[u8], mut f: impl FnMut(u32, u8, &[u8], u64)) -> bool {
        let mut i = 0usize;
        while i < b.len() {
            let Some(key) = read_varint(b, &mut i) else {
                return false;
            };
            let (num, wt) = ((key >> 3) as u32, (key & 7) as u8);
            if num == 0 {
                return false;
            }
            match wt {
                0 => {
                    let Some(v) = read_varint(b, &mut i) else {
                        return false;
                    };
                    f(num, wt, &[], v);
                }
                2 => {
                    let Some(len) = read_varint(b, &mut i) else {
                        return false;
                    };
                    let end = i.saturating_add(len as usize);
                    if end > b.len() {
                        return false;
                    }
                    f(num, wt, &b[i..end], 0);
                    i = end;
                }
                _ => return false,
            }
        }
        true
    }

    /// 一份 rawDesc 解出的全部符号表。
    ///
    /// `messages`：message 名 → (字段名 → 字段号)。
    /// `enums`：enum 名 → (值名 → 值号)。**枚举值号同属 wire 契约**：`DefaultLogLevel` 的全部载荷
    /// 就是一个枚举数字，值序漂了就是把 warn 显示成 info——比不显示更糟。
    #[derive(Debug, Default, Clone)]
    pub struct CoreDescriptor {
        pub messages: BTreeMap<String, BTreeMap<String, u32>>,
        pub enums: BTreeMap<String, BTreeMap<String, u32>>,
    }

    /// 从二进制里定位 rawDesc 并解析出全部 message / enum 符号表。
    ///
    /// rawDesc 在 Go 二进制里没有自带长度前缀（长度存在别处的 string header 里），故从锚点起**贪心**
    /// 逐字段解析，遇到不像 `FileDescriptorProto` 的 tag 即停——与逐个二进制实测出的边界一致
    /// （linux/mac-arm64 的 beta.7、以及 alpha.40 三份都在此策略下解出完整 message 表）。
    pub fn descriptor_from_core(binary: &[u8]) -> Result<CoreDescriptor, String> {
        let mut needle = Vec::with_capacity(PROTO_FILE_NAME.len() + 2);
        needle.push(0x0A);
        needle.push(PROTO_FILE_NAME.len() as u8);
        needle.extend_from_slice(PROTO_FILE_NAME);

        let start = binary
            .windows(needle.len())
            .position(|w| w == needle)
            .ok_or_else(|| {
                format!(
                    "在二进制里找不到 `{}` 的 FileDescriptorProto 锚点 —— \
                     该核可能不是 sing-box、或上游改了 daemon proto 的文件路径。",
                    String::from_utf8_lossy(PROTO_FILE_NAME)
                )
            })?;

        // 贪心切一段足够大的窗口；`each_field` 自己会在非法 tag 处停。
        let window = &binary[start..binary.len().min(start + (1 << 20))];
        let mut out = CoreDescriptor::default();
        let mut i = 0usize;
        while i < window.len() {
            let mut j = i;
            let Some(key) = read_varint(window, &mut j) else {
                break;
            };
            let (num, wt) = ((key >> 3) as u32, (key & 7) as u8);
            // FileDescriptorProto 的合法 tag 集；越界即认为 rawDesc 到头了。
            if !matches!(num, 1..=9 | 12) || !matches!(wt, 0 | 2) {
                break;
            }
            let payload: &[u8] = if wt == 2 {
                let Some(len) = read_varint(window, &mut j) else {
                    break;
                };
                let end = j.saturating_add(len as usize);
                if end > window.len() {
                    break;
                }
                let p = &window[j..end];
                j = end;
                p
            } else {
                if read_varint(window, &mut j).is_none() {
                    break;
                }
                &[]
            };
            // FileDescriptorProto：4=message_type, 5=enum_type。
            match num {
                4 => collect_message(payload, "", &mut out),
                5 => collect_enum(payload, "", &mut out.enums),
                _ => {}
            }
            i = j;
        }

        if out.messages.is_empty() {
            return Err(
                "找到了锚点但没解析出任何 message —— rawDesc 结构可能已变，请重核解析策略。".into(),
            );
        }
        Ok(out)
    }

    /// 递归收集 `DescriptorProto`（field 1=name, 2=field, 3=nested_type, 4=enum_type）。
    fn collect_message(buf: &[u8], prefix: &str, out: &mut CoreDescriptor) {
        let mut name = String::new();
        let mut fields: Vec<Vec<u8>> = Vec::new();
        let mut nested: Vec<Vec<u8>> = Vec::new();
        let mut nested_enums: Vec<Vec<u8>> = Vec::new();
        each_field(buf, |num, wt, val, _| {
            if wt != 2 {
                return;
            }
            match num {
                1 => name = String::from_utf8_lossy(val).into_owned(),
                2 => fields.push(val.to_vec()),
                3 => nested.push(val.to_vec()),
                4 => nested_enums.push(val.to_vec()),
                _ => {}
            }
        });
        if name.is_empty() {
            return;
        }
        let full = format!("{prefix}{name}");
        let mut map = BTreeMap::new();
        for fb in &fields {
            // FieldDescriptorProto：field 1=name(string), 3=number(varint)。
            let (mut fname, mut fnum) = (String::new(), 0u32);
            each_field(fb, |num, wt, val, v| match (num, wt) {
                (1, 2) => fname = String::from_utf8_lossy(val).into_owned(),
                (3, 0) => fnum = v as u32,
                _ => {}
            });
            if !fname.is_empty() && fnum != 0 {
                map.insert(fname, fnum);
            }
        }
        out.messages.insert(full.clone(), map);
        for n in &nested {
            collect_message(n, &format!("{full}."), out);
        }
        for e in &nested_enums {
            collect_enum(e, &format!("{full}."), &mut out.enums);
        }
    }

    /// 收集 `EnumDescriptorProto`（field 1=name, 2=value）。
    ///
    /// 值号用 varint 且**合法取 0**（`PANIC = 0`），故不能像字段号那样拿 `0` 当「没读到」丢掉：
    /// 缺 `number` 字段按 protobuf 语义即默认值 0，与「显式写 0」同义。
    fn collect_enum(buf: &[u8], prefix: &str, out: &mut BTreeMap<String, BTreeMap<String, u32>>) {
        let mut name = String::new();
        let mut values: Vec<Vec<u8>> = Vec::new();
        each_field(buf, |num, wt, val, _| match (num, wt) {
            (1, 2) => name = String::from_utf8_lossy(val).into_owned(),
            (2, 2) => values.push(val.to_vec()),
            _ => {}
        });
        if name.is_empty() {
            return;
        }
        let mut map = BTreeMap::new();
        for vb in &values {
            // EnumValueDescriptorProto：field 1=name(string), 2=number(varint)。
            let (mut vname, mut vnum) = (String::new(), 0u32);
            each_field(vb, |num, wt, val, v| match (num, wt) {
                (1, 2) => vname = String::from_utf8_lossy(val).into_owned(),
                (2, 0) => vnum = v as u32,
                _ => {}
            });
            if !vname.is_empty() {
                map.insert(vname, vnum);
            }
        }
        out.insert(format!("{prefix}{name}"), map);
    }

    // ── vendored .proto 文本解析 ───────────────────────────────────────────────

    /// 从 `.proto` 源文本里取某个 message 的 `字段名 → 字段号`。
    ///
    /// `message` 支持**带点的嵌套全名**（`Log.Message`），与 descriptor 侧
    /// [`collect_message`] 产出的全名同一套词汇 —— 否则 `Log.Message` 这类嵌套消息只能靠拍平成顶层
    /// 才进得了对拍表，而拍平之后真核 descriptor 里点名不到它，那格覆盖面就是空的。
    ///
    /// 只认最朴素的声明形态（`<type> <name> = <num>;`，可带 `repeated`/`optional` 前缀），嵌套块
    /// 整段跳过（由带点路径单独对拍）。行扫而非写文法：目标是让**门自身足够笨**——门比被测对象复杂
    /// 就轮到门自己出错了。
    pub fn message_from_proto_src(
        src: &str,
        message: &str,
    ) -> Result<BTreeMap<String, u32>, String> {
        let mut body: Vec<&str> = src.lines().collect();
        for seg in message.split('.') {
            body = block_body(&body, &format!("message {seg} {{")).ok_or_else(|| {
                format!("vendored proto 里找不到 `message {seg}`（对拍名 `{message}`）")
            })?;
        }
        fields_from_block(&body, message)
    }

    /// 取 `header` 那一行之后、到与之配对的 `}` 之前的全部行（不含首尾两行本身）。
    /// 花括号计数只看代码部分（行注释先剥掉），故注释里的括号不会把配对算歪。
    fn block_body<'a>(lines: &[&'a str], header: &str) -> Option<Vec<&'a str>> {
        let start = lines.iter().position(|l| l.trim() == header)?;
        let mut depth = 1usize;
        let mut out = Vec::new();
        for line in &lines[start + 1..] {
            let code = line.split("//").next().unwrap_or("");
            depth += code.matches('{').count();
            let closes = code.matches('}').count();
            if closes >= depth {
                return Some(out); // 本块闭合
            }
            depth -= closes;
            out.push(*line);
        }
        None // 花括号没闭合
    }

    /// 一个 message 块体 → `字段名 → 字段号`。嵌套块（`message X {` / `oneof x {`）整段跳过。
    fn fields_from_block(body: &[&str], message: &str) -> Result<BTreeMap<String, u32>, String> {
        let mut out = BTreeMap::new();
        let mut skip_depth = 0usize;
        for line in body {
            let code = line.split("//").next().unwrap_or("").trim();
            if code.is_empty() {
                continue;
            }
            if skip_depth > 0 {
                skip_depth += code.matches('{').count();
                skip_depth -= code.matches('}').count().min(skip_depth);
                continue;
            }
            if code.ends_with('{') {
                skip_depth = 1; // 嵌套块：本层不收，带点路径单独对拍
                continue;
            }
            let mut tok: Vec<&str> = code.split_whitespace().collect();
            if tok.first() == Some(&"repeated") || tok.first() == Some(&"optional") {
                tok.remove(0);
            }
            // 期望形态：<type> <name> = <num>;
            if tok.len() != 4 || tok[2] != "=" {
                return Err(format!(
                    "vendored proto 里这行看不懂（门只认最朴素的字段声明）：{code}"
                ));
            }
            let num: u32 = tok[3]
                .trim_end_matches(';')
                .parse()
                .map_err(|_| format!("字段号不是数字：{code}"))?;
            out.insert(tok[1].to_string(), num);
        }
        if out.is_empty() {
            return Err(format!("`message {message}` 里一个字段都没解出来"));
        }
        Ok(out)
    }

    /// 从 `.proto` 源文本里取某个 enum 的 `值名 → 值号`。同 [`message_from_proto_src`] 的取舍：
    /// 只认最朴素的 `NAME = <num>;`，门自身足够笨。
    pub fn enum_from_proto_src(src: &str, name: &str) -> Result<BTreeMap<String, u32>, String> {
        let header = format!("enum {name} {{");
        let mut lines = src.lines().skip_while(|l| l.trim() != header.trim());
        lines
            .next()
            .ok_or_else(|| format!("vendored proto 里找不到 `enum {name}`"))?;

        let mut out = BTreeMap::new();
        for line in lines {
            let code = line.split("//").next().unwrap_or("").trim();
            if code == "}" {
                return Ok(out);
            }
            if code.is_empty() {
                continue;
            }
            // 期望形态：<NAME> = <num>;
            let tok: Vec<&str> = code.split_whitespace().collect();
            if tok.len() != 3 || tok[1] != "=" {
                return Err(format!(
                    "vendored proto 里这行看不懂（门只认最朴素的枚举值声明）：{code}"
                ));
            }
            let num: u32 = tok[2]
                .trim_end_matches(';')
                .parse()
                .map_err(|_| format!("枚举值号不是数字：{code}"))?;
            out.insert(tok[0].to_string(), num);
        }
        Err(format!("`enum {name}` 的花括号没闭合"))
    }

    /// 被对拍的符号种类。message 与 enum 的取证路径不同（descriptor 里分别是 field 4 / field 5），
    /// 但判据完全一样：**vendored 声明的每一项，号必须与真核一致**。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SymbolKind {
        Message,
        Enum,
    }

    impl SymbolKind {
        fn label(self) -> &'static str {
            match self {
                SymbolKind::Message => "message",
                SymbolKind::Enum => "enum",
            }
        }
    }

    /// vendored proto 文本里某个 message/enum 的号表。
    pub fn symbol_from_proto_src(
        src: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<BTreeMap<String, u32>, String> {
        match kind {
            SymbolKind::Message => message_from_proto_src(src, name),
            SymbolKind::Enum => enum_from_proto_src(src, name),
        }
    }

    // ── 对拍 ──────────────────────────────────────────────────────────────────

    /// 判据：**vendored 声明的每一项（message 字段 / enum 值），号必须与真核一致**。
    ///
    /// 只做单向（vendored ⊆ real）而不要求两侧集合相等 —— 上游新增我们不消费的字段是常态、无害；
    /// 而「我们声明了、但号对不上」才是会把整帧解崩的那一类。返回全部不一致项，不在第一条就短路：
    /// 字段号漂移往往是**成片**的（插一个字段后面全 +1），只报第一条会让人误以为是孤立笔误。
    pub fn diff(vendored: &BTreeMap<String, u32>, real: &BTreeMap<String, u32>) -> Vec<String> {
        let mut bad = Vec::new();
        for (name, num) in vendored {
            match real.get(name) {
                Some(r) if r == num => {}
                Some(r) => bad.push(format!("  {name}: vendored={num} 真核={r}")),
                None => bad.push(format!("  {name}: vendored={num} 真核**无此项**")),
            }
        }
        bad
    }

    /// 一次完整对拍：读核 → 抠 descriptor → 与 vendored proto 文本比某个 message/enum。
    /// `Ok(())` = 一致；`Err(人类可读报告)` = 不一致或提取失败。
    pub fn check_core_against_proto(
        core: &Path,
        proto_src: &str,
        kind: SymbolKind,
        name: &str,
    ) -> Result<(), String> {
        let bytes =
            std::fs::read(core).map_err(|e| format!("读不到随包核 {}：{e}", core.display()))?;
        let desc =
            descriptor_from_core(&bytes).map_err(|e| format!("{}：{e}", core.display()))?;
        let table = match kind {
            SymbolKind::Message => &desc.messages,
            SymbolKind::Enum => &desc.enums,
        };
        let kind_label = kind.label();
        let real = table.get(name).ok_or_else(|| {
            format!(
                "{} 的 descriptor 里没有 {kind_label} `{name}`",
                core.display()
            )
        })?;
        let vendored = symbol_from_proto_src(proto_src, kind, name)?;
        let bad = diff(&vendored, real);
        if bad.is_empty() {
            return Ok(());
        }
        Err(format!(
            "vendored proto 与随包内核的 wire 契约不一致：{}\n\
             {kind_label} `{name}` 的号对不上：\n{}\n\n\
             后果（2026-08-05 真机实证）：prost 对 wire type 不匹配零容忍，错位后**整帧解码失败**，\n\
             而 ReconnectingStream 把它当断线无限重连 —— 外部只看得到「没有下一帧」，功能整块静默消失。\n\
             修复：以随包核的 descriptor 为准改 `crates/singbox-grpc/proto/started_service.proto`，\n\
             不要以文档或旧版本记忆为准。真核字段号可直接读出：\n\
               strings -n 6 {} | grep -oE 'protobuf:\"[^\"]*\"' | sort -u",
            core.display(),
            bad.join("\n"),
            core.display(),
        ))
    }

    // ── 共享真值：符号表 + vendored proto 文本 ────────────────────────────────
    //
    // 这两项此前在 `build.rs` 与 `tests/bundled_core_wire.rs` 各存一份，两处注释都写着
    // 「一处漏加，另一处就白守」——那是一个已知的漂移源，且随着运行期也要用它而变成三份。
    // 下沉到本文件后三个消费点共用一份。
    //
    // `include_str!` 的相对路径按**本文件所在目录**解析（`crates/singbox-grpc/`），
    // 故三个 include 点（build.rs / tests / lib）拿到的是同一份文本。

    /// vendored proto 原文。
    pub const PROTO_SRC: &str = include_str!("proto/started_service.proto");

    /// 必须与真核 wire 一致的符号表。**新增的消费面必须进表**（理由见 `build.rs` 的
    /// `assert_proto_matches_bundled_core` 文档：立手法优先于铺覆盖面）。
    pub const CHECKED_SYMBOLS: &[(SymbolKind, &str)] = &[
        (SymbolKind::Message, "TailscaleEndpointStatus"),
        (SymbolKind::Message, "DefaultLogLevel"),
        (SymbolKind::Enum, "LogLevel"),
        (SymbolKind::Message, "Log"),
        (SymbolKind::Message, "Log.Message"),
        // Taildrop 收件侧的全部消费面（1.14.0-beta.15）。进表的理由同 `Log`：这几个的字段撞号
        // 不会报错，只会静默失真 —— `TaildropFile.size/modifiedAt` 都是 int64、`senderName/name`
        // 都是 string，互换后收件箱照常渲染，只是把文件名当成发件人、把时间当成大小。
        // `TaildropReceivingFile` 更硬：`senderID` + `name` 是 `CancelTaildropReceiving` 的定位键，
        // 撞号 = 取消到别的文件上去。
        (SymbolKind::Message, "TaildropInbox"),
        (SymbolKind::Message, "TaildropFile"),
        (SymbolKind::Message, "TaildropReceivingFile"),
        (SymbolKind::Message, "DownloadTaildropFileChunk"),
    ];

    /// 对**内存里的一份核**做整表对拍的三态结论。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum WireVerdict {
        /// 表里每个符号的字段号都对得上。
        Match,
        /// **有字段号对不上** —— 唯一「危险到该拦」的一档，附人类可读报告。
        Mismatch(String),
        /// 取不到判据（抠不出 descriptor / 表里的符号在该核里不存在 / vendored 侧解析失败）。
        Unobservable(String),
    }

    /// 整表对拍一份**内存中的**核（不落盘、不需要路径）。
    ///
    /// # 为什么「符号缺失」与「提取失败」都判 `Unobservable` 而不是 `Mismatch`
    ///
    /// 两档的**失败响度**不同，而这个结论会被用来拦一次用户主动发起的换核：
    ///
    /// - **字段号对不上** ⇒ prost 整帧解码失败 ⇒ `ReconnectingStream` 当断线无限重连 ⇒
    ///   功能整块**静默**消失（2026-08-05 真机实证）。用户看不出，必须拦。
    /// - **符号根本不在** ⇒ 对应 rpc 直接 `Unimplemented` / 空表，失败是**响亮**的。
    /// - **抠不出 descriptor** ⇒ 我们对这份核一无所知 —— 「没观测到 ≠ 观测到没问题」，
    ///   据此拦下用户自选的核属于把一次读失败升级成一次功能剥夺。
    ///
    /// 故只有第一档拦，其余两档放行 + 大声告警。判据方向与 `runtime::proxy::pid_identity_verdict`
    /// 一致（那里同样是「取不到材料一律 Unobservable，绝不折成不匹配」）。
    pub fn verdict_for_core_bytes(bytes: &[u8]) -> WireVerdict {
        let desc = match descriptor_from_core(bytes) {
            Ok(d) => d,
            Err(e) => return WireVerdict::Unobservable(e),
        };
        let mut bad = Vec::new();
        for (kind, name) in CHECKED_SYMBOLS {
            let table = match kind {
                SymbolKind::Message => &desc.messages,
                SymbolKind::Enum => &desc.enums,
            };
            let Some(real) = table.get(*name) else {
                return WireVerdict::Unobservable(format!(
                    "该核的 descriptor 里没有 {} `{name}`",
                    kind.label()
                ));
            };
            let vendored = match symbol_from_proto_src(PROTO_SRC, *kind, name) {
                Ok(v) => v,
                Err(e) => return WireVerdict::Unobservable(e),
            };
            for line in diff(&vendored, real) {
                bad.push(format!("{} `{name}`{line}", kind.label()));
            }
        }
        if bad.is_empty() {
            WireVerdict::Match
        } else {
            WireVerdict::Mismatch(bad.join("\n"))
        }
    }
}

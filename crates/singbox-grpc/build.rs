// proto 编译：tonic-prost-build（tonic 0.14 起 prost 编译从 tonic-build 拆出到本 crate）。
// 内部用 prost-build（默认经 protoc；CI 需装 protobuf-compiler）解析 vendored proto。
// vendored proto = crate 内 proto/started_service.proto（1:1 Polaris singbox-api-client.ts）。
//
// client + server 都生成：client 是本 crate 对外暴露面；server trait 仅供集成测试 mock 用
// （测试起一条 h2c tonic transport::Server，验证客户端连接/认证/流/重连）。

include!("proto_wire_check.rs");

// 符号表与 vendored proto 原文均取自 `proto_wire_check`（此前 build.rs / tests 各存一份，
// 两处注释都写着「一处漏加，另一处就白守」；运行期换核检查要用第三次，故下沉共用）。
use proto_wire_check::{CHECKED_SYMBOLS, PROTO_SRC};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_proto_matches_bundled_core();
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/started_service.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/started_service.proto");
    println!("cargo:rerun-if-changed=proto_wire_check.rs");
    Ok(())
}

/// **vendored proto ⇄ 随包内核 wire 契约断言（打包期硬门）**。
///
/// 与 `src-tauri/build.rs` 的 `assert_bundled_geo_data` / `assert_bundled_dashboard` 同口径、同机制：
/// **只在 release 生效**（release ⟺ 会被打包分发的那份），debug（开发 / CI 单测）不阻断。
///
/// # 为什么这道门必须落在 build script，而不能只做成 cargo test
///
/// `ci.yml` **不拉核** —— 它只 `mkdir resources/{linux,win,mac-*}` + `touch .keep` 让 tauri 的路径
/// 校验过关（见该文件「Create resource placeholder dirs」步）。于是任何依赖真核的 cargo test 在 CI 上
/// 只能 skip，做成唯一防线就正好是「门在 CI 上恒绿零信息量」。而 `package.yml` 在构建前**四平台一律
/// 全拉**（"三平台一律全拉（不按 runner 过滤）——支持 Linux 上交叉构建"），故 release 构型下核必然在盘上，
/// 这道门在真正会出包的那条腿上有牙。
///
/// 分工因此是：本函数守 release/打包；`tests/bundled_core_wire.rs` 守开发机（核已 fetch 时自动生效），
/// 并且**无核也能跑**那一半——它把解析器与对拍器喂合成 descriptor 做变异验证，使 CI 上仍有信息量。
///
/// # 判据
///
/// 「盘上存在的每一份核都要一致」，而不是「至少有一份一致」：四平台核同版本同 descriptor，
/// 若某一份对不上，说明 fetch 拉串了版本 —— 那正是要拦的事故，不该被另外三份的绿掩盖。
/// 一份都没有 ⇒ release 构型下直接拒绝出包（同 geo/dashboard 那两条：出包必须带齐随包资源）。
///
/// # 表里为什么是这三个符号，而不是全部
///
/// 立手法优先于铺覆盖面，但**新增的消费面必须进表**。
///
/// - `TailscaleEndpointStatus`：唯一有实证事故的那个（上游在 f3 插 `stateText` 把后面全顶掉一位，
///   详见 proto 文件内的段落）——这道门就是为它建的。
/// - `DefaultLogLevel` + `LogLevel`：`GetDefaultLogLevel` 的全部载荷。这条 rpc 的产物是直接显示给
///   用户看的「核在跑的真实级别」，**枚举值序错一位就是把 warn 说成 info**，而那正好是这处自证要
///   揭穿的那类谎 —— 显示一个错的真值比不显示更糟，故枚举与 message 一并进表。
/// - `Log` + `Log.Message`：`SubscribeLog` 的全部载荷，也是日志页现在**唯一**的核日志来源。
///   `reset` 与 `messages` 撞号即「历史帧被当增量重放」或「整帧解不开后无限重连」；`Log.Message`
///   的 `level`/`message` 撞号则是把级别与正文互换 —— 两者都会让日志页静默失真，故连嵌套那层一起进表。
///
/// 扩到全部 message 仍是显然的下一步，但要不要扩由人决定，不在此自作主张 —— 扩的成本主要在
/// 「上游合法新增字段」会不会把门变成噪声源，那需要单独判。
fn assert_proto_matches_bundled_core() {
    for rel in [
        "resources/linux/sing-box",
        "resources/mac-arm64/sing-box",
        "resources/mac-x64/sing-box",
        "resources/win/sing-box.exe",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            proto_wire_check::repo_root().join(rel).display()
        );
    }

    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }

    let cores = proto_wire_check::bundled_cores();
    assert!(
        !cores.is_empty(),
        "随包 sing-box 内核缺失，拒绝出包：{}/resources/*/sing-box 一份都不存在。\n\
         后果：无法验证 vendored proto 与真核的 wire 契约 —— 这条契约一旦漂移，管理 API 的整条流会\
         静默死掉（2026-08-05 真机：Tailscale 组网列表整块消失，且零日志）。\n\
         修复：node scripts/fetch-core.mjs",
        proto_wire_check::repo_root().display()
    );

    for core in &cores {
        for (kind, name) in CHECKED_SYMBOLS {
            if let Err(report) =
                proto_wire_check::check_core_against_proto(core, PROTO_SRC, *kind, name)
            {
                panic!("{report}");
            }
        }
    }
}

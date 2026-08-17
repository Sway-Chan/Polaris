//! 下载/FS trait 抽象（移植纪律：测试 mock，不在本 crate 触碰宿主网络/FS）。
//!
//! Polaris 的网络获取（`CoreDownloader.downloadFile` / `UpdateService.downloadFile`）和文件操作
//! （`CoreUpdateService.installCoreFromDir` / `backupCurrentCore` / `restoreBackup`）都直接耦合
//! Electron `net.request` / Node `fs`，无法在纯逻辑层单测。本 crate 把它们抽象为两个 trait：
//!
//! - [`UpdateDownloader`]：`download(url) -> bytes`（+ 失败/镜像/超时细节由实现封装）。
//!   生产实现（不在本 crate）注入真实 HTTP；测试注入 [`MemoryDownload`] 内存 map。
//! - [`UpdateFs`]：staged 暂存目录 + 目标目录的文件读写 + 原子 rename。
//!   生产实现注入真实 std::fs；测试注入 [`MockFs`]（in-memory 或 tempfile）。
//!
//! 关键：trait 返回 [`Result`] 而非 panic，让 staged 周期能把下载/FS 失败转成 [`StagedUpdateError`]
//! 走 Error 态重试，对齐 Polaris handleError → error 态重试循环。

use std::path::{Path, PathBuf};

use thiserror::Error;

/// 下载错误（trait 实现的失败统一形状，移植自 Polaris 下载失败分类）。
#[derive(Debug, Error)]
pub enum DownloadError {
    /// HTTP 非 2xx（= 上游 `Download failed: HTTP {code}`）。
    #[error("download failed: HTTP {0}")]
    HttpStatus(u16),
    /// 下载不完整：实收字节与期望不符（= 上游 `下载不完整：收到 {n} 字节，期望 {m}`）。
    #[error("download incomplete: received {received} bytes, expected {expected}")]
    Incomplete { received: u64, expected: u64 },
    /// 停滞超时（= 上游 `下载停滞超时（30s 无数据）`）。
    #[error("download stalled: no data for {0}ms")]
    Stalled(u64),
    /// 网络/IO 错误（= Polaris request 'error' / response 'error'）。
    #[error("download io error: {0}")]
    Io(#[from] std::io::Error),
    /// **下载后端根本未接线**（见 [`UnavailableDownloader`]；生产已注入 `CoreDownloader`，本变体
    /// 现只在纯逻辑层测试与 trait 契约上出现）。
    ///
    /// 与其余变体的语义区别是**关键**的：其余变体表示「试过了、失败了」（网络抖动/限流/校验不过 →
    /// 可重试）；本变体表示「压根没试，因为没有后端」（重试无意义，必须先接 HTTP 栈）。
    /// 宿主层据此映射结构化错误码 `HTTP_BACKEND_UNAVAILABLE`，让 UI 能区分
    /// 「功能未接线」与「下载失败」——**绝不可折叠进 [`DownloadError::Other`]**，
    /// 那会让「未接线」伪装成一次普通的失败重试，正是 §N4「绿灯空功能」的形态。
    #[error("download backend unavailable: {0}")]
    BackendUnavailable(String),
    /// 其它（如 mirror 全部失败、URL 非法）。
    #[error("download error: {0}")]
    Other(String),
}

/// 下载 trait：把 URL 解析为字节序列（移植自 `CoreDownloader.downloadFile` / `UpdateService.downloadFile`）。
///
/// 实现负责：
///   - HTTP GET + 重定向跟随 + User-Agent（上游 `APP_USER_AGENT`）
///   - Content-Length 完整性校验（上游 `parseExpectedBytes`）
///   - 停滞看门狗（上游 `createIdleTimeout`，30s 无数据 abort）
///   - GitHub 镜像回退（上游 `ghMirrorUrl`，失败自动换镜像重试一次）
///
/// 本 trait 只暴露 `download` 一个方法，让纯逻辑层不关心上述细节；实现可注入真实 HTTP 客户端或测试 mock。
pub trait UpdateDownloader {
    /// 下载 `url` 指向的全部字节。失败返回 [`DownloadError`]（由调用方转 [`StagedUpdateError`]）。
    ///
    /// 返回 `Vec<u8>` 而非写文件：让 staged 周期在校验前把字节留在内存（SHA256 校验纯函数化），
    /// 校验通过再交 [`UpdateFs`] 落盘，避免「下完即写盘、校验失败再删残件」的来回（Polaris 原实现写 temp
    /// 文件再 unlink，本 crate 简化为内存流转）。
    fn download(&self, url: &str) -> Result<Vec<u8>, DownloadError>;
}

/// 内存下载器（测试 mock）：URL → 字节的静态 map。
///
/// 不触碰网络。查不到 key 返回 [`DownloadError::Other`]。用于 staged 周期单测注入。
#[derive(Debug, Default, Clone)]
pub struct MemoryDownload {
    entries: std::collections::HashMap<String, Vec<u8>>,
}

impl MemoryDownload {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入一条 URL → 字节映射。
    #[must_use]
    pub fn with(mut self, url: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.entries.insert(url.into(), bytes);
        self
    }

    /// 插入映射（可变引用版，便于批量构建）。
    pub fn insert(&mut self, url: impl Into<String>, bytes: Vec<u8>) {
        self.entries.insert(url.into(), bytes);
    }
}

impl UpdateDownloader for MemoryDownload {
    fn download(&self, url: &str) -> Result<Vec<u8>, DownloadError> {
        self.entries
            .get(url)
            .cloned()
            .ok_or_else(|| DownloadError::Other(format!("mock: no entry for {url}")))
    }
}

/// **永远失败**的下载器 —— 标记「本仓尚无 HTTP/TLS 后端」这一事实，而非一个「HTTP 下载器」。
///
/// # 为什么改名（原 `HttpDownload`，2026-07-16）
///
/// 原类型名为 `HttpDownload`、文档写「真实 HTTP 下载器的薄占位实现」，但 `download()` **无条件返回错误**。
/// 这是 §K7.1 点名的「组合面无门」的教科书形态，且比 `spawner.rs` 的 argv bug 更隐蔽：
///
/// - 名字读起来像一个可用的 HTTP 客户端 → 宿主层 `HttpDownload::new()` 装上，编译过、类型对；
/// - 全部单测注入 [`MemoryDownload`] → **mock 全绿**；
/// - **生产路径第一次真下载必失败**，且失败信息是泛化的 `Other(...)`，与「网络抖动」难以区分
///   → 上层大概率当成可重试的网络错误，无限重试一个永不成功的调用。
///
/// 「有测试」与「测到了生产路径」是两件事。改名 + 专用错误变体让这个事实在**每个调用点**都刺眼，
/// 而不是藏在一句文档注释里。
///
/// # 本仓为什么没有 HTTP 后端（实证，2026-07-16）
///
/// # HTTP 栈已落地 —— 本类型不再是生产路径（2026-07-29 订正）
///
/// 本段原记「整个 workspace 不存在任何 TLS 栈」（`cargo tree -i reqwest` 空、`rustls` 未引、
/// 唯一的 `hyper 1` 来自 `polaris-singbox-grpc` 且是明文 h2c，够不到 `https://api.github.com`），
/// 那是**依赖决策待用户拍板期间**的状态，**已过时**：宿主已引
/// `reqwest 0.13`（`rustls-no-provider` + `socks`）+ `rustls 0.23`（`ring` provider），
/// 生产实现是 `src-tauri/src/runtime/http.rs` 的 `CoreDownloader`（15s 超时 / OOM 闸 / 限流分类 /
/// 镜像回退 / 完整性校验 / idle 看门狗俱全，带端到端单测），并已在注入点取代本类型。
///
/// 故 [`DownloadError::BackendUnavailable`] 在**生产不可达**；本类型保留的价值只剩两条：
/// ① 纯逻辑层单测的零 I/O 替身；② 让「没有后端」在类型层面仍与「试过了、失败了」可区分——
/// 这条语义边界不能因为当前有后端就折叠掉（见下方 `impl` 注与
/// `unavailable_downloader_reports_backend_unavailable_not_generic_failure`）。
///
/// 注意 **Rust 侧只实现一次**：上游的 `core-downloader.ts` 与 `UpdateService.ts` 各写了一份
/// 约 170 行的同构编排，两文件注释互指重复，勿把两份一起搬。
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableDownloader;

impl UnavailableDownloader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 结构化错误码：宿主层把它原样放进 `ApiResponse.code`，供 UI 区分
    /// 「功能未接线」（本码）与「下载失败」（其余）。
    pub const CODE: &'static str = "HTTP_BACKEND_UNAVAILABLE";
}

impl UpdateDownloader for UnavailableDownloader {
    fn download(&self, _url: &str) -> Result<Vec<u8>, DownloadError> {
        // 专用变体（非 Other）：调用方**必须**能把「没有后端」与「下载失败」分开处理。
        Err(DownloadError::BackendUnavailable(
            "未注入真实下载后端（占位 UnavailableDownloader）；\
             生产装配应注入 src-tauri runtime::http::CoreDownloader"
                .into(),
        ))
    }
}

/// FS trait：staged 暂存 + 目标落位 + 原子 rename（移植自 `CoreUpdateService.installCoreFromDir`
/// 的非 Windows staged-rename 落位，`CoreUpdateService.ts:453-479`）。
///
/// 抽象出的最小 FS 面：写临时文件 + 原子 rename + 读/删/列目录。生产实现包 std::fs；测试可注入
/// tempfile 或 in-memory FS。**trait 不暴露权限/签名操作**（macOS codesign / Windows UAC 由宿主
/// privilege 层负责，超出本纯逻辑 crate 边界）。
pub trait UpdateFs {
    /// 把 `bytes` 写到 `path`（覆盖已存在）。= 上游 `fs.writeFileSync` / `fs.copyFileSync`。
    ///
    /// 调用方负责确保父目录存在（经 [`UpdateFs::create_dir_all`]）。
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), std::io::Error>;

    /// 打开 `path` 的**流式写句柄**（截断已存在内容；父目录须已存在，同 [`UpdateFs::write`]）。
    ///
    /// # 为什么 `write` 不够用
    ///
    /// [`UpdateFs::write`] 吃 `&[u8]` ⇒ 调用方必须先把整个负载攒在内存里。App 安装包是几十 MiB
    /// 到上百 MiB 量级，「整包入内存再落盘」把内存峰值与包体积绑死，还逼下载腿在校验前
    /// 一直持有全部字节。有了写句柄，下载腿才能边收边写、边写边算摘要。
    ///
    /// # 为什么返回 `Box<dyn std::io::Write + Send>` 而不是自造句柄 trait
    ///
    /// `std::io::Write` 就是 stdlib 给写句柄定的契约（`write_all` / `flush` 齐备），
    /// 再定义一个同形的本地 trait 只会多一层适配（简约阶梯：stdlib 优先）。
    /// `Send` 是硬要求 —— 句柄要被移进下载 task。
    ///
    /// # Errors
    ///
    /// 透传底层 IO 错误（父目录不存在 / 无权限 / 磁盘满）。
    fn open_write(&self, path: &Path) -> Result<Box<dyn std::io::Write + Send>, std::io::Error>;

    /// 把 `path` 已写出的内容**刷到存储介质**（`fsync`），返回后内容才算真的落了盘。
    ///
    /// # 为什么落位路径需要它
    ///
    /// 「写完 → rename」中间没有 `sync` 时，断电 / 内核崩溃后完全可能出现「dest 这个名字在、
    /// 内容却是零或半截」：rename 只保证目录项的原子替换，不保证被替换的那个 inode 的**数据**
    /// 已经离开 page cache。而 dest 一旦存在就会被复用判定与安装腿当成完整包
    /// （`commands/updater.rs` 的 `cached_download_is_reusable` / `update_install`）。
    ///
    /// # 实现须持**写句柄**
    ///
    /// Windows 的 `FlushFileBuffers` 要求句柄带写权限，只读句柄会直接失败；
    /// 故 [`StdFs`] 用 `OpenOptions::new().write(true)` 而不是 `File::open`。
    ///
    /// # Errors
    ///
    /// 透传底层 IO 错误（文件不存在 / 无写权限 / 介质错误）。
    fn sync_file(&self, path: &Path) -> Result<(), std::io::Error>;

    /// 读取 `path` 全部字节。= 上游 `fs.readFileSync`。
    fn read(&self, path: &Path) -> Result<Vec<u8>, std::io::Error>;

    /// 递归创建目录（已存在则 no-op）。= 上游 `fs.mkdirSync({ recursive: true })`。
    fn create_dir_all(&self, path: &Path) -> Result<(), std::io::Error>;

    /// 原子 rename：`from` → `to`（同目录 rename 为原子 syscall，覆盖 `to`）。
    /// = 上游 `fs.renameSync(tmp, dest)`（staged-rename 落位的最后一步）。
    ///
    /// 失败时调用方（[`atomic_replace`](crate::verify::atomic_replace)）负责删 `from` 残件。
    fn rename(&self, from: &Path, to: &Path) -> Result<(), std::io::Error>;

    /// 删除文件（不存在则 no-op，对齐 上游 `if (fs.existsSync(p)) fs.unlinkSync(p)`）。= 上游 `fs.unlinkSync`。
    fn remove_file(&self, path: &Path) -> Result<(), std::io::Error>;

    /// 递归删除目录（不存在则 no-op）。= 上游 `fs.rmSync({ recursive: true, force: true })`。
    fn remove_dir_all(&self, path: &Path) -> Result<(), std::io::Error>;

    /// 列出 `dir` 下的**文件**名（跳过子目录）。= 上游 `fs.readdirSync(dir).filter(isFile)`
    /// （`CoreUpdateService.installCoreFromDir:443-445` 的 staged-rename 文件清单来源）。
    fn list_files(&self, dir: &Path) -> Result<Vec<String>, std::io::Error>;

    /// 文件是否存在。= 上游 `fs.existsSync`。
    fn exists(&self, path: &Path) -> bool;

    /// 拼接路径的便利方法（trait object 友好：避免调用方到处 `path.join`）。
    fn join(&self, base: &Path, name: &str) -> PathBuf {
        base.join(name)
    }
}

/// 真实 std::fs 实现的生产 FS（测试也直接可用 tempfile 驱动，无需另造 mock）。
///
/// 设计取舍：Polaris 的 FS 操作就是直接调 Node `fs`，没有抽象层。本 crate 把它抽象成 trait 主要为
/// 让 staged 周期可注入「失败 mock」（测错误恢复路径），而非为替换 std::fs。因此生产实现 = std::fs
/// 直接转发，测试用 [`MockFs`] 注入受控失败或 tempfile 隔离。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFs;

impl UpdateFs for StdFs {
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
        std::fs::write(path, bytes)
    }

    fn open_write(&self, path: &Path) -> Result<Box<dyn std::io::Write + Send>, std::io::Error> {
        // `File::create` = 建新 / 截断已存在，与 `write` 的覆盖语义一致。
        Ok(Box::new(std::fs::File::create(path)?))
    }

    fn sync_file(&self, path: &Path) -> Result<(), std::io::Error> {
        // **必须带写权限**：Windows 的 `FlushFileBuffers` 对只读句柄直接失败（见 trait 文档）。
        // 不用 `File::create`（会截断）也不用 `append`（无谓地改变文件偏移语义）。
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .sync_all()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(path)
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), std::io::Error> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> Result<(), std::io::Error> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            // 不存在视为成功（对齐 上游 `if exists then unlink` 的 no-op 语义）。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), std::io::Error> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn list_files(&self, dir: &Path) -> Result<Vec<String>, std::io::Error> {
        let mut files = Vec::new();
        for ent in std::fs::read_dir(dir)? {
            let ent = ent?;
            if ent.file_type()?.is_file() {
                if let Some(name) = ent.file_name().to_str() {
                    files.push(name.to_string());
                }
            }
        }
        files.sort();
        Ok(files)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Mock FS（测试用）：包一个真实 [`StdFs`] 但根植于给定 tmp 目录，并支持注入「下一次 op 失败」。
///
/// 用真实 std::fs + tempfile 隔离目录（而非 in-memory map）：这样原子 rename / 权限等 std 行为
/// 与生产一致，mock 只额外提供「受控失败」和「根目录沙箱」两个测试杠杆。
#[derive(Debug)]
pub struct MockFs<'a> {
    /// 沙箱根：所有相对路径解析于此（绝对路径直传 std）。None = 不沙箱（= StdFs）。
    pub sandbox: Option<&'a Path>,
    /// 注入：下一次指定 op 返回此错误（一次性，触发后清空）。用于测错误恢复。
    pub next_fail: Option<MockFailOp>,
}

/// mock FS 可注入失败的 op 种类（测 staged 周期各阶段错误恢复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFailOp {
    Write,
    Rename,
    Remove,
    Read,
    List,
    /// 落位前的 `fsync` 失败（介质错误 / 磁盘满在 flush 时才暴露）。
    ///
    /// **不与 [`Self::Write`] 合流**：`write` 成功而 `sync` 失败正是本注入要覆盖的那一格
    /// —— 合流的话「写都写不进去」会盖掉「写进了 page cache 但没落到介质」这条腿。
    SyncFile,
}

impl<'a> MockFs<'a> {
    #[must_use]
    pub fn new(sandbox: &'a Path) -> Self {
        Self {
            sandbox: Some(sandbox),
            next_fail: None,
        }
    }

    /// 注入「下一次 `op` 永久失败」（命中后不清除，覆盖该 op 所有后续调用）。
    ///
    /// 注：trait 方法是 `&self`，无法 take（一次性）——故采用「永久失败某 op」语义。
    /// 测试如需「一次性失败」请用 `Rc<RefCell<MockFs>>` 自行控制；本 MockFs 覆盖 staged 错误恢复路径足够。
    pub fn fail_next(&mut self, op: MockFailOp) {
        self.next_fail = Some(op);
    }

    /// 解析路径：sandbox 存在且 path 相对时拼到 sandbox 下；绝对路径直传。
    fn resolve(&self, path: &Path) -> std::path::PathBuf {
        match self.sandbox {
            Some(root) if !path.is_absolute() => root.join(path),
            _ => path.to_path_buf(),
        }
    }
}

impl<'a> UpdateFs for MockFs<'a> {
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
        // 注：MockFs::fail_next 需 &mut self，但 trait 方法是 &self。为保持 trait object 友好，
        // 此处用 Cell-like 手法（RefCell）会引入开销；改用内部可变性的标准做法是包 Rc<RefCell<MockFs>>。
        // 本 crate 测试直接用 StdFs + tempfile（见 staged 测试），MockFs 仅作为「可注入失败」的轻量占位，
        // 失败注入经 fail_next 预设、take_fail 在此处读 —— 但 &self 无法 take。
        // 结论：MockFs 的 fail 注入需要内部可变性。用 RefCell 包装 next_fail。
        // —— 为避免在 trait 签名上引入 &mut，这里把 next_fail 改成运行时检查（若 next_fail 命中则报错，
        // 但不清除，因无法 &mut）。测试如需「一次性失败」请用 Rc<RefCell<MockFs>> 自行控制。
        // 简化：本 MockFs 仅做 sandbox 解析 + 可选的「永久失败某 op」（不清除），覆盖 staged 错误恢复测试。
        if matches!(self.next_fail, Some(MockFailOp::Write)) {
            return Err(std::io::Error::other("mock injected failure: Write"));
        }
        std::fs::write(self.resolve(path), bytes)
    }

    fn open_write(&self, path: &Path) -> Result<Box<dyn std::io::Write + Send>, std::io::Error> {
        // 与 `write` 同归 `MockFailOp::Write`：二者是同一件事（往 path 写内容）的两种粒度，
        // 分成两个注入口会让「写失败」的测试覆盖按调用形式分叉。
        if matches!(self.next_fail, Some(MockFailOp::Write)) {
            return Err(std::io::Error::other("mock injected failure: Write"));
        }
        Ok(Box::new(std::fs::File::create(self.resolve(path))?))
    }

    fn sync_file(&self, path: &Path) -> Result<(), std::io::Error> {
        if matches!(self.next_fail, Some(MockFailOp::SyncFile)) {
            return Err(std::io::Error::other("mock injected failure: SyncFile"));
        }
        // 未注入失败时**真的做一次 fsync**（不是无脑 Ok）：这样「文件根本不存在就调 sync」
        // 在 mock 上同样会报错，与 StdFs 的行为一致。
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.resolve(path))?
            .sync_all()
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>, std::io::Error> {
        if matches!(self.next_fail, Some(MockFailOp::Read)) {
            return Err(std::io::Error::other("mock injected failure: Read"));
        }
        std::fs::read(self.resolve(path))
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(self.resolve(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), std::io::Error> {
        if matches!(self.next_fail, Some(MockFailOp::Rename)) {
            return Err(std::io::Error::other("mock injected failure: Rename"));
        }
        std::fs::rename(self.resolve(from), self.resolve(to))
    }

    fn remove_file(&self, path: &Path) -> Result<(), std::io::Error> {
        if matches!(self.next_fail, Some(MockFailOp::Remove)) {
            // 注：staged 周期里 remove 失败多为 best-effort（清残件），不应阻断流程。
            // 但为测错误传播，这里仍报错；调用方 staged 逻辑对残件清理失败已吞掉。
            return Err(std::io::Error::other("mock injected failure: Remove"));
        }
        match std::fs::remove_file(self.resolve(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), std::io::Error> {
        match std::fs::remove_dir_all(self.resolve(path)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn list_files(&self, dir: &Path) -> Result<Vec<String>, std::io::Error> {
        if matches!(self.next_fail, Some(MockFailOp::List)) {
            return Err(std::io::Error::other("mock injected failure: List"));
        }
        let mut files = Vec::new();
        for ent in std::fs::read_dir(self.resolve(dir))? {
            let ent = ent?;
            if ent.file_type()?.is_file() {
                if let Some(name) = ent.file_name().to_str() {
                    files.push(name.to_string());
                }
            }
        }
        files.sort();
        Ok(files)
    }

    fn exists(&self, path: &Path) -> bool {
        self.resolve(path).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_download_hit_and_miss() {
        let dl = MemoryDownload::new().with("https://a/rel", vec![1, 2, 3]);
        assert_eq!(dl.download("https://a/rel").unwrap(), vec![1, 2, 3]);
        let err = dl.download("https://missing").unwrap_err();
        assert!(matches!(err, DownloadError::Other(_)));
    }

    #[test]
    fn unavailable_downloader_reports_backend_unavailable_not_generic_failure() {
        // 反伪造 + §K7.1：这个占位**必须**用专用变体报「没有后端」，而不是泛化的 Other(...)。
        // 若折叠进 Other，上层会把「HTTP 栈根本没接」当成一次可重试的网络失败 → 无限重试永不成功的调用。
        let dl = UnavailableDownloader::new();
        let err = dl.download("https://anywhere").unwrap_err();
        assert!(
            matches!(err, DownloadError::BackendUnavailable(_)),
            "占位下载器必须报 BackendUnavailable（可映射 HTTP_BACKEND_UNAVAILABLE），实得: {err:?}"
        );
        assert_eq!(UnavailableDownloader::CODE, "HTTP_BACKEND_UNAVAILABLE");
    }

    #[test]
    fn backend_unavailable_is_distinguishable_from_retryable_failures() {
        // 钉住语义边界：BackendUnavailable（不可重试，需接线）vs 其余（试过了、可重试）。
        let unavailable = DownloadError::BackendUnavailable("no tls".into());
        let retryable = [
            DownloadError::HttpStatus(403),
            DownloadError::Stalled(30_000),
            DownloadError::Other("mirror exhausted".into()),
        ];
        assert!(matches!(unavailable, DownloadError::BackendUnavailable(_)));
        for e in retryable {
            assert!(
                !matches!(e, DownloadError::BackendUnavailable(_)),
                "可重试失败不得被当成「后端未接线」: {e:?}"
            );
        }
    }

    #[test]
    fn std_fs_roundtrip() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let root = tmpdir.path();
        let p = fs.join(root, "core.bin");
        fs.write(&p, b"hello").unwrap();
        assert!(fs.exists(&p));
        assert_eq!(fs.read(&p).unwrap(), b"hello");
        fs.rename(&p, &fs.join(root, "moved.bin")).unwrap();
        assert!(!fs.exists(&p));
        assert!(fs.exists(&fs.join(root, "moved.bin")));
        fs.remove_file(&fs.join(root, "moved.bin")).unwrap();
        // 二次 remove 不存在 → no-op（对齐 上游 `if exists` 语义）。
        fs.remove_file(&fs.join(root, "moved.bin")).unwrap();
    }

    /// 写句柄与 `write` 的落盘结果必须一致（覆盖语义、分片无关）。
    ///
    /// 这条是流式下载腿的地基：句柄写出来的文件与整包 `write` 出来的不是同一个东西，
    /// 后面的 sha256 校验就会在真机大包上莫名其妙地不过。
    #[test]
    fn open_write_handle_matches_whole_buffer_write() {
        use std::io::Write as _;

        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let streamed = fs.join(tmpdir.path(), "streamed.bin");
        let whole = fs.join(tmpdir.path(), "whole.bin");

        // 先放一份旧内容：句柄必须**截断**，不是追加。
        fs.write(&streamed, b"XXXXXXXXXXXXXXXXXXXXXXXX").unwrap();
        {
            let mut h = fs.open_write(&streamed).unwrap();
            h.write_all(b"chunk-1").unwrap();
            h.write_all(b"chunk-2").unwrap();
            h.flush().unwrap();
        }
        fs.write(&whole, b"chunk-1chunk-2").unwrap();
        assert_eq!(fs.read(&streamed).unwrap(), fs.read(&whole).unwrap());
    }

    /// MockFs 的 `Write` 注入必须同时管住 `write` 与 `open_write`。
    ///
    /// 只管一个的话，「落盘失败」的清理路径在流式腿上就没有测试杠杆可用。
    #[test]
    fn mock_fs_write_injection_also_covers_open_write() {
        let tmpdir = tempfile::tempdir().unwrap();
        let mut fs = MockFs::new(tmpdir.path());
        fs.fail_next(MockFailOp::Write);
        // `Box<dyn Write + Send>` 无 Debug ⇒ 不能用 `unwrap_err()`。
        let Err(err) = fs.open_write(Path::new("blocked.bin")) else {
            panic!("注入 Write 失败后 open_write 必须报错");
        };
        assert!(err.to_string().contains("Write"));
        assert!(
            !tmpdir.path().join("blocked.bin").exists(),
            "注入失败时不得留下空文件"
        );
    }

    #[test]
    fn std_fs_list_files_skips_dirs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let fs = StdFs;
        let root = tmpdir.path();
        fs.write(&fs.join(root, "sing-box"), b"bin").unwrap();
        fs.write(&fs.join(root, "libcronet.so"), b"lib").unwrap();
        fs.create_dir_all(&fs.join(root, "subdir")).unwrap();
        let files = fs.list_files(root).unwrap();
        // 排序后 = [libcronet.so, sing-box]，不含 subdir。
        assert_eq!(
            files,
            vec!["libcronet.so".to_string(), "sing-box".to_string()]
        );
    }

    #[test]
    fn mock_fs_sandbox_resolve_and_fail_injection() {
        // MockFs 沙箱 + 失败注入：覆盖 staged 错误恢复路径所需的 mock FS 能力。
        let tmpdir = tempfile::tempdir().unwrap();
        let root = tmpdir.path();
        let mut fs = MockFs::new(root);
        // 正常写入（沙箱内）。
        fs.write(&fs.join(Path::new(""), "core.bin"), b"data")
            .unwrap();
        assert!(fs.exists(&fs.join(Path::new(""), "core.bin")));
        assert_eq!(
            fs.read(&fs.join(Path::new(""), "core.bin")).unwrap(),
            b"data"
        );

        // 注入 Write 永久失败 → 后续 write 报错。
        fs.fail_next(MockFailOp::Write);
        let err = fs
            .write(&fs.join(Path::new(""), "other.bin"), b"x")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert!(err.to_string().contains("Write"));

        // 注入 Rename 失败。
        fs.fail_next(MockFailOp::Rename);
        let err = fs
            .rename(
                &fs.join(Path::new(""), "core.bin"),
                &fs.join(Path::new(""), "moved.bin"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("Rename"));
    }
}

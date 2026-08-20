//! 跨进程日志文件的硬预算 writer。
//!
//! 单一职责：把任意字节流写入 `file` + `file.1` 两代文件，并保证每代都不超过给定预算。
//! app 日志 sink 与三平台 helper 的 sing-box stdout/stderr 共用本实现，避免各平台复制一份
//! 「何时轮转 / Windows 先关句柄 / 超长单条如何处理」的判据。

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Polaris 管理日志的单代预算；两代文件的总硬上限为其两倍。
pub const DEFAULT_GENERATION_BYTES: u64 = 5 * 1024 * 1024;

/// 打开时是否把现有 current 先滚入 `.1`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// 延续当前代；app 常驻 sink 使用。
    Append,
    /// 新会话从空 current 开始；helper 每次起核使用，保证启动失败诊断不会串到旧会话。
    Fresh,
}

/// 两代有界文件 writer。调用方需要并发写时在外层用 `Mutex` 串行化。
#[derive(Debug)]
pub struct RotatingFile {
    file: File,
    path: PathBuf,
    bytes: u64,
    generation_bytes: u64,
}

impl RotatingFile {
    /// 打开一个有界 writer。
    ///
    /// `Fresh` 会先轮转非空 current；`Append` 会续写。旧版本留下的超限 managed 文件会就地
    /// 保留最近一代预算，避免它继续绕过新上限。任何 IO 错误显式返回给调用方，由日志调用链降级。
    pub fn open(
        path: impl Into<PathBuf>,
        generation_bytes: u64,
        mode: OpenMode,
    ) -> std::io::Result<Self> {
        if generation_bytes == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "log generation budget must be greater than zero",
            ));
        }
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        trim_file_to_tail(&rotated_path(&path), generation_bytes)?;
        trim_file_to_tail(&path, generation_bytes)?;
        if mode == OpenMode::Fresh && std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            rotate(&path)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata().map_or(0, |m| m.len());
        Ok(Self {
            file,
            path,
            bytes,
            generation_bytes,
        })
    }

    /// 写一个完整字节块。超出单代预算的块只保留其末尾（日志 tail 语义）；current 与 `.1`
    /// 在本方法返回时均不超过预算。
    pub fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let max = usize::try_from(self.generation_bytes).unwrap_or(usize::MAX);
        let bounded = if bytes.len() > max {
            &bytes[bytes.len() - max..]
        } else {
            bytes
        };
        let append_len = u64::try_from(bounded.len()).unwrap_or(u64::MAX);
        if self.bytes > 0 && self.bytes.saturating_add(append_len) > self.generation_bytes {
            self.rotate()?;
        }
        self.file.write_all(bounded)?;
        self.bytes = self.bytes.saturating_add(append_len);
        Ok(())
    }

    /// 写一条文本日志并补换行；整条作为一个预算单元，避免正文与换行被拆到两代。
    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let mut record = Vec::with_capacity(line.len().saturating_add(1));
        record.extend_from_slice(line.as_bytes());
        record.push(b'\n');
        self.write_chunk(&record)
    }

    /// 刷新当前文件。
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        // Windows 不能 rename 本进程仍打开的文件：先用一个临时空句柄替换并 drop 旧句柄。
        let replacement = open_sink()?;
        let old = std::mem::replace(&mut self.file, replacement);
        drop(old);
        rotate(&self.path)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.bytes = 0;
        Ok(())
    }
}

/// 把 child 的 stdout/stderr 持续排入同一个有界文件。即使文件打不开，也会起线程把管道读空，
/// 避免 child 因 pipe buffer 填满而卡死。
pub fn spawn_pipe_loggers<O, E>(
    stdout: Option<O>,
    stderr: Option<E>,
    path: impl Into<PathBuf>,
    generation_bytes: u64,
) where
    O: Read + Send + 'static,
    E: Read + Send + 'static,
{
    let writer = RotatingFile::open(path.into(), generation_bytes, OpenMode::Fresh).ok();
    let shared = Arc::new(Mutex::new(writer));
    if let Some(reader) = stdout {
        spawn_pipe_logger(reader, Arc::clone(&shared));
    }
    if let Some(reader) = stderr {
        spawn_pipe_logger(reader, shared);
    }
}

fn spawn_pipe_logger(
    mut reader: impl Read + Send + 'static,
    writer: Arc<Mutex<Option<RotatingFile>>>,
) {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 16 * 1024];
        loop {
            let Ok(read) = reader.read(&mut buf) else {
                return;
            };
            if read == 0 {
                return;
            }
            if let Ok(mut guard) = writer.lock() {
                if let Some(file) = guard.as_mut() {
                    if file.write_chunk(&buf[..read]).is_err() {
                        *guard = None;
                    }
                }
            }
        }
    });
}

/// 读取 current + `.1` 组成的逻辑日志尾部，按旧代→当前代顺序返回，合计不超过 `max_bytes`。
pub fn read_rotated_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }
    let current_len = std::fs::metadata(path).map_or(0, |m| m.len());
    let current_take = current_len.min(max_bytes);
    let old_take = max_bytes.saturating_sub(current_take);
    let mut out = if old_take > 0 {
        read_file_tail(&rotated_path(path), old_take).unwrap_or_default()
    } else {
        Vec::new()
    };
    out.extend(read_file_tail(path, current_take).unwrap_or_default());
    if out.is_empty() && !path.exists() && !rotated_path(path).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "log file not found",
        ));
    }
    Ok(out)
}

/// `.1` 路径（`foo.log` → `foo.log.1`）。
#[must_use]
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".1");
    PathBuf::from(os)
}

fn rotate(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let rotated = rotated_path(path);
    if rotated.exists() {
        std::fs::remove_file(&rotated)?;
    }
    std::fs::rename(path, rotated)
}

fn trim_file_to_tail(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() <= max_bytes {
        return Ok(());
    }
    let tail = read_file_tail(path, max_bytes)?;
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(&tail)?;
    file.flush()
}

fn read_file_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    if max_bytes == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let take = len.min(max_bytes);
    file.seek(SeekFrom::Start(len.saturating_sub(take)))?;
    let cap = usize::try_from(take).unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(cap);
    file.take(take).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(unix)]
fn open_sink() -> std::io::Result<File> {
    OpenOptions::new().write(true).open("/dev/null")
}

#[cfg(windows)]
fn open_sink() -> std::io::Result<File> {
    OpenOptions::new().write(true).open("NUL")
}

#[cfg(not(any(unix, windows)))]
fn open_sink() -> std::io::Result<File> {
    let path = std::env::temp_dir().join("polaris-log-budget-sink");
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "polaris-log-budget-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn runtime_rotation_keeps_two_hard_bounded_generations() {
        let dir = temp_dir("rotate");
        let path = dir.join("core.log");
        let mut file = RotatingFile::open(&path, 8, OpenMode::Append).unwrap();
        file.write_chunk(b"12345678").unwrap();
        file.write_chunk(b"abcdefgh").unwrap();
        file.write_chunk(b"XYZ").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"XYZ");
        assert_eq!(std::fs::read(rotated_path(&path)).unwrap(), b"abcdefgh");
        assert!(std::fs::metadata(&path).unwrap().len() <= 8);
        assert!(std::fs::metadata(rotated_path(&path)).unwrap().len() <= 8);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_record_keeps_tail_without_breaking_budget() {
        let dir = temp_dir("oversized");
        let path = dir.join("core.log");
        let mut file = RotatingFile::open(&path, 5, OpenMode::Append).unwrap();
        file.write_chunk(b"0123456789").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"56789");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fresh_mode_separates_helper_sessions() {
        let dir = temp_dir("fresh");
        let path = dir.join("startup.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"old\n").unwrap();
        let mut file = RotatingFile::open(&path, 32, OpenMode::Fresh).unwrap();
        file.write_chunk(b"new\n").unwrap();
        drop(file);
        assert_eq!(std::fs::read(rotated_path(&path)).unwrap(), b"old\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_tail_spans_old_and_current_in_order() {
        let dir = temp_dir("read");
        let path = dir.join("core.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(rotated_path(&path), b"old-1234").unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert_eq!(read_rotated_tail(&path, 8).unwrap(), b"-1234new");
        let _ = std::fs::remove_dir_all(dir);
    }
}

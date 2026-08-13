//! 行协议 IO 原语（mac/linux helper 共用）。
//!
//! ## 为何合并
//!
//! Polaris helper 用 line-based 文本协议（Go `bufio.Reader.ReadString('\n')`，每行以 `\n` 结尾）。
//! 读方向原本两份：`helper-mac/src/server.rs` 的 `read_line`（`Option<String>`，EOF→None）与
//! `helper-linux/src/handler.rs` 的 `LineConn::read_line`（`String`，EOF→""）。二者都泛型于
//! `std::io::{Read, Write}`、trim 同一字符集（`\r`/`\n`）、EOF 判定同一条件 —— **是假差异**
//! （只是返回值形状不同），故读逻辑上提至此单一真值，各 crate 只留自己的返回形状适配。
//!
//! ## win 为何不用本模块
//!
//! win helper 走裸 Win32 `HANDLE` 的**整帧读**（命名管道，一次 `ReadFile` 取整个请求帧再切行），
//! 不经 `std::io::BufRead` —— 是**真平台差异**，不强行归一。

use std::io::{BufRead, Write};

/// 读一行并 trim 尾部 `\r\n`（移植自 Go `readLine`，`helper/helper.go:92-95`）。
///
/// - `None` = EOF 或读失败（含读超时）—— 调用方据此判「连接关闭/无数据」。
/// - `Some(s)` = 一行内容，已剥尾部 `\r`/`\n`（Go: `strings.TrimRight(s, "\r\n")`）。
///   空行（仅 `\n`）返回 `Some("")`，与 EOF 的 `None` **可区分**（Go 源同样区分：
///   EOF 时 `ReadString` 带 err，空行不带）。
///
/// 需要「EOF 与空行都当空串」的调用方（linux `Conn::read_line` 的契约）用
/// `read_line_trimmed(..).unwrap_or_default()`。
#[must_use]
pub fn read_line_trimmed<R: BufRead>(buf: &mut R) -> Option<String> {
    let mut line = String::new();
    match buf.read_line(&mut line) {
        // Ok(0) = EOF；Err = 读失败/超时。二者对调用方等价（无更多行可读）。
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_owned()),
    }
}

/// 写一行（自动补 `\n`）并 flush —— 对齐 Go `fmt.Fprintln(conn, ...)`。
///
/// flush 对裸 socket（`UnixStream`）是 no-op，对缓冲 writer 是必需 —— 统一 flush 保证
/// 「响应写完即可读」，不依赖调用方传的 writer 是否带缓冲。
pub fn write_line<W: Write>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn reads_lines_and_trims_crlf() {
        let data = b"tok123\r\nping\nlast-no-newline";
        let mut r = BufReader::new(Cursor::new(&data[..]));
        assert_eq!(read_line_trimmed(&mut r).as_deref(), Some("tok123"));
        assert_eq!(read_line_trimmed(&mut r).as_deref(), Some("ping"));
        assert_eq!(
            read_line_trimmed(&mut r).as_deref(),
            Some("last-no-newline"),
            "末行无 \\n 仍应产出内容"
        );
        assert_eq!(read_line_trimmed(&mut r), None, "EOF → None");
    }

    #[test]
    fn empty_line_is_some_empty_not_eof() {
        // 空行 vs EOF 必须可区分 —— linux handler 靠空串判「无参数」，EOF 判「连接断」。
        let mut r = BufReader::new(Cursor::new(&b"\n"[..]));
        assert_eq!(read_line_trimmed(&mut r).as_deref(), Some(""));
        assert_eq!(read_line_trimmed(&mut r), None);
    }

    #[test]
    fn writes_line_with_newline() {
        let mut out: Vec<u8> = Vec::new();
        write_line(&mut out, "OK pong uid=0 v9").unwrap();
        write_line(&mut out, "OK stopped").unwrap();
        assert_eq!(&out[..], b"OK pong uid=0 v9\nOK stopped\n");
    }

    #[test]
    fn write_line_propagates_io_error() {
        /// 恒失败 writer（模拟对端已断）。
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write_line(&mut Broken, "x").is_err());
    }
}

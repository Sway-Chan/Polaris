//! install-core 的 mac wire 适配 —— 公共核心见 [`crate::core_install`]（同 crate 普通模块，三平台共享）。
//!
//! ## 本模块只剩什么
//!
//! 合并成单 crate 后，原「re-export 公共层符号」的转发块已删（同 crate 内 `crate::core_install::*`
//! 直接可达，转发纯属噪音）。本模块只留**真 mac 差异**：[`to_response`] —— 把公共
//! [`InstallResult`](crate::core_install::InstallResult) 适配成 proto wire [`Response`]。
//!
//! ## mac 专属（本模块外）
//!
//! mac 的 xattr 清 quarantine + codesign adhoc 签名在 `handler.rs` 的 `handle_install_core`
//! 文件就位后触发（`helper.go:195-196`），不经本模块。

use crate::core_install::InstallResult;
use polaris_helper_proto::Error as ProtoError;
use polaris_helper_proto::ErrorCode;
use polaris_helper_proto::Response;
use polaris_helper_proto::ResponseKind;

/// 把 [`InstallResult`] 转换成 wire [`Response`]（mac 侧 wire 谱系适配）。
///
/// 用自由函数而非 `From` trait —— [`Response`] 是 proto crate 的类型，orphan rule 禁止
/// `impl From<InstallResult> for Response`。detail 格式与公共
/// [`InstallResult::to_wire_line`](crate::core_install::InstallResult::to_wire_line) 同（`read-singbox <d>` / `readdir <d>` / ...），走
/// `ErrorCode::Other` + detail，handler 据此构造 wire 响应 + 触发 mac 专属签名步骤。
#[must_use]
pub fn to_response(r: InstallResult) -> Response {
    match r {
        InstallResult::Installed => Response::Ok(ResponseKind::Installed),
        InstallResult::CoreDirUnset => Response::Err(ProtoError::new(ErrorCode::CoredirUnset)),
        InstallResult::BadArgs => Response::Err(ProtoError::new(ErrorCode::BadArgs)),
        InstallResult::HashMismatch => Response::Err(ProtoError::new(ErrorCode::HashMismatch)),
        InstallResult::ReadSingbox(d) => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("read-singbox {d}"),
        )),
        InstallResult::ReadDir(d) => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("readdir {d}"),
        )),
        InstallResult::Mkdir(d) => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("mkdir {d}"),
        )),
        InstallResult::Read { name, detail } => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("read {name} {detail}"),
        )),
        InstallResult::Write { name, detail } => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("write {name} {detail}"),
        )),
        InstallResult::Rename { name, detail } => Response::Err(ProtoError::with_detail(
            ErrorCode::Other,
            format!("rename {name} {detail}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_result_to_response_installed() {
        let resp = to_response(InstallResult::Installed);
        assert!(matches!(resp, Response::Ok(ResponseKind::Installed)));
    }

    #[test]
    fn install_result_to_response_error_codes() {
        let resp = to_response(InstallResult::CoreDirUnset);
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::CoredirUnset,
                ..
            })
        ));

        let resp = to_response(InstallResult::BadArgs);
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::BadArgs,
                ..
            })
        ));

        let resp = to_response(InstallResult::HashMismatch);
        assert!(matches!(
            resp,
            Response::Err(ProtoError {
                code: ErrorCode::HashMismatch,
                ..
            })
        ));
    }

    #[test]
    fn install_result_to_response_os_errors_keep_detail() {
        // read-singbox 等走 ErrorCode::Other + detail 保留完整原文
        let resp = to_response(InstallResult::ReadSingbox("open /x: no such file".into()));
        match resp {
            Response::Err(e) => {
                assert_eq!(e.code, ErrorCode::Other);
                assert!(e.detail.contains("read-singbox"), "{}", e.detail);
                assert!(e.detail.contains("no such file"), "{}", e.detail);
            }
            other => panic!("{other:?}"),
        }
    }
}

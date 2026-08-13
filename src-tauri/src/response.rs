//! 上游 `ApiResponse<T>` 信封的 Rust 镜像 + Tauri command 返回约定。
//!
//! Polaris 的 IPC handler 注册器（`main/ipc/ipc-handler.ts`）把每个 handler 的返回值包成
//! `{ success: boolean; data?: T; error?: string; code?: string }` 再回传渲染端；渲染端
//! `IpcClient.invoke` 拆信封、`success=false` 时 throw。
//!
//! Tauri 2 的 `#[tauri::command]` 默认把 `Result<T, E>` 序列化成 `{ status: ok|error, ... }`，
//! 与 Polaris 信封**不兼容**。为保持前端契约不变（B6 前端移植期 ipcClient 可薄改直连），
//! 本层所有 command 统一返回 [`ApiResponse<T>`]，序列化形与 Polaris 逐字段一致。
//!
//! 用法：
//! ```ignore
//! #[tauri::command]
//! async fn config_get(state: State<'_, AppState>) -> ApiResponse<Value> {
//!     ok(state.config().load_full()).or_else_log()
//! }
//! ```
//! 命令体内把任意 `Result<T, E>` 经 [`ApiResponse::ok`] / [`ApiResponse::err`] 转成信封。
//!
//! 注：`err_with_code` / `from_result` 为后续批次（错误码细分 / Result 适配）预留，`dead_code` 放行。

#![forbid(unsafe_code)]
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// 上游 `ApiResponse<T>` 信封（`shared/types/runtime.ts` 1:1 镜像）。
///
/// 序列化字段名与 TS 一致（`success` / `data` / `error` / `code`）。前端在
/// `ui/src/ipc/ipc-client.ts` 的 `invoke` **单点拆信封**（`success:true` → 取 `data`；
/// `success:false` → throw `IpcError` 带 `error`/`code`），api-client 各方法据此标注解包后的类型。
/// `data` 用 `Option`（失败时缺省），与 TS `data?: T` 对齐；`success` 恒序列化（无 `skip_serializing_if`），
/// 前端据此判定「是不是信封」——**新增 command 必须返回本信封，裸返 T 会被前端当作非信封原样透传**。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiResponse<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl<T> ApiResponse<T> {
    /// 成功信封（`{ success: true, data }`）。
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            code: None,
        }
    }

    /// 失败信封（`{ success: false, error, code? }`）。
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
            code: None,
        }
    }

    /// 带 code 的失败信封（对齐 上游 `(error as any)?.code`）。
    pub fn err_with_code(message: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
            code: Some(code.into()),
        }
    }

    /// 把 `Result<T, E>`（E: Display）折成信封——command 最常用入口。
    pub fn from_result<E: std::fmt::Display>(result: Result<T, E>) -> Self {
        match result {
            Ok(v) => Self::ok(v),
            Err(e) => Self::err(format!("{e}")),
        }
    }
}

/// 无数据成功信封（上游 `void` 返回 → `{ success: true }`）。
pub fn ok_void() -> ApiResponse<()> {
    ApiResponse {
        success: true,
        data: None,
        error: None,
        code: None,
    }
}

/// 把任意 `Result<T, E>` 折成成功 / 失败信封（模块级便捷函数）。
pub fn from_result<T, E: std::fmt::Display>(result: Result<T, E>) -> ApiResponse<T> {
    ApiResponse::from_result(result)
}

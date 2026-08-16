//! Taildrop 收件箱命令（sing-box 1.14.0-beta.15 起）。
//!
//! # 这组命令为什么必须存在
//!
//! 核从 beta.15 起在 `Start(StartStateInitialize)` 里**无条件**建收件目录并注册收件 handler
//! （`protocol/tailscale/endpoint.go:253-263`）⇒ 只要 tailnet 授了 `cap/file-sharing`，对端发来的
//! 文件**已经在往盘上落**。没有这组命令，用户拥有的是一个看不见、也清不掉的收件箱。
//!
//! # 为什么是一次性快照而不是常驻订阅
//!
//! 收件箱面板的生命周期以分钟计；而**角标要的三个计数**（未读 / 待处理 / 接收中）本来就随
//! `SubscribeTailscaleStatus` 每帧下发（`TailscaleStatusEvent` 的 `unreadFileCount` 等），
//! 走的是已有的 STATUS relay，不需要新流。故这里只做「打开面板时读一次、操作后再读一次」，
//! 判据见 [`SingBoxApiClient::first_taildrop_inbox_snapshot`]（上游在等待信号前先发一帧）。
//!
//! # 错误一律回稳定 code，不回中文
//!
//! `error` 字段里的串会被直接显示，写中文就等于把文案钉死在 Rust 侧、绕开 i18n。故本模块的失败
//! 一律 [`ApiResponse::err_with_code`]：`error` 放**给日志看的英文诊断**，`code` 放前端查表用的
//! 稳定 token（对照表见 `ui/src/domain/taildrop.ts` 的 `TAILDROP_ERROR_KEY`）。

use polaris_singbox_grpc::{Endpoint, SingBoxApiClient, TaildropDownload};
use serde::Serialize;
use tauri::{Manager, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use crate::response::{ok_void, ApiResponse};
use crate::runtime::AppRuntime;

/// 核没在跑，或该节点不在运行核吃进去的那份配置里（刚加未重启 / 已删）。
const ERR_UNAVAILABLE: &str = "TAILDROP_ENDPOINT_UNAVAILABLE";
/// 连管理 API 失败（核刚起还没 bind / 端口被占）。
const ERR_API: &str = "TAILDROP_API_UNREACHABLE";
/// RPC 本身失败（核拒绝 / 超时 / 文件已不在）。
const ERR_CALL: &str = "TAILDROP_CALL_FAILED";
/// 落盘失败（目标路径不可写 / 空间不足）。
const ERR_WRITE: &str = "TAILDROP_WRITE_FAILED";

/// 收件箱里已落盘、等待处理的一个文件（前端 `contracts/taildrop.ts` 镜像）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaildropFile {
    pub name: String,
    pub size: i64,
    pub sender_name: String,
    /// Unix 秒。前端负责按本地时区与语言格式化 —— Rust 侧不产生任何面向用户的时间文案。
    pub modified_at: i64,
}

/// 正在接收中的一个文件。`sender_id` + `name` 是取消操作的定位键（缺一不可：
/// 两个发件人可以同时发同名文件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaildropReceiving {
    pub name: String,
    pub size: i64,
    pub received_bytes: i64,
    #[serde(rename = "senderID")]
    pub sender_id: String,
    pub sender_name: String,
}

/// 一次收件箱快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaildropInbox {
    pub files: Vec<TaildropFile>,
    pub receiving: Vec<TaildropReceiving>,
}

/// 建一条到运行核管理 API 的连接 + 解出该节点的 endpoint tag。
///
/// 两段失败分别给不同 code：**「拿不到落点」与「连不上」不是一回事** —— 前者是「现在做不了」
/// （核没跑 / 节点没进核），后者是「本该能做但连不上」，用户的下一步动作不同。
async fn connect_for(
    state: &State<'_, AppRuntime>,
    server_id: &str,
) -> Result<(SingBoxApiClient, String), (String, &'static str)> {
    let (port, secret, tag) = state
        .proxy()
        .management_target_for(server_id)
        .ok_or_else(|| {
            (
                format!("no running endpoint for server {server_id}"),
                ERR_UNAVAILABLE,
            )
        })?;
    let client = SingBoxApiClient::connect(Endpoint::new("127.0.0.1", port), secret)
        .await
        .map_err(|e| (format!("management api connect failed: {e}"), ERR_API))?;
    Ok((client, tag))
}

/// 读一次该节点的 Taildrop 收件箱。
///
/// 🔴 **空结果不等于「tag 正确且没有文件」**：核对未知 endpointTag 回的是一帧空收件箱而非错误。
/// 本命令用 [`crate::runtime::proxy::ProxyRuntime::management_target_for`] 解 tag，解不到就直接
/// 报 `TAILDROP_ENDPOINT_UNAVAILABLE` 而**不猜**，正是为了让「空」只剩一种含义。
#[tauri::command]
pub async fn taildrop_list(
    state: State<'_, AppRuntime>,
    server_id: String,
) -> Result<ApiResponse<TaildropInbox>, ()> {
    let (client, tag) = match connect_for(&state, &server_id).await {
        Ok(v) => v,
        Err((msg, code)) => return Ok(ApiResponse::err_with_code(msg, code)),
    };
    match client.first_taildrop_inbox_snapshot(tag).await {
        Ok(inbox) => Ok(ApiResponse::ok(TaildropInbox {
            files: inbox
                .files
                .into_iter()
                .map(|f| TaildropFile {
                    name: f.name,
                    size: f.size,
                    sender_name: f.sender_name,
                    modified_at: f.modified_at,
                })
                .collect(),
            receiving: inbox
                .receiving
                .into_iter()
                .map(|r| TaildropReceiving {
                    name: r.name,
                    size: r.size,
                    received_bytes: r.received_bytes,
                    sender_id: r.sender_id,
                    sender_name: r.sender_name,
                })
                .collect(),
        })),
        Err(e) => Ok(ApiResponse::err_with_code(
            format!("SubscribeTaildropInbox failed: {e}"),
            ERR_CALL,
        )),
    }
}

/// 把收件箱标记为已读（清未读角标）。**不删文件** —— 待处理数不变。
#[tauri::command]
pub async fn taildrop_mark_read(
    state: State<'_, AppRuntime>,
    server_id: String,
) -> Result<ApiResponse<()>, ()> {
    let (client, tag) = match connect_for(&state, &server_id).await {
        Ok(v) => v,
        Err((msg, code)) => return Ok(ApiResponse::err_with_code(msg, code)),
    };
    match client.mark_taildrop_inbox_read(tag).await {
        Ok(()) => Ok(ok_void()),
        Err(e) => Ok(ApiResponse::err_with_code(
            format!("MarkTaildropInboxRead failed: {e}"),
            ERR_CALL,
        )),
    }
}

/// 删除收件箱里的一个文件。
#[tauri::command]
pub async fn taildrop_delete(
    state: State<'_, AppRuntime>,
    server_id: String,
    name: String,
) -> Result<ApiResponse<()>, ()> {
    let (client, tag) = match connect_for(&state, &server_id).await {
        Ok(v) => v,
        Err((msg, code)) => return Ok(ApiResponse::err_with_code(msg, code)),
    };
    match client.delete_taildrop_file(tag, name).await {
        Ok(()) => Ok(ok_void()),
        Err(e) => Ok(ApiResponse::err_with_code(
            format!("DeleteTaildropFile failed: {e}"),
            ERR_CALL,
        )),
    }
}

/// 取消一个**接收中**的文件。定位键是 `sender_id` + `name` 两个一起。
#[tauri::command]
pub async fn taildrop_cancel(
    state: State<'_, AppRuntime>,
    server_id: String,
    sender_id: String,
    name: String,
) -> Result<ApiResponse<()>, ()> {
    let (client, tag) = match connect_for(&state, &server_id).await {
        Ok(v) => v,
        Err((msg, code)) => return Ok(ApiResponse::err_with_code(msg, code)),
    };
    match client.cancel_taildrop_receiving(tag, sender_id, name).await {
        Ok(()) => Ok(ok_void()),
        Err(e) => Ok(ApiResponse::err_with_code(
            format!("CancelTaildropReceiving failed: {e}"),
            ERR_CALL,
        )),
    }
}

/// 取件：把收件箱里的一个文件写到用户选定的路径。
///
/// # 为什么先写 `.part` 再改名
///
/// 下载流**不重连**（见 [`SingBoxApiClient::download_taildrop_file`]）：中途断开会让已写出的字节
/// 成为半截文件。直接写目标路径的话，用户在文件管理器里看到的是一个大小对不上、却完全像回事的
/// 文件；写 `.part` + 成功才改名，则失败路径上目标位置**从来没有出现过**这个名字。
/// 临时文件与目标**同目录**（同卷），改名才是原子的；失败路径必删。
async fn write_stream_to(
    mut stream: TaildropDownload,
    dest: &std::path::Path,
) -> std::io::Result<u64> {
    use std::io::Write;

    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let mut file = std::fs::File::create(&part)?;
    let mut written = 0u64;
    let outcome = async {
        // 首帧只带 size（总字节、data 空）—— 把它当数据块写进去会在文件头多出内容。
        while let Some(chunk) = stream
            .message()
            .await
            .map_err(|e| std::io::Error::other(format!("DownloadTaildropFile stream: {e}")))?
        {
            if chunk.data.is_empty() {
                continue;
            }
            file.write_all(&chunk.data)?;
            written += chunk.data.len() as u64;
        }
        file.flush()?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    drop(file);
    match outcome {
        Ok(()) => {
            std::fs::rename(&part, dest)?;
            Ok(written)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// 取件结果。`canceled` = 用户在原生保存框里按了取消（**不是错误**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaildropSaveResult {
    pub canceled: bool,
    /// 实际写入的目标路径（取消时缺省）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 实际写出的字节数（取消时缺省）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// 取件：选一个保存位置，把收件箱里的该文件写过去。
///
/// **保存框开在 Rust 侧**，与 `local_import_pick_file` 同一范式 —— 前端因此不需要
/// `@tauri-apps/plugin-dialog` 这个 JS 依赖（本仓 UI 至今没有它，为一个按钮引进来不划算），
/// 框的标题也就自然走 Rust 侧 i18n 表（`native.taildropSaveTitle`，五语齐备由
/// `rust-i18n-coverage.test.ts` 守）。
///
/// 默认文件名取 `name`（收件箱里的原名），用户可改。
#[tauri::command]
pub async fn taildrop_save(
    state: State<'_, AppRuntime>,
    window: WebviewWindow,
    server_id: String,
    name: String,
) -> Result<ApiResponse<TaildropSaveResult>, ()> {
    let (client, tag) = match connect_for(&state, &server_id).await {
        Ok(v) => v,
        Err((msg, code)) => return Ok(ApiResponse::err_with_code(msg, code)),
    };

    // 先问路径再开流：反过来的话，用户在保存框里犹豫的这几十秒里流一直挂着，
    // 而取消之后那条流还得额外收一次尾。
    let lang = crate::i18n::app_lang(window.app_handle());
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title(crate::i18n::t(
            lang,
            crate::i18n::key::NATIVE_TAILDROP_SAVE_TITLE,
        ))
        .set_file_name(&name)
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(dest) = rx.await.ok().flatten().and_then(|p| p.into_path().ok()) else {
        return Ok(ApiResponse::ok(TaildropSaveResult {
            canceled: true,
            ..Default::default()
        }));
    };

    let stream = match client.download_taildrop_file(tag, &name).await {
        Ok(s) => s,
        Err(e) => {
            return Ok(ApiResponse::err_with_code(
                format!("DownloadTaildropFile failed: {e}"),
                ERR_CALL,
            ))
        }
    };
    match write_stream_to(stream, &dest).await {
        Ok(n) => Ok(ApiResponse::ok(TaildropSaveResult {
            canceled: false,
            path: Some(dest.to_string_lossy().into_owned()),
            bytes: Some(n),
        })),
        Err(e) => Ok(ApiResponse::err_with_code(
            format!("write {} failed: {e}", dest.display()),
            ERR_WRITE,
        )),
    }
}

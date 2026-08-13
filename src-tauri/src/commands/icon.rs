//! 自定义应用图标缓存 command（配套 [`crate::icon_cache`]）。
//!
//! `cache_app_icon`：设定即缓存 —— 下载一次 `remote_url` 到 `<userData>/icons/`，返回本地缓存 ref
//! （`polaris-icon://c/<file>`）供前端写进 `preset.iconUrl`。失败返错误信封，前端回落存 remote URL
//! （旧行为）。**只在这一「设定时刻」联网**，正常渲染永不触网。

#![allow(clippy::needless_pass_by_value)]

use tauri::State;

use crate::icon_cache;
use crate::response::ApiResponse;
use crate::runtime::http::SystemDnsLookup;
use crate::runtime::AppRuntime;

/// 下载并缓存自定义应用图标，返回本地缓存 ref。
///
/// 前端 `api.icon.cacheAppIcon(appId, remoteUrl)` → 成功取 `data`（本地 ref）；失败 throw → 回落 remote URL。
#[tauri::command]
pub async fn cache_app_icon(
    state: State<'_, AppRuntime>,
    app_id: String,
    remote_url: String,
) -> Result<ApiResponse<String>, ()> {
    if app_id.trim().is_empty() {
        return Ok(ApiResponse::err("应用 ID 为空"));
    }
    if !icon_cache::is_http_url(&remote_url) {
        return Ok(ApiResponse::err("图标 URL 必须是 http/https"));
    }
    // await 前取出 owned 值（路径 + Arc<HttpRuntime>），下载不持 State 借用。
    let dir = icon_cache::icons_dir(state.config().dir());
    let http = state.http().clone();
    let result = match icon_cache::fetch_image(
        http.as_ref(),
        &SystemDnsLookup,
        &remote_url,
        icon_cache::MAX_ICON_BYTES,
    )
    .await
    {
        Ok((ext, bytes)) => icon_cache::write_icon(&dir, &app_id, ext, &bytes),
        Err(e) => Err(e),
    };
    Ok(ApiResponse::from_result(result))
}

/**
 * 图标代理协议（Phase 1b §8）：renderer 的外部图标 <img> 不直接走 default session（删 default session
 * pin 后 manual 接管模式会破图），改用自定义 protocol polaris-icon:// 由 main 经 update-in 统一会话拉取，
 * 全模式经核。本模块零运行时依赖，scheme 名 + URL 构造在 main/renderer 间单一真值共享。
 */
export const ICON_PROXY_SCHEME = 'polaris-icon';

/**
 * 把外部图标 URL 包成 polaris-icon:// 代理 URL（renderer 端 <img src>）。空 / 非 http(s) URL 原样返回
 * （emoji 占位、data:、本地资源不代理）。远端 URL：host 段固定 'i'，真实 URL 经 encodeURIComponent 放
 * pathname，由 Rust scheme handler decode 后经传输层单点拉取。
 *
 * 本地缓存 ref（`polaris-icon://c/<file>`，自定义应用设定图标后 preset.iconUrl 持有的形态）已是本
 * scheme 的可服务 URL —— 直接原样返回，路由到缓存服务（读 <userData>/icons/ 本地副本，渲染零出站），
 * 绝不二次包裹成 `polaris-icon://i/polaris-icon%3A%2F%2F...`。签名不变（并发批的 AppIcon 直接复用）。
 */
export function iconProxySrc(url: string | undefined | null): string {
  if (!url) return '';
  // 已是本 scheme 的可服务 ref（本地缓存等）：直接返回，勿再包裹。
  if (url.startsWith(`${ICON_PROXY_SCHEME}://`)) return url;
  if (!/^https?:\/\//i.test(url)) return url; // 非网络 URL（data:/本地）不代理
  return `${ICON_PROXY_SCHEME}://i/${encodeURIComponent(url)}`;
}

/**
 * 浏览器内置 DoH 端点的内置起点清单 —— **Rust 侧是真值**，这里是它的前端镜像。
 *
 * 真值在 `crates/config-engine/src/builder/route.rs` 的 `DEFAULT_BROWSER_DOH_SUFFIXES`：
 * 生成配置的是 Rust，前端这份只用来在「用户还没编辑过清单」时把内容显示出来。
 * 两侧必须逐项同序相等，由 `browser-doh-parity.test.ts` 从 Rust 源码解析后对差钉住 ——
 * 不设这道锁的话，前端显示的和实际下发的会悄悄分家，而用户看到的是前端那份。
 *
 * `domain_suffix` 语义：`cloudflare-dns.com` 覆盖 `mozilla.` / `chrome.` / `security.` /
 * `family.` 等子域，故不逐个列。
 *
 * **它必然不全**，这是设计不是缺陷：DoH 端点可以是任意自建域名甚至纯 IP，黑名单原理上穷尽不了。
 * 清单可编辑 + 可批量导入才是兜底。
 */
export const DEFAULT_BROWSER_DOH_SUFFIXES: readonly string[] = [
  'dns.google',
  'cloudflare-dns.com',
  'one.one.one.one',
  'dns.quad9.net',
  'dns9.quad9.net',
  'dns10.quad9.net',
  'dns11.quad9.net',
  'doh.opendns.com',
  'doh.familyshield.opendns.com',
  'dns.nextdns.io',
  'adguard-dns.com',
  'dns.adguard.com',
  'doh.cleanbrowsing.org',
  'dns.controld.com',
  'freedns.controld.com',
  'dns.mullvad.net',
  'doh.mullvad.net',
  'doh.sb',
  'doh.dns.sb',
  'dns.comss.one',
  'router.comss.one',
  'wikimedia-dns.org',
  'dns.digitale-gesellschaft.ch',
  'doh.libredns.gr',
];

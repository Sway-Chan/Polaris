/**
 * capture-screenshots.mjs — 为 README 生成界面截图。
 *
 * # 为什么不截真 app
 *
 * 真 app 要先出安装包、装上、起核，而起核会碰系统代理/DNS —— 截图不值得付那个代价。
 * 前端是纯 React，`ipc-client.ts` 在非 Tauri 环境本就有 mock 回落，故直接用无头 Chrome 渲染
 * 构建产物即可。代价是数据全空，所以这里注入一份**桩数据**让界面呈现真实使用状态。
 *
 * # 桩注入点
 *
 * `isTauri()` 的判据是 `'__TAURI_INTERNALS__' in window`。页面脚本执行前塞一个同名对象，
 * 其 `invoke` 按命令名查表返回。表只覆盖「让界面有内容」所需的少数命令，其余返回 undefined
 * （与真实 mock 回落同语义，不会崩）。
 *
 * # 用法
 *
 *   node scripts/capture-screenshots.mjs            # 全部
 *   node scripts/capture-screenshots.mjs home nodes # 指定
 *
 * 产物落 `docs/screenshots/<name>.png`。需要先 `cd ui && npx vite build`。
 */
import { createServer } from 'node:http';
import { readFile, mkdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { extname, join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'ui/dist');
const OUT = join(ROOT, 'docs/screenshots');
const PORT = 4731;

// playwright 是 `ui/` 的 devDep，本脚本在仓根跑 ⇒ ESM 解析不到。从 ui 的 node_modules 显式解析，
// 而不是把脚本挪进 ui/（它产出的是仓库文档资产，不属于前端构建）。
const require_ = createRequire(join(ROOT, 'ui/package.json'));
const { chromium } = require_('@playwright/test');

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json',
  '.woff2': 'font/woff2',
};

/** 桩数据：只覆盖「让界面有内容」所需的命令。 */
const STUB = {
  config_get: {
    servers: [
      { id: 's1', name: '香港 · 01', protocol: 'vless', address: 'hk01.example.com', port: 443, uuid: '6ba7b810-9dad-11d1-80b4-00c04fd430c8', security: 'tls' },
      { id: 's2', name: '日本 · 东京', protocol: 'trojan', address: 'jp-tyo.example.com', port: 443, password: 'x' },
      { id: 's3', name: '新加坡 · 03', protocol: 'hysteria2', address: 'sg03.example.com', port: 443, password: 'x' },
      { id: 's4', name: '美国 · 洛杉矶', protocol: 'vless', address: 'us-lax.example.com', port: 443, uuid: '6ba7b810-9dad-11d1-80b4-00c04fd430c8', security: 'reality' },
    ],
    selectedServerId: 's1',
    proxyMode: 'smart',
    proxyModeType: 'tun',
    tunConfig: { mtu: 9000, stack: 'auto', autoRoute: true, strictRoute: false },
    customRules: [],
    appRules: [],
    autoStart: false,
    autoConnect: false,
    mixedPort: 7890,
    dnsConfig: { enableFakeIp: true },
    language: 'zh-CN',
    theme: 'dark',
  },
  proxy_get_status: { running: true, mode: 'smart', uptimeMs: 4_512_000 },
  config_get_privacy_mode: false,
  system_proxy_get_status: { enabled: false },
  app_get_version: '1.0.0',
};

// nav 用**可见文案**定位（侧栏是 `<button class="nav-item"><span>{译文}</span></button>`，
// 没有 data-screen 之类的测试锚点）。文案取 zh-CN 的 `sidebar.*`。
const SHOTS = [
  { name: 'home', nav: null },
  { name: 'nodes', nav: '节点' },
  { name: 'rules', nav: '规则' },
  { name: 'connections', nav: '连接' },
  { name: 'settings', nav: '设置' },
];

function serve() {
  return new Promise((ok) => {
    const s = createServer(async (req, res) => {
      const url = (req.url || '/').split('?')[0];
      let file = join(DIST, url === '/' ? 'index.html' : url);
      if (!existsSync(file)) file = join(DIST, 'index.html'); // SPA 回落
      try {
        const buf = await readFile(file);
        res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' });
        res.end(buf);
      } catch {
        res.writeHead(404).end();
      }
    });
    s.listen(PORT, () => ok(s));
  });
}

const initScript = (stub) => `
  window.__TAURI_INTERNALS__ = {
    invoke: (cmd) => Promise.resolve(
      Object.prototype.hasOwnProperty.call(${JSON.stringify(stub)}, cmd)
        ? { success: true, data: ${JSON.stringify(stub)}[cmd] }
        : { success: true, data: undefined }
    ),
    transformCallback: (cb) => { const id = Math.random(); window[id] = cb; return id; },
  };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => Promise.resolve() };
`;

async function main() {
  if (!existsSync(DIST)) {
    console.error(`缺 ${DIST} —— 先跑 \`cd ui && npx vite build\``);
    process.exit(2);
  }
  const want = process.argv.slice(2);
  const shots = want.length ? SHOTS.filter((s) => want.includes(s.name)) : SHOTS;
  await mkdir(OUT, { recursive: true });
  const server = await serve();
  const browser = await chromium.launch({ executablePath: '/usr/bin/google-chrome' });
  const page = await browser.newPage({ viewport: { width: 1120, height: 680 }, deviceScaleFactor: 2 });
  await page.addInitScript(initScript(STUB));

  let failed = 0;
  // 「截到了但都是同一屏」比空白更隐蔽：文件非空、尺寸正常，肉眼不看就发现不了。
  // 按内容哈希去重，撞了即红。
  const seen = new Map();
  for (const shot of shots) {
    await page.goto(`http://127.0.0.1:${PORT}/`, { waitUntil: 'networkidle' });
    if (shot.nav) {
      // 点真实导航项而不是改 store：store 没有全局出口，且点击这条路顺带证明壳是活的。
      const item = page.locator(`nav[aria-label="Primary"] button.nav-item`, { hasText: shot.nav }).first();
      if (!(await item.count())) {
        console.error(`  ✗ 找不到导航项「${shot.nav}」—— 截出来会是上一屏，等于假截图`);
        failed++;
        continue;
      }
      await item.click();
    }
    await page.waitForTimeout(900);
    const file = join(OUT, `${shot.name}.png`);
    await page.screenshot({ path: file });
    // 空白页自曝：全白/全黑的 PNG 压缩后极小，比「截到了但没内容」更该当场发现。
    const { size } = await import('node:fs').then((m) => m.promises.stat(file));
    if (size < 12_000) {
      console.error(`  ✗ ${shot.name}.png 只有 ${size}B —— 多半是空白页`);
      failed++;
    } else {
      const { createHash } = await import('node:crypto');
      const hash = createHash('sha256').update(await readFile(file)).digest('hex').slice(0, 12);
      if (seen.has(hash)) {
        console.error(`  ✗ ${shot.name}.png 与 ${seen.get(hash)}.png 逐字节相同 —— 导航没生效，截的是同一屏`);
        failed++;
      } else {
        seen.set(hash, shot.name);
        console.log(`  ✓ ${shot.name}.png  ${(size / 1024).toFixed(0)}KB`);
      }
    }
  }

  await browser.close();
  server.close();
  if (failed) process.exit(1);
}

main();

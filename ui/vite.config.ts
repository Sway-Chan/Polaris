// Vite 配置（Polaris / Tauri 2 集成）。
//
// Tauri 官方推荐：
//  - clearScreen: false —— 保留 cargo/rust 编译输出，dev 时 rust 报错不被 vite 清屏盖掉。
//  - server.port 对齐 tauri.conf.json 的 devUrl（:5173）。
//  - strictPort —— 端口占用直接报错（而非递增），否则 Tauri 连不上 webview 会静默黑屏。
//  - HMR 端口与 dev server 分离；忽略 Rust 侧文件变更（改 Rust 不触发前端热更，无意义）。
//  - envPrefix: VITE_ / TAURI_ —— 暴露 TAURI_ENV_* 给前端读取（平台/架构）。
//  - resolve.alias '@' → src（与 tsconfig paths 对齐）。
//
// Tauri 2 提供 @tauri-apps/cli 的 vite 插件（host 配置等），但底座阶段用纯 vite 配置 + 手写对齐
// 已足够；cli 主要提供 tauri dev/build 命令编排（beforeDevCommand 等，已在 tauri.conf.json 配好）。
/// <reference types="vitest/config" />
import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// Tauri dev 环境变量（TAURI_ENV_PLATFORM 等），供条件编译用
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,

  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },

  // 仅暴露 VITE_ 与 TAURI_ 前缀的环境变量给前端
  envPrefix: ['VITE_', 'TAURI_'],

  server: {
    port: 5173,
    strictPort: true,
    // Tauri 移动端 dev（via @tauri-apps/cli mobile）走网络 host
    host: host || false,
    hmr: host
      ? {
          protocol: 'ws',
          host,
          port: 5174,
        }
      : undefined,
    watch: {
      // 忽略 Rust 侧变更（改 src-tauri 不该触发前端 HMR）
      ignored: ['**/src-tauri/**'],
    },
  },

  build: {
    // Tauri webview 用相对路径加载本地文件；不指定 base 时默认相对，满足 frontendDist(../ui/dist)。
    target: ['es2021', 'chrome100', 'safari13'],
    // 生产构建产物最小化（Tauri 打包体积敏感）
    minify: !process.env.TAURI_ENV_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,

    rollupOptions: {
      // 多入口：主窗（index）+ mini 更新弹窗（update-popup）+ 托盘自绘浮层（tray）。
      //
      // 弹窗/浮层**刻意不复用 index.html**：复用主入口会把整个 React 应用（i18n / 路由 / 全部 provider）
      // 塞进小窗——既拖慢首帧，又会让主窗白屏自愈门（`window_health.rs` 只认 label=="main"）对着它们误判。
      // update-popup 是零框架 vanilla TS；tray 浮层需真实接线（连接态/节点/api）故用精简 React 入口。
      input: {
        index: path.resolve(__dirname, 'index.html'),
        'update-popup': path.resolve(__dirname, 'update-popup.html'),
        tray: path.resolve(__dirname, 'tray.html'),
      },
    },
  },

  // Vitest —— 仅跑纯逻辑单测（Csel 定位/键盘协议、URL 推断/字段校验），无 DOM 依赖，
  // 走默认 node 环境（不引 jsdom/happy-dom，零额外运行时；与已装 vite 同源）。
  test: {
    environment: 'node',
    include: ['src/**/*.{test,spec}.{ts,tsx}'],
  },
});

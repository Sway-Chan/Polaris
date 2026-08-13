/**
 * Tailwind 配置（Polaris 设计系统 · 接入原型令牌）。
 *
 * 设计真值源：~/docs/polaris/design/prototype/polaris-prototype.html 的 :root 块，
 * 逐个映射为 CSS 变量（见 src/styles/tokens.css），Tailwind 经
 * colors.* = 'hsl(var(--token) / <alpha-value>)' 引用 —— 透明度工具类（bg-flow/50）可用。
 *
 * 主题切换约定：darkMode:'class' + <html data-theme="dark|light">（tokens.css 用
 * [data-theme] 覆盖变量）。App 阶段据 config.uiTheme 写 data-theme。
 */

/** @type {import('tailwindcss').Config} */
export default {
  darkMode: 'class',
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: {
    extend: {
      colors: {
        // ── 设计令牌（引用 CSS 变量，alpha 透明度可用）──
        bg: 'hsl(var(--bg) / <alpha-value>)',
        surface: {
          DEFAULT: 'hsl(var(--surface) / <alpha-value>)',
          2: 'hsl(var(--surface-2) / <alpha-value>)',
          3: 'hsl(var(--surface-3) / <alpha-value>)',
        },
        line: 'hsl(var(--line) / <alpha-value>)',
        hair: 'hsl(var(--hair) / <alpha-value>)',
        fg: {
          DEFAULT: 'hsl(var(--fg) / <alpha-value>)',
          dim: 'hsl(var(--fg-dim) / <alpha-value>)',
          faint: 'hsl(var(--fg-faint) / <alpha-value>)',
        },
        flow: {
          DEFAULT: 'hsl(var(--flow) / <alpha-value>)',
          hi: 'hsl(var(--flow-hi) / <alpha-value>)',
          weak: 'hsl(var(--flow-weak) / <alpha-value>)',
        },
        ring: 'hsl(var(--ring) / <alpha-value>)',
        aurora: {
          DEFAULT: 'hsl(var(--aurora) / <alpha-value>)',
          hi: 'hsl(var(--aurora-hi) / <alpha-value>)',
          weak: 'hsl(var(--aurora-weak) / <alpha-value>)',
        },
        ok: {
          DEFAULT: 'hsl(var(--ok) / <alpha-value>)',
          weak: 'hsl(var(--ok-weak) / <alpha-value>)',
        },
        warn: {
          DEFAULT: 'hsl(var(--warn) / <alpha-value>)',
          weak: 'hsl(var(--warn-weak) / <alpha-value>)',
        },
        err: {
          DEFAULT: 'hsl(var(--err) / <alpha-value>)',
          weak: 'hsl(var(--err-weak) / <alpha-value>)',
        },
        dn: 'hsl(var(--dn) / <alpha-value>)',
        up: 'hsl(var(--up) / <alpha-value>)',
        indigo: 'hsl(var(--indigo) / <alpha-value>)',
        'logo-tile': 'hsl(var(--logo-tile) / <alpha-value>)',
        'logo-tile-bd': 'hsl(var(--logo-tile-bd) / <alpha-value>)',
      },
      borderRadius: {
        xs: 'var(--r-xs)',
        sm: 'var(--r-sm)',
        md: 'var(--r-md)',
        DEFAULT: 'var(--r)',
        lg: 'var(--r-lg)',
      },
      spacing: {
        1: 'var(--sp-1)',
        2: 'var(--sp-2)',
        3: 'var(--sp-3)',
        4: 'var(--sp-4)',
        5: 'var(--sp-5)',
        6: 'var(--sp-6)',
        7: 'var(--sp-7)',
      },
      boxShadow: {
        pop: 'var(--shadow-pop)',
      },
      fontFamily: {
        sans: [
          '-apple-system',
          '"Segoe UI Variable"',
          '"Segoe UI"',
          '"PingFang SC"',
          '"Hiragino Sans GB"',
          '"Microsoft YaHei"',
          'system-ui',
          'sans-serif',
        ],
        display: [
          '"Space Grotesk"',
          '"Avenir Next"',
          '"SF Pro Display"',
          '"Segoe UI"',
          '-apple-system',
          '"Segoe UI Variable"',
          '"Segoe UI"',
          '"PingFang SC"',
          '"Microsoft YaHei"',
          'system-ui',
          'sans-serif',
        ],
        mono: [
          'ui-monospace',
          '"SF Mono"',
          '"JetBrains Mono"',
          '"Cascadia Code"',
          'Menlo',
          'Consolas',
          'monospace',
        ],
      },
    },
  },
  plugins: [
    // tailwindcss-animate（组件阶段启用，依赖未装时注释避免 require 失败）
    // require('tailwindcss-animate'),
  ],
};

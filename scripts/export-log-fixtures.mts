/**
 * export-log-fixtures.mts — 导出 buildLogConfig 金样对拍 fixture（B1 对拍脚手架）。
 *
 * 内联 上游 `singbox-log-builder.ts buildLogConfig` + `log-level.ts effectiveLogLevel` 逻辑
 * （避免 import 拖入 electron 依赖 —— getSingBoxLogPath → app.getPath）。
 * 逻辑与 上游 源逐行对齐；Rust 侧 build_log_config 对拍此输出。
 *
 * 覆盖矩阵：3 platform × 3 proxyModeType × {privacy on/off} × {disableLogFile on/off} × 5 logLevel = 180 cases。
 *
 * 用法：npx tsx scripts/export-log-fixtures.mjs > crates/config-engine/fixtures/log.json
 */
type LogLevel = 'debug' | 'info' | 'warn' | 'error' | 'fatal';

const ORDER: LogLevel[] = ['debug', 'info', 'warn', 'error', 'fatal'];
const FAKE_LOG_PATH = '/fake/singbox.log';

// effectiveLogLevel —— 上游 shared/log-level.ts 1:1 内联。
function effectiveLogLevel(level: LogLevel, privacy: boolean): LogLevel {
  if (!privacy) return level;
  const cur = ORDER.indexOf(level);
  const warn = ORDER.indexOf('warn');
  return cur < warn ? 'warn' : level;
}

// buildLogConfig —— 上游 singbox-log-builder.ts 1:1 内联（platform 参数化）。
function buildLogConfig(
  logLevel: LogLevel,
  disableLogFile: boolean,
  proxyModeType: 'systemProxy' | 'tun' | 'manual',
  privacyMode: boolean,
  platform: string
): { level: string; timestamp: boolean; output?: string; disabled?: boolean } {
  const cfg: { level: string; timestamp: boolean; output?: string; disabled?: boolean } = {
    level: effectiveLogLevel(logLevel || 'info', privacyMode),
    timestamp: true,
  };
  if (disableLogFile) {
    cfg.disabled = true;
    return cfg;
  }
  const isTunMode = proxyModeType?.toLowerCase() === 'tun';
  const writesLogToFile =
    isTunMode && (platform === 'darwin' || platform === 'win32' || platform === 'linux');
  if (writesLogToFile) {
    cfg.output = FAKE_LOG_PATH;
  }
  return cfg;
}

interface LogCase {
  name: string;
  platform: string;
  input: { logLevel: LogLevel; disableLogFile: boolean; proxyModeType: 'systemProxy' | 'tun' | 'manual' };
  privacyMode: boolean;
  output: { level: string; timestamp: boolean; output?: string; disabled?: boolean };
}

const LOG_LEVELS: LogLevel[] = ['debug', 'info', 'warn', 'error', 'fatal'];
const PLATFORMS = ['darwin', 'win32', 'linux'] as const;
const PROXY_MODE_TYPES = ['systemProxy', 'tun', 'manual'] as const;
const cases: LogCase[] = [];

for (const platform of PLATFORMS) {
  for (const pmt of PROXY_MODE_TYPES) {
    for (const privacy of [false, true]) {
      for (const disable of [false, true]) {
        for (const level of LOG_LEVELS) {
          const input = { logLevel: level, disableLogFile: disable, proxyModeType: pmt };
          const output = buildLogConfig(level, disable, pmt, privacy, platform);
          cases.push({
            name: `${platform}_${pmt}_priv${privacy ? '1' : '0'}_dis${disable ? '1' : '0'}_${level}`,
            platform,
            input,
            privacyMode: privacy,
            output,
          });
        }
      }
    }
  }
}

process.stdout.write(JSON.stringify({ cases }, null, 2) + '\n');

/**
 * export-inbounds-fixtures.mts — 导出 buildInbounds 金样对拍 fixture（B1/H2）。
 *
 * 从**上游参考实现**的 singbox-inbounds-builder 取 buildInbounds，遍历覆盖矩阵输出 JSON。
 * 需 electron mock（getOwnLanCidrs → os.networkInterfaces 非阻塞，但路径相关 import 可能拖入）。
 *
 * 上游仓路径由环境变量 `REF_REPO` 注入（不写死在源码里）：
 *
 *   REF_REPO=<上游仓根> npx tsx --import <本仓>/scripts/electron-mock-hook.mjs \
 *     <本仓>/scripts/export-inbounds-fixtures.mts > <本仓>/crates/config-engine/fixtures/inbounds.json
 *
 * 用动态 import 而非静态：静态 import 的路径必须是字面量，那就只能把某台机器上的绝对路径写死进仓。
 */
const REF_REPO = process.env.REF_REPO;
if (!REF_REPO) {
  console.error('缺 REF_REPO 环境变量：需指向上游参考实现的仓库根目录');
  process.exit(2);
}
const { buildInbounds } = (await import(`${REF_REPO}/src/main/services/singbox-inbounds-builder.ts`)) as {
  buildInbounds: (...args: never[]) => unknown[];
};
type UserConfig = Record<string, unknown>;

interface InboundCase {
  name: string;
  input: { config: UserConfig; platform: string; ports: { probeDirect?: number; probeProxy?: number; updateIn?: number; probePool?: number[] } };
  output: unknown[];
}

function cfg(over: Partial<UserConfig>): UserConfig {
  return {
    servers: [],
    selectedServerId: 's1',
    proxyMode: 'smart',
    proxyModeType: 'systemProxy',
    tunConfig: { mtu: 1350, stack: 'auto', autoRoute: true, strictRoute: true },
    customRules: [],
    appRules: [],
    autoStart: false,
    silentStart: false,
    autoConnect: false,
    minimizeToTray: false,
    mixedPort: 7890,
    logLevel: 'info',
    ...over,
  } as UserConfig;
}

function run(name: string, config: UserConfig, platform: string, ports: InboundCase['input']['ports']): InboundCase {
  Object.defineProperty(process, 'platform', { value: platform, configurable: true });
  const deps = {
    probeDirectPort: ports.probeDirect ?? null,
    probeProxyPort: ports.probeProxy ?? null,
    updateInPort: ports.updateIn ?? null,
    probePoolPorts: ports.probePool ?? [],
  };
  const out = buildInbounds(config, undefined, deps);
  return { name, input: { config, platform, ports }, output: out };
}

const cases: InboundCase[] = [];

// systemProxy 各平台
cases.push(run('sys_linux', cfg({}), 'linux', {}));
cases.push(run('sys_mac', cfg({}), 'darwin', {}));
cases.push(run('sys_win', cfg({}), 'win32', {}));
cases.push(run('sys_allowlan', cfg({ allowLan: true }), 'linux', {}));

// TUN 各平台
cases.push(run('tun_linux', cfg({ proxyModeType: 'tun' }), 'linux', {}));
cases.push(run('tun_mac', cfg({ proxyModeType: 'tun' }), 'darwin', {}));
cases.push(run('tun_win', cfg({ proxyModeType: 'tun', bypassLAN: true }), 'win32', {}));
cases.push(run('tun_ipv6', cfg({ proxyModeType: 'tun', enableIPv6: true }), 'linux', {}));
cases.push(run('tun_manual_addr', cfg({ proxyModeType: 'tun', tunConfig: { mtu: 1400, stack: 'system', autoRoute: true, strictRoute: false, inet4Address: '10.0.0.1/24' } }), 'linux', {}));

// 探针
cases.push(run('probe_ports', cfg({}), 'linux', { probeDirect: 12345, probeProxy: 12346 }));
cases.push(run('update_in', cfg({}), 'linux', { updateIn: 12347 }));
cases.push(run('probe_pool', cfg({ proxyModeType: 'tun' }), 'linux', { probePool: [20000, 20001] }));

// manual 模式（无 TUN）
cases.push(run('manual', cfg({ proxyModeType: 'manual' }), 'linux', {}));

process.stdout.write(JSON.stringify({ cases }, null, 2) + '\n');

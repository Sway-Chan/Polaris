import type { UserConfig } from '@/contracts/types';

/**
 * 消费「FakeIP-TUN 待纠正」快照（纯函数，不可变）：仅当目标模式为 tun 且 dnsConfig.fakeIpTunAutoEnable===true
 * 时，把 systemProxy 迁移冻结的 enableFakeIp 回 true 并消费 flag（置 false，一次性）；其余一律不动。
 *
 * 移植自 上游 `src/shared/fakeip-tun-entry.ts`。快照三态语义见 contracts/types.ts
 * `DnsConfig.fakeIpTunAutoEnable`；flag 由 Rust 迁移期一次性写入，见 crates/store/src/migrate.rs
 * `migrate_fake_ip_tun_pending`（systemProxy + enableFakeIp===false + migrated → true）。
 *
 * 触发只看**目标模式为 tun**（不限来源模式）：systemProxy→manual→tun 的绕行仍应纠正（manual 下 enableFakeIp:false
 * 无害、flag 存续到进入 tun 才消费）。**用户手改 Settings→DNS 的 FakeIP 开关会同写 false**
 * （`SettingsDns.tsx` 的 `fakeip-swt` onChange，对齐契约 L95「手改写 fakeIpTunAutoEnable:false」）——
 * 意图即撤销，故不误伤「升级后在 TUN 下主动关 FakeIP」的用户。（升级前的 systemProxy 手动关历史无字节
 * 留痕、不可区分，属快照方案固有边界：可能误植后首次进 TUN 自动开回一次，toast 告知 + 再关即永久 false。
 * 已知窄口径缺口：`SettingsTun.tsx`「一键开 FakeIP」按钮只强制写 `enableFakeIp:true`、不写此 flag——
 * 该按钮只会把 FakeIP 打开而非「主动关」，不触发本文件要防的误纠正场景，暂不影响本 invariant；但若
 * 该按钮未来支持双向操作，须一并补写 `fakeIpTunAutoEnable:false`。）
 *
 * @returns config——corrected 时是**新对象**（zustand store 禁原地改，dnsConfig 浅拷贝）；否则原样返回。
 *          corrected——是否真把 enableFakeIp 从 false 改成了 true（供调用方发提示；flag 已开着只消费时为 false）。
 */
export function applyFakeIpTunEntry(config: UserConfig): {
  config: UserConfig;
  corrected: boolean;
} {
  const modeType = (config.proxyModeType || 'systemProxy').toLowerCase();
  if (modeType !== 'tun' || config.dnsConfig?.fakeIpTunAutoEnable !== true) {
    return { config, corrected: false };
  }
  const corrected = config.dnsConfig.enableFakeIp === false;
  return {
    config: {
      ...config,
      dnsConfig: {
        ...config.dnsConfig,
        enableFakeIp: corrected ? true : config.dnsConfig.enableFakeIp,
        fakeIpTunAutoEnable: false,
      },
    },
    corrected,
  };
}

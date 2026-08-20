/**
 * W24 防回潮：package job 分开编译 helper 与 app，两次 Cargo 调用必须继承同一个 commit build id。
 *
 * 行为侧由 helper-proto/manager 的 Rust 测试覆盖；这里守住 CI 注入点，避免源码逻辑正确、真正出包
 * 却都退回 1.0.0，导致同 protocol 旧 helper 再次无法识别。
 */
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = fileURLToPath(new URL('../../..', import.meta.url));
const read = (rel: string): string => readFileSync(join(REPO_ROOT, rel), 'utf8');

describe('W24：app/helper 共享出包构建身份', () => {
  it('package job 以 job-level env 把 github.sha 同时注入 helper 与 app', () => {
    const workflow = read('.github/workflows/package.yml');
    const packageStart = workflow.indexOf('\n  package:');
    const releaseStart = workflow.indexOf('\n  release:', packageStart + 1);
    expect(packageStart, 'package job 消失').toBeGreaterThanOrEqual(0);
    const packageJob = workflow.slice(packageStart, releaseStart === -1 ? undefined : releaseStart);

    expect(
      packageJob.includes('env:\n      POLARIS_BUILD_ID: ${{ github.sha }}'),
      'POLARIS_BUILD_ID 必须在 package job 级注入；只放 helper 或 tauri 单步会让两侧身份分叉',
    ).toBe(true);
    expect(packageJob.includes('cargo build --release -p polaris-helper'), 'helper 构建腿消失').toBe(
      true,
    );
    expect(packageJob.includes('uses: tauri-apps/tauri-action@v0'), 'app 构建腿消失').toBe(true);
  });

  it('shared crate 是唯一读取 POLARIS_BUILD_ID 的 Rust 真值点', () => {
    const proto = read('crates/helper-proto/src/lib.rs');
    expect(proto.includes('option_env!("POLARIS_BUILD_ID")')).toBe(true);

    const rustFiles = [
      read('crates/helper-client/src/manager.rs'),
      read('crates/helper/src/platform/windows/helper.rs'),
      read('crates/helper/src/platform/macos/handler.rs'),
      read('crates/helper/src/platform/linux/handler.rs'),
      read('src-tauri/src/runtime/helper.rs'),
    ];
    expect(
      rustFiles.every((source) => !source.includes('option_env!("POLARIS_BUILD_ID")')),
      '调用方不得各自读取环境变量；否则 app/helper 可能出现第二份 fallback/校验规则',
    ).toBe(true);
  });

  it('升级卡片区分 protocol 落后与同 protocol build 漂移，且不硬编码期望版本', () => {
    const screen = read('ui/src/components/screens/settings/SettingsHelper.tsx');
    expect(screen.includes('status.version < status.expectedProtocolVersion')).toBe(true);
    expect(screen.includes("t('helper.buildVersionMismatch')")).toBe(true);
    expect(screen.includes('required: status.expectedProtocolVersion')).toBe(true);
    expect(screen.includes('required: 3'), 'shared protocol 已是 v1，UI 不得残留 v3 硬编码').toBe(false);
  });
});

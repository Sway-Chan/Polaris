/**
 * W26 跨文件接线门：行为预算在 Rust 单测，这里钉住最容易被后续“局部优化”拆开的四条生产腿。
 */
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../../');
const read = (path: string) => readFileSync(resolve(ROOT, path), 'utf8');

describe('W26 bounded core-log wiring', () => {
  it('runtime never hands a fixed output file back to sing-box', () => {
    const proxy = read('src-tauri/src/runtime/proxy.rs');
    expect(proxy).toContain('log_file_path: None');
    expect(proxy).toContain('runtime_log_output_is_owned_by_bounded_sink_not_core');
  });

  it('all three helper spawners drain pipes through the shared bounded writer', () => {
    for (const path of [
      'crates/helper/src/platform/macos/server.rs',
      'crates/helper/src/platform/linux/server.rs',
      'crates/helper/src/platform/windows/winproc/win.rs',
    ]) {
      const source = read(path);
      expect(source, path).toContain('polaris_log_budget::spawn_pipe_loggers(');
      expect(source, path).toContain('std::process::Stdio::piped()');
    }
  });

  it('legacy log is surfaced and only archived/deleted after an explicit user action', () => {
    const commands = read('src-tauri/src/commands/misc.rs');
    const archive = commands.indexOf('fn archive_legacy_log(');
    const copy = commands.indexOf('std::fs::copy(source, &temporary)', archive);
    const sync = commands.indexOf('.and_then(|file| file.sync_all())', copy);
    const commit = commands.indexOf('std::fs::rename(&temporary, destination)', sync);
    const remove = commands.indexOf('std::fs::remove_file(source)', commit);
    expect([archive, copy, sync, commit, remove].every((position) => position >= 0)).toBe(true);
    expect(archive).toBeLessThan(copy);
    expect(copy).toBeLessThan(sync);
    expect(sync).toBeLessThan(commit);
    expect(commit).toBeLessThan(remove);

    const screen = read('ui/src/components/screens/logs/LogsScreen.tsx');
    expect(screen).toContain('.legacyInfo()');
    expect(screen).toContain('.archiveLegacy()');
    expect(screen).toContain('.deleteLegacy()');
    expect(screen).toContain("confirmTwice(DELETE_LEGACY_KEY");
    expect(screen).toContain("t('logs.legacyBody'");
    expect(screen).toContain("t('logs.archiveLegacy')");
    expect(screen).toContain("t('logs.deleteLegacy')");
    expect(commands).toContain('key::NATIVE_LOG_FILE_TYPE');

    const deleteCommand = commands.slice(
      commands.indexOf('pub fn logs_delete_legacy('),
      commands.indexOf('fn delete_legacy_log('),
    );
    expect(deleteCommand).toContain('state.config().dir().join(LEGACY_SINGBOX_LOG)');
    expect(deleteCommand).not.toMatch(/(?:path|source)\s*:\s*(?:String|PathBuf)/);
  });
});

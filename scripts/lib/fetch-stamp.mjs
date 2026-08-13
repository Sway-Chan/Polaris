/**
 * fetch 脚本的**落地指纹簿**：让 skip-exists 的判据从「文件在不在」变成「文件是不是**当前钉扎版本**的产物」。
 *
 * ── 为什么需要它 ──
 * 三个 fetch 脚本原本都是 `existsSync(dest) && !FORCE → skip`。这条判据在 CI 上无害（每次
 * 全新 checkout，`resources/` 是空的），但在**本地开发机**上是个静默陷阱：升了
 * `bundledCoreVersion` 之后不加 `--force` 重拉，盘上留着的还是旧核，而脚本照打
 * `skip (exists)` 并汇报成功 ⇒ **本地打出来的包静默带旧核**。
 * 实测踩到过：`resources/win/sing-box.exe` 停在 beta.5 之前那份（sha `9f255377…`，
 * 正确的是 `08ed3fd0…`），而每次跑脚本都显示 4 ready 0 failed。
 *
 * fetch-core 此前在 skip 那行印了「若刚改 bundledCoreVersion，须加 --force」的提示。
 * 但**跑在成功路径上的一句提示不是门** —— 它要求人每次都读、且读了要动手。改成判据。
 *
 * ── 为什么是「旁记指纹」而不是「直接校验盘上的二进制」──
 * manifest 钉的是**压缩包**的 sha，不是解出来那个二进制的 sha（fetch-core 头注的原话：
 * 「压缩包正确则解出的二进制必正确（tar/zip 解压确定性），故不必再对解压后二进制单独算 sha」）。
 * 要直接校验盘上产物就得给 manifest 再加一套「二进制 sha」字段、且每次升版本多维护一份 ——
 * 而这套字段唯一的用途就是回答「盘上这份是不是当前版本的」，正是本文件用一行指纹回答的问题。
 * [不选「跑 `sing-box version`」：四个平台的核在任一台开发机上只有一个能执行]
 * [不选「在二进制里 grep 版本串」：那是启发式，匹配到与否都不构成证明]
 *
 * ── 指纹簿与产物同生共死 ──
 * 落在 `resources/.fetch-stamp.json`，而 `.gitignore` 里 `/resources/*` 整个忽略 ⇒
 * 指纹簿与它描述的产物在同一棵被忽略的树里，一起被 clean 掉，不会出现「产物没了指纹还在」。
 * 判据也**同时要求** `existsSync(dest)`，故手删单个产物一样会重拉。
 */
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from 'fs';
import { join } from 'path';

/** 指纹簿路径（相对仓库根）。 */
export function stampPath(root) {
  return join(root, 'resources', '.fetch-stamp.json');
}

/**
 * 读指纹簿。文件不存在 / 内容坏掉一律返回 `{}` —— 读不到就当作「什么都没落地过」，
 * 后果是多拉一次（幂等且有 sha 校验），比因为一个坏 JSON 让整条构建腿硬失败合理。
 */
export function readStamps(root) {
  const p = stampPath(root);
  if (!existsSync(p)) return {};
  try {
    const v = JSON.parse(readFileSync(p, 'utf-8'));
    return v && typeof v === 'object' ? v : {};
  } catch {
    return {};
  }
}

/**
 * 盘上这份是否就是 `fingerprint` 描述的那一份。
 *
 * 两个条件缺一不可：产物文件在 **且** 指纹对得上。
 */
export function isFresh(stamps, key, fingerprint, destExists) {
  return destExists && stamps[key] === fingerprint;
}

/**
 * 记一条指纹并落盘（`.tmp` → rename 原子顶替，与三个 fetch 脚本落产物的写法一致）。
 *
 * 逐条写而不是跑完一次性写：某个平台失败时，已成功的那些平台的指纹必须已经落下，
 * 否则下一趟会把它们全部重拉。
 */
export function recordStamp(root, key, fingerprint) {
  const stamps = readStamps(root);
  stamps[key] = fingerprint;
  const p = stampPath(root);
  const tmp = `${p}.tmp`;
  rmSync(tmp, { force: true });
  writeFileSync(tmp, `${JSON.stringify(stamps, null, 2)}\n`, 'utf-8');
  renameSync(tmp, p);
}

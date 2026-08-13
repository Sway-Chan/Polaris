/**
 * zip 解压：三个 fetch 脚本（core / cronet / dashboard）共用的**单一真值点**。
 *
 * ── 为什么必须分平台，统一不了一条命令 ──
 *   · **Linux 只能走 `unzip`**：GNU tar 不认 zip（本机实测 tar 1.35：
 *     `tar: This does not look like a tar archive`）。ubuntu-22.04 runner 自带 unzip，
 *     已由绿过的 linux 打包腿（run 30968735116）实证。
 *   · **Windows 只能走 `tar`**：`unzip` 在 GH windows runner 上**不保证存在** ——
 *     `actions/runner-images` 的 windows-2022 清单只列了 7zip，MSYS2 明写
 *     「pre-installed on image but not added to PATH」，Git bash 的 `usr/bin` 里有没有
 *     unzip 未经证实。而 `C:\Windows\System32\tar.exe` 是 **bsdtar**（Windows 10 1803 /
 *     Server 2019 起随系统自带），bsdtar 认 zip。
 *     三个 fetch step 都没写 `shell:` ⇒ Windows 上默认 pwsh，拿的是系统 PATH。
 *   · macOS 的 tar 同为 bsdtar，两条都成立；跟 Linux 一起走 unzip，让判据收敛成
 *     「win32 → tar，其余 → unzip」这一条。
 *
 * 🔴 **不要写成「先试 unzip，失败再 fallback tar」**：静默 fallback 会让「unzip 其实不在」
 *    这个事实永远不被观测到，下次换 runner 镜像时原样复发，且届时报错点已经漂到别处。
 *    判据按平台写死，并把实际用的解压器打进日志 —— 换了镜像行为变化时肉眼可见。
 *
 * 🔴 **只提供「全解」，不提供「按成员选择性解压」**：此前 fetch-cronet 用 unzip 的
 *    `-j`（junk-paths）配一个通配 pattern 只解出要的那一个成员，那套 flag 语义
 *    在 bsdtar 上没有逐字对应物。三个调用点里两个本来就是全解，统一成全解后**三处形状一致**，
 *    成员定位交给解压后的 JS 代码做（临时工作区随后整个删掉，多解出来的字节不留痕）。
 */
import { execFileSync } from 'child_process';
import { readdirSync } from 'fs';
import { join } from 'path';

/**
 * 把 `archive` 全部解压到 `destDir`（destDir 须已存在）。
 *
 * 失败即抛（`execFileSync` 在非零退出时抛），由调用方的 try/catch 转成该目标的 FAILED。
 */
export function extractZip(archive, destDir) {
  if (process.platform === 'win32') {
    // bsdtar：`-x` 解，`-f` 指定档，`-C` 指定落点。zip 由 libarchive 自动识别，不需 `-z`。
    execFileSync('tar', ['-xf', archive, '-C', destDir], { stdio: 'inherit' });
  } else {
    // `-q` 静默（日志里只留下面那行汇总），`-o` 覆盖同名不交互提问（CI 里没有 stdin 可答）。
    execFileSync('unzip', ['-q', '-o', archive, '-d', destDir], { stdio: 'inherit' });
  }
}

/**
 * 在解压产物 `dir` 下**递归**找基名为 `name` 的文件，要求**恰好一个**命中。
 *
 * ⚠️ 本注释里不写 `<星号><斜杠>` 那个字面组合 —— 它会**提前闭合块注释**，让后面的正文变成代码。
 * （本文件已被它咬过两次；同类事故在 `verify-packaging.mjs` 里也发生过一次。）
 *
 * 🔴 **必须递归，不能只看一两层**（2026-08-05 实测教训）：上游布局深度不一 ——
 * sing-box 的 zip 是单层前缀（`sing-box-<ver>-<os>-<arch>/sing-box.exe`），而 cronet 的
 * Go module zip 深达 5 层（`github.com/sagernet/cronet-go/lib/<平台>@<伪版本>/libcronet.dll`）。
 * 此前用 unzip 的 `'<星号>/<member>'` 通配之所以两者通吃，是因为 **unzip 的通配默认跨 `/` 匹配**；
 * 改用「解全 + JS 定位」后若照搬「一层」的直觉，cronet 必然找不到（本机实测已复现：
 * `module zip (windows_amd64) 里没有 libcronet.dll`）。
 *
 * **恰好一个**而不是「取首个命中」：多命中说明上游塞了同名的第二份（调试符号 / 多架构并存），
 * 取首个等于按目录遍历顺序赌一个，赌错了是**静默装错库**。宁可红在这里。
 *
 * 用 `Dirent.isDirectory()` 而非 `statSync`（与 `verify-packaging.mjs` 的 `walk` 相反，那边刻意
 * 跟进软链）：这里扫的是**我们刚解出来的**临时产物、不是别人铺的 bundle，跟进软链只会引入环路风险，
 * 换不到覆盖面。深度上限兜底。
 */
export function findInZipRoot(dir, name, maxDepth = 12) {
  const hits = [];
  const walk = (d, depth) => {
    if (depth > maxDepth) return;
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const full = join(d, e.name);
      if (e.isDirectory()) walk(full, depth + 1);
      // 判据用 `isFile()` 而不是 `existsSync`：后者对**同名目录**也为真，会把目录当成命中返回，
      // 调用方随即 `copyFileSync` 它 → EISDIR，报错点离真因两跳远。
      else if (e.name === name && e.isFile()) hits.push(full);
    }
  };
  walk(dir, 0);
  if (hits.length === 1) return hits[0];
  if (hits.length > 1) {
    throw new Error(`解压产物里有 ${hits.length} 个 ${name}，无法确定该取哪个：${hits.join(', ')}`);
  }
  return null;
}

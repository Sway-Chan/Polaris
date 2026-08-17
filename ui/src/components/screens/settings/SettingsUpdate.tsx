/**
 * SettingsUpdate —— 更新子页（原型 [data-sec="update"] L2270-2352）。
 *
 * 六块（**后台检查间隔置顶**：它是订阅与规则资源共用的全局 cadence，下面两张卡的文案都写着
 * 「跟随上方后台检查间隔」——被夹在应用版本卡之后时，「上方」指的东西要往回翻才看得到）：
 *  1. 后台检查间隔（统一 cadence，订阅/规则资源共享）
 *  2. GitHub 加速器：默认（不加速）+ 推荐镜像预设（GH_PROXY_PRESETS，对齐 上游 shared/gh-proxy.ts）+
 *     自定义。原型的 ghproxy.example/mirror.example 是 RFC 2606 保留示例域名（非真实服务），不塞；
 *     改用 上游 出货的真实可用镜像 gh-proxy.org 系列（[0] 为默认推荐加速地址）。
 *  3. 应用版本：7 态更新机（idle/checking/available/downloading/downloaded/manual/error）
 *  4. sing-box 内核：当前版本 + 来源徽章（official/fork/unknown 三态）、已暂存、在线检查与更新、
 *     手动换核/回滚/恢复出厂、内核自动更新、仅兼容 minor
 *  5. 规则资源自动更新
 *  6. 订阅更新：启动时自动更新 + 跟随间隔 + 更新通道
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { UserConfig } from '@/contracts/types';
// 单一真值：预设清单由 domain 层持有（同文件的 `ghMirrorUrl()` 自己也消费 `GH_PROXY_PRESETS[0]` 作
// 兜底镜像）。本页原先另存一份同名同构副本 —— 两份一旦漂移，下拉里选的镜像与下载兜底用的镜像就不是一个。
import { GH_PROXY_PRESETS } from '@/domain/gh-proxy';
import {
  updateApi,
  coreUpdateApi,
  versionApi,
  type InstallAdvisory,
  type UpdateProgress,
  type UpdateInfo,
  type VersionInfo,
} from '@/ipc/api-client';
import { useDialogStore } from '../../dialogs/dialog-store';
import {
  Phead,
  Card,
  CardH,
  CardSub,
  SetBlock,
  SetRow,
  SetRowSection,
  Switch,
  Select,
  TextInput,
  Segmented,
  Button,
  Dot,
  Pill,
  Spinner,
  ProgressBar,
} from './primitives';
import CoreVersionBanner from './CoreVersionBanner';
import { markAppVersionSkipped } from '../../layout/app-update-banner';
import { cn } from '@/lib/utils';
import { toast } from '@/lib/error-handler';
import { useConfirmTwice } from '@/lib/confirm-twice';
import { revealOnToggle } from '@/components/reveal';
import {
  appDownloadIntegrity,
  backgroundIntervalSelectValue,
  isPortableZipUpdate,
  progressInvalidatesUpdateInfo,
  progressResetsIntegrity,
  releaseShipsDigest,
  ruleResourceAutoStatus,
  ruleResourceAutoUpdateChecked,
  subscriptionAutoUpdateStatus,
  type AppDownloadIntegrity,
} from './settings-logic';

export interface SettingsUpdateProps {
  config: UserConfig;
  update: (patch: Partial<UserConfig>) => Promise<void>;
}

/** 本页唯一的原地二次确认项（原型 :4075 `core-rollback`）。 */
const CORE_ROLLBACK_KEY = 'core-rollback';

/**
 * `manual` = **已下载，需用户手动替换**（当前唯一来源：Windows 便携版 zip）。
 *
 * 它刻意**不并进 `error`**：后端那条回包是 `{ ok:false, reason:"form-mismatch" }`，
 * `ok:false` 对调用方准确（一次安装都没执行），但 error 态渲染的是红色「更新失败」+「重试」按钮 ——
 * 便携用户的包**已经下好了**，重试只会再下一遍同一个 zip。两者下一步动作不同，故分成两态。
 */
type UpdateState =
  | 'idle'
  | 'checking'
  | 'available'
  | 'downloading'
  | 'downloaded'
  | 'manual'
  | 'error';

/**
 * 内核在线更新的四态（与上面的应用更新机独立：两条链路的后端命令、闸门、失败面都不一样）。
 *  - `idle`：没查过 / 查完已是最新（结果文案另用 `coreUpdMsg` 承载）
 *  - `checking` / `updating`：进行中，两个按钮都禁用
 *  - `available`：查到严格更新的版本，才渲染「立即更新」
 */
type CoreUpdateState = 'idle' | 'checking' | 'available' | 'updating';

/**
 * 内核更新检查的**整体**兜底（契约 `polaris-上游-capability-contract.md:130`「`core-update:check`(20s 兜底)」）。
 *
 * 后端 `runtime/http.rs:72` 的 15s 是**逐跳**超时（每次 HTTP 请求各算各的），与「整体 20s」不等价：
 * `core_update_check` 一次调用里含 releases 拉取 + 可能的重定向跟随，逐跳超时叠加后总耗时可以远超 20s，
 * 而 UI 会一直卡在「正在检查…」的转圈上。
 *
 * ⚠️ **诚实边界**：Tauri 的 `invoke` **没有取消语义**，本兜底只能中止「UI 的等待」，
 * 后端那次请求仍会跑完（跑完的结果被丢弃）。故它守住的是「用户不会无限等」，
 * 不是「20 秒后一定不再有网络活动」——真正的请求级总超时得在 Rust 侧加，属另一批。
 */
const CORE_UPDATE_CHECK_TIMEOUT_MS = 20_000;

/**
 * 给 promise 加一个**只影响调用方等待**的上限。超时即 reject（`timeoutError()` 构造的错误），
 * 原 promise 的后续 settle 被静默丢弃（已 clearTimeout，不会二次 settle —— Promise 本身也只 settle 一次）。
 */
function withTimeout<T>(p: Promise<T>, ms: number, timeoutError: () => Error): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(timeoutError()), ms);
    p.then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      (e: unknown) => {
        clearTimeout(timer);
        reject(e instanceof Error ? e : new Error(String(e)));
      },
    );
  });
}

type SubProxyPolicy = 'follow' | 'proxy' | 'direct';

export default function SettingsUpdate({ config, update }: SettingsUpdateProps) {
  const { t } = useTranslation();
  const [us, setUs] = useState<UpdateState>('idle');
  // 应用版本走真实 versionApi.getInfo()（对齐 SettingsAbout 的做法），不再硬编码字面量。
  const [appVersionInfo, setAppVersionInfo] = useState<VersionInfo | null>(null);
  // 用户刚选中「自定义…」但尚未输入内容时的本地态：此时 config.ghProxyPrefix 仍为空，
  // 不能仅凭 prefix 派生显示态，否则自定义输入框永远不可达（选中即弹回默认项）。
  const [ghCustomMode, setGhCustomMode] = useState(false);
  const [updateInfo, setUpdateInfo] = useState<UpdateInfo | null>(null);
  /** 本页是否正持有一次自己发起的下载（见 `downloadUpdate` / 进度监听器）。 */
  const startedHereRef = useRef(false);
  const [progress, setProgress] = useState(0);
  const [errMsg, setErrMsg] = useState('');
  const [downloadedPath, setDownloadedPath] = useState<string | null>(null);
  /**
   * 本次下载的摘要校验结果（`update_download` 回包的 `verified`）。
   *
   * 它是**上一次**下载的事实，跨包沿用就是拿旧结论给新包背书。故凡是「一次新的下载可能开始」
   * 的入口都必须先回 `unknown`，而那样的入口有**三个**（只堵前两个不够）：
   *  1. 本页 `checkUpdate()` —— 查到的可能是另一个版本；
   *  2. 本页 `downloadUpdate()` —— 本会话自己发起；
   *  3. **`onProgress` 监听器** —— 别的窗口发起的下载会把本页推进下载生命周期，而本页
   *     **拿不到那次 invoke 的回包**。`emit_progress` 走 `events::broadcast`（`events.rs:213`
   *     的 `handle.emit`）fan-out 给所有窗口，可达来源有两条：`startup_tasks::spawn_auto_download`
   *     的启动自动下载，以及 `update_popup_action` 的「更新/重试」（`updater.rs:1526-1545`：
   *     用户点弹窗按钮 → 复查 → 直调 `update_download`，回包只回弹窗窗口）。
   *     不复位就会出现「设置页对一个已逐字节校验过的包举着上一次的『未提供校验摘要』」。
   *
   * 第 3 个入口**不按 status 分支各补一次**：哪些 status 要复位由 `progressResetsIntegrity`
   * 的真值表单点回答（含「复用本地已有包那条腿只发 `downloaded`、不发 `downloading`」这一格），
   * 监听器只留一个调用点。按分支补是枚举型判据，联合里多一个取值就会静默漏掉一格。
   *
   * 失效方向安全：万一 invoke 回包早于事件到达，最坏是被事件盖回 `unknown`（少一条警告），
   * 绝不会凭空造出一条假警告。只有确知 `unverified` 才提示 —— 判据见 `appDownloadIntegrity`。
   */
  const [downloadIntegrity, setDownloadIntegrity] = useState<AppDownloadIntegrity>('unknown');
  const [coreVer, setCoreVer] = useState<{
    current: string;
    hasBackup: boolean;
    build: 'official' | 'fork' | 'unknown';
  } | null>(null);
  const [staged, setStaged] = useState<{ version: string; stagedAt: string } | null>(null);
  const [coreBusy, setCoreBusy] = useState(false);
  const [coreMsg, setCoreMsg] = useState('');
  // 内核在线更新（core_update_check / core_update_run）三态 + 查到的目标版本 + 结果回显。
  const [cus, setCus] = useState<CoreUpdateState>('idle');
  /** 内核更新通道。缺省 `stable` —— 与本开关引入前写死「只看正式版」的行为逐字一致。 */
  const coreChannel: 'stable' | 'prerelease' =
    config.coreUpdateChannel === 'prerelease' ? 'prerelease' : 'stable';
  const [coreLatest, setCoreLatest] = useState<{
    version: string;
    downloadUrl?: string;
    crossBand: boolean;
  } | null>(null);
  const [coreUpdMsg, setCoreUpdMsg] = useState('');
  const [coreUpdErr, setCoreUpdErr] = useState('');
  const openDialog = useDialogStore((s) => s.open);
  const closeDialog = useDialogStore((s) => s.close);
  /* 本页唯一的原地二次确认项：内核回滚（原型 :4075 `core-rollback`）。同排的「恢复出厂内核」保留
     弹窗——它要交代的是「当前备份会被清除」这条按钮上写不下的副作用，属 ConfirmDialog 的既有射程
     （见 `destructive-confirm-wiring.test.ts` T3 头注：需成段解释的确认不退役）。 */
  const { armed, confirmTwice } = useConfirmTwice();

  // 拉取应用版本：运行时包元数据是唯一真值，加载失败时不伪造版本号。
  useEffect(() => {
    void versionApi.getInfo().then(setAppVersionInfo).catch(() => undefined);
  }, []);

  const refreshCoreVer = useCallback(() => {
    void coreUpdateApi
      .getVersionInfo()
      .then((v) =>
        setCoreVer({ current: v.currentVersion, hasBackup: v.hasBackup, build: v.build }),
      )
      .catch(() => undefined);
  }, []);

  // 拉取内核版本信息
  useEffect(() => refreshCoreVer(), [refreshCoreVer]);

  // 拉取暂存内核状态 + 订阅变更（水合快照 + 订阅推送，同 HomeScreen 解锁检测模式）
  useEffect(() => {
    let cancelled = false;
    void coreUpdateApi
      .getAutoStatus()
      .then((s) => {
        if (!cancelled) setStaged(s.staged);
      })
      .catch(() => undefined);
    const off = coreUpdateApi.onAutoStatusChanged((s) => setStaged(s.staged));
    return () => {
      cancelled = true;
      off();
    };
  }, []);

  // 订阅下载进度
  useEffect(() => {
    const off = updateApi.onProgress((p: UpdateProgress) => {
      // 第三个「新下载可能开始」的入口：事件可能来自别的窗口（弹窗腿 / 启动自动下载腿），
      // 本页拿不到那次回包 ⇒ 不复位就会把上一次的结论盖到这个新包头上。
      // **唯一调用点**：该不该复位由 `progressResetsIntegrity` 的真值表单点回答，不在下面的
      // status 分支里各补一次 —— 那是枚举型判据，联合里多一个取值就会静默漏掉一格。
      if (progressResetsIntegrity(p.status)) setDownloadIntegrity('unknown');
      // 同一根因的第二半：本页持有的 `updateInfo` 描述的是**上一次检查查到的那个版本**，
      // 对别的窗口下的那个包不成立。不清 ⇒ 卡片会举着 beta 的版本号与预发布徽标，去描述一份
      // 正式包。判据多一个「不是本页发起」的限定词，因为 `updateInfo` 在 `downloading` 态就要
      // 渲染（清早了会打断本页自己的下载）—— 详见 `progressInvalidatesUpdateInfo` 的头注，
      // 那里也写明了这只是止血、正解（把随行事实带过来）属 W5。
      if (progressInvalidatesUpdateInfo(p.status, startedHereRef.current)) setUpdateInfo(null);
      if (p.status === 'downloading') {
        setUs('downloading');
        setProgress(p.percentage);
      } else if (p.status === 'downloaded') {
        setUs('downloaded');
      } else if (p.status === 'error') {
        setUs('error');
        setErrMsg(p.error ?? p.message ?? t('settings.update.downloadInterrupted'));
      }
    });
    return off;
  }, []);

  /**
   * 手动「检查更新」——**全仓唯一**含预发布的 App 更新入口（`check(true)`）。
   *
   * 这是「拉」：用户自己按的按钮，且结果卡会用 `isPrerelease` 明确标出拿到的是不是预发布。
   * 与之相对的「推」腿恒只看正式版 —— 启动自动检查与托盘检查更新（连同弹窗点「更新」时的复查）
   * 共用后端 `commands/updater.rs::PUSH_UPDATE_INCLUDE_PRERELEASE`，顶部常驻横幅同口径
   * （`AppUpdateBanner` 的 `check(false)`）。我们主动推到用户脸上的东西不能是预发布，因为 App 侧
   * **没有**任何「更新通道」开关（`coreUpdateChannel` 只管内核）。
   *
   * # 如实登记：一处**故意不修**的残留不一致
   *
   * 顶部横幅可能举着 `v1.2.0`（正式版），用户点「去更新」进到本页、再点「检查更新」，拿到的却是
   * `v1.3.0-beta.1`。这与弹窗那条缺陷**不同形**：中间隔着一次用户主动点击，不是我们替他换的包；
   * 且拿到的那份会带预发布徽标，用户看得见。真要消除得给 App 加更新通道设置，属独立一批。
   */
  async function checkUpdate() {
    setUs('checking');
    // 上一轮下载的校验结论对新查到的版本不成立，先清空再查。
    setDownloadIntegrity('unknown');
    try {
      const r = await updateApi.check(true);
      if (r.hasUpdate && r.updateInfo) {
        setUpdateInfo(r.updateInfo);
        setUs('available');
      } else {
        setUs('idle');
      }
    } catch (e) {
      setUs('error');
      setErrMsg(e instanceof Error ? e.message : String(e));
    }
  }

  /** 跳过此版本：持久化 updater.rs::update_skip（skippedVersion），再做本地 UI 回退。 */
  async function skipVersion() {
    if (updateInfo) {
      try {
        await updateApi.skip(updateInfo.version);
      } catch (e) {
        console.error('[update] skip failed:', e);
      }
      // 常驻横幅（AppUpdateBanner）与本卡是两个独立组件、各持一份检查结果快照。后端 skippedVersion
      // 要**下次** check 才会生效，不同步会话态的话，在这里跳过后顶部横幅仍会举着同一个版本号。
      markAppVersionSkipped(updateInfo.version);
    }
    setUs('idle');
  }

  async function downloadUpdate() {
    if (!updateInfo) return;
    setUs('downloading');
    setProgress(0);
    setDownloadIntegrity('unknown');
    // 认领这次下载：进度事件 fan-out 给所有窗口，监听器只能靠这面旗分辨「这是我点的」
    // 还是「别的窗口在下」。用 ref 而非 state：监听器在 `useEffect(..., [])` 的闭包里，
    // 读 state 只会读到挂载那一刻的快照。
    startedHereRef.current = true;
    try {
      const r = await updateApi.download(updateInfo);
      // 走到这里必是成功回包：业务失败在 `ipc-client.invoke` 拆信封时 throw IpcError
      // （`api-client.ts` 头注第 3 条，拆包点唯一），一律落进下面的 catch —— 那里
      // `downloadIntegrity` 保持函数开头那次复位的 `unknown`，不出提示。
      setDownloadIntegrity(appDownloadIntegrity(r));
      if (r.success) {
        setDownloadedPath(r.filePath ?? null);
      } else {
        setUs('error');
        setErrMsg(r.error ?? t('settings.about.downloadFail'));
      }
    } catch (e) {
      setUs('error');
      setErrMsg(e instanceof Error ? e.message : String(e));
    } finally {
      // 交还认领：这次下载已经落地（成或败），之后再收到的下载事件就都不是本页的了。
      // 放 `finally` 而非成功分支 —— 失败也必须交还，否则本页会永久把别人的下载当成自己的。
      startedHereRef.current = false;
    }
  }

  /**
   * 安装已下载的更新包。**两段式**：后端返 `needConfirm` 时先弹说明框讲清 OS 会怎么拦、用户怎么放行，
   * 确认后才带 `confirmed:true` 重调（此时后端才停代理 + 起脚本）。
   *
   * 为什么必须讲：**本应用走 ad-hoc 签名**（无 Developer ID / Authenticode）。macOS 侧安装脚本会
   * 自动清 quarantine，但万一失败用户会遇到「装完打不开」；Windows 侧 SmartScreen 根本消不掉。
   * 装完打不开还不告诉人 = 制造一个静默故障。
   */
  async function installUpdate(confirmed = false) {
    if (!downloadedPath) return;
    try {
      const r = await updateApi.install(downloadedPath, confirmed);
      if (r.needConfirm && r.advisory) {
        const adv = r.advisory as InstallAdvisory;
        openDialog({
          kind: 'confirm',
          payload: {
            title: t(`settings.update.advisory.${adv}.title`),
            message: t(`settings.update.advisory.${adv}.message`),
            confirmLabel: t('settings.update.advisory.continue'),
            onConfirm: async () => {
              closeDialog();
              await installUpdate(true);
            },
          },
        });
        return;
      }
      if (r.handedToSystem || r.reason === 'form-mismatch') {
        // 便携版 zip：**不是失败**，是「已下载、待手动替换」（判据与理由见 isPortableZipUpdate）。
        if (isPortableZipUpdate(downloadedPath)) {
          setUs('manual');
          setErrMsg(
            t('settings.update.portableManualReplace', {
              path: downloadedPath,
            }),
          );
        } else {
          setUs('error');
          setErrMsg(
            t('settings.update.formMismatch'),
          );
        }
      }
      // 成功路径无需处理：后端已 app.exit(0)，进程随即退出。
    } catch (e) {
      setUs('error');
      setErrMsg(e instanceof Error ? e.message : String(e));
    }
  }

  /** 换核类操作的统一外壳：忙碌态 + 结果回显 + 版本信息刷新。 */
  async function runCoreOp(op: () => Promise<{ ok?: boolean; error?: string }>, okMsg: string) {
    setCoreBusy(true);
    setCoreMsg('');
    try {
      const r = await op();
      // `cancelled` = 用户在文件选择器点了取消，正常流程，不显示任何提示。
      if ((r as { cancelled?: boolean }).cancelled) return;
      if ((r as { needConfirm?: boolean }).needConfirm) {
        const d = r as { uploadVersion?: string; bundledVersion?: string; filePath?: string };
        openDialog({
          kind: 'confirm',
          payload: {
            title: t('settings.core.replaceConfirmTitle'),
            message: t('settings.core.replaceConfirmMessage', {
              upload: d.uploadVersion || t('common.unknown'),
              bundled: d.bundledVersion || '-',
            }),
            danger: true,
            onConfirm: async () => {
              closeDialog();
              await runCoreOp(
                () => coreUpdateApi.replaceManual({ filePath: d.filePath, force: true }),
                okMsg,
              );
            },
          },
        });
        return;
      }
      setCoreMsg(r.ok ? okMsg : r.error || t('settings.core.swapFailedShort'));
    } catch (e) {
      setCoreMsg(e instanceof Error ? e.message : String(e));
    } finally {
      setCoreBusy(false);
      refreshCoreVer();
    }
  }

  /**
   * 检查内核在线更新（`core_update_check`）。
   *
   * 后端全绿：fork 硬闸（零网络前置拒绝）→ 拉 SagerNet/sing-box releases → 选平台/架构资产 →
   * 严格版本比较 → 跨带标注。此前全仓 `.tsx` **零调用点** ⇒ 用户在 UI 上完全没有在线更新内核的入口，
   * 只剩「手动换核」（自己去下二进制）。本函数即那个缺失的入口。
   *
   * 失败一律如实回显（不吞、不伪装成「已是最新」）：fork 硬闸、网络失败、20s 兜底超时各有各的文案。
   */
  async function checkCoreUpdate() {
    setCus('checking');
    setCoreUpdMsg('');
    setCoreUpdErr('');
    setCoreLatest(null);
    try {
      const r = await withTimeout(
        coreUpdateApi.check(),
        CORE_UPDATE_CHECK_TIMEOUT_MS,
        () =>
          new Error(
            t('settings.coreManagement.checkTimeout', {
              sec: CORE_UPDATE_CHECK_TIMEOUT_MS / 1000,
            }),
          ),
      );
      if (r.hasUpdate && r.latestVersion) {
        setCoreLatest({
          version: r.latestVersion,
          downloadUrl: r.downloadUrl,
          crossBand: !!r.crossBand,
        });
        setCus('available');
        return;
      }
      // hasUpdate:false（含「本机无适配资产」这条后端如实报无更新的腿）→ 已是最新。
      setCus('idle');
      setCoreUpdMsg(t('settings.coreManagement.upToDate'));
    } catch (e) {
      setCus('idle');
      setCoreUpdErr(e instanceof Error ? e.message : String(e));
    }
  }

  /**
   * 执行内核在线更新（`core_update_run`）：下载 → sha256 强校验 → 解归档 → 停核换核起核（零提权）。
   *
   * **不复用 `runCoreOp`**：那个外壳把 `ok:false` 一律渲染成「操作失败」，而本命令的
   * `{ result:'deferred', ok:false, crossBand:true }` 是**跨带硬闸按设置拦下**（用户自己开着
   * 「仅兼容 minor 版本自动更新」），不是失败 —— 折叠成「失败」会让用户去重试一件永远不会成功的事，
   * 而正确的下一步是关掉那个开关或走手动换核。三条腿（applied / deferred / noop）各给各的文案。
   */
  async function runCoreUpdate() {
    if (!coreLatest) return;
    setCus('updating');
    setCoreUpdMsg('');
    setCoreUpdErr('');
    try {
      const r = await coreUpdateApi.update(coreLatest.downloadUrl);
      if (r.result === 'deferred' && r.crossBand) {
        setCoreUpdErr(
          t('settings.coreManagement.crossBandFound', {
            version: r.latestVersion || coreLatest.version,
          }),
        );
        setCus('idle');
        return;
      }
      if (r.result === 'noop') {
        setCoreUpdMsg(t('settings.coreManagement.upToDate'));
      } else if (r.ok) {
        setCoreUpdMsg(t('settings.core.applied'));
      } else {
        setCoreUpdErr(t('settings.core.swapFailedShort'));
      }
      setCus('idle');
      setCoreLatest(null);
    } catch (e) {
      setCus('idle');
      setCoreUpdErr(e instanceof Error ? e.message : String(e));
    } finally {
      // 换核成功后当前版本已变 —— 不刷新的话卡片会一直显示旧版本号（又一处 UI 与事实分叉）。
      refreshCoreVer();
    }
  }

  const interval = backgroundIntervalSelectValue(config);
  // Select 显示态：空=不加速；命中推荐预设=显示该预设项；否则（非空且非预设）=自定义。
  const ghProxy = ghCustomMode
    ? 'custom'
    : !config.ghProxyPrefix
      ? 'none'
      : (GH_PROXY_PRESETS as readonly string[]).includes(config.ghProxyPrefix)
        ? config.ghProxyPrefix
        : 'custom';
  const subPolicy: SubProxyPolicy = config.subscriptionProxyPolicy ?? 'follow';
  // 规则资源自动更新的真实三态（off / manual / active），右侧状态徽标据此如实回显。
  const resAutoStatus = ruleResourceAutoStatus(config);
  const subAutoStatus = subscriptionAutoUpdateStatus(config);
  /**
   * fork 核 ⇒ 在线更新与自动更新两条路后端都硬闸拒绝（`core_update_check` / `core_update_run` 开头
   * 零网络直接返 CODE_FORK_BLOCKED）。UI 据此置灰，**判据只认 `build === 'fork'`**：
   * `unknown`（探测失败）后端并不拦，跟着置灰就会把能用的功能误锁死。
   */
  const coreForkBlocked = coreVer?.build === 'fork';
  /**
   * 「无校验摘要」的两处常驻明示 —— **两个字段、两个时机，不合并**：
   *  - `available` 态用 `updateInfo.sha256`（检查阶段就已知）：这是**预测**，且是它还能改变
   *    用户决定的最后一刻；
   *  - `downloaded` 态用回包 `verified`（下载之后才有）：这是**既成事实**，也是唯一覆盖
   *    「复用本地已有包」那条腿的字段。
   * 合并成一个状态必然要么在 check 阶段替下载腿说话、要么把明示推迟到风险已承担之后。
   *
   * 文案只讲**缺什么**（无 sha256 摘要 ⇒ 无法逐字节核对），不去枚举「还剩哪些弱校验」：
   * 剩下那两级（清单 `fileSize` 等值判据 / Content-Length）都**有条件**——前者只在
   * `fileSize > 0` 时生效、后者只在服务端给了该头时生效，写进文案就成了一句会在边界上变假的断言。
   * 把缺口精确限定在「摘要」这一级，既不说成「完全没校验」，也不暗示「已校验」。
   */
  const releaseDigestMissing = !releaseShipsDigest(updateInfo);
  const downloadUnverified = downloadIntegrity === 'unverified';

  return (
    <section className="screen" data-sec="update">
      <Phead title={t('settings.nav.update')} sub={t('settings.update.pageSub')} />

      {/* 1. 后台检查间隔（全局 cadence，故置顶） */}
      <SetBlock>
        {/* 文案按真实调度形态写，不含糊其辞：订阅与规则资源确实按此周期后台刷新；应用更新则只在
            每次启动时检查一次（startup_tasks 的 5s 一次性任务），全仓没有周期性 app update 轮询——
            旧文案「应用更新、规则资源、订阅刷新统一使用此检查周期」把应用更新也算进周期，是错的。 */}
        {/* subscriptionUpdateIntervalHours 已有真实消费者，disabled 已删除：
            `subscription_scheduler.rs::select_due` 按此周期判订阅是否陈旧（0 = 「仅手动」→ 周期不跑，
            后端已正确处理该哨兵值）；`rule_resource_scheduler.rs` 落地后规则资源也跟随同一周期。
            旧注释声称「全仓零消费者」已过时。 */}
        {/* **两个字段一起写**：规则资源调度器读的是 `ruleResourceUpdateIntervalHours`（独立字段，
            store seed 12），而本卡文案与下方规则资源卡都承诺「跟随上方后台检查间隔」。此前该字段
            只在规则资源开关被拨动的那一刻快照一次 → 之后改本下拉不会传导，「跟随」是假的；且若拨动
            开关时本项恰为「仅手动」(0)，会把规则资源也永久钉成 0，而开关旁却显示「按所选间隔在
            后台刷新」——又一处假绿。两边同写即让「跟随」成为真话。 */}
        <SetRow
          label={t('settings.update.intervalCard')}
          tip={t('settings.update.intervalCardSub')}
        >
          <Select
            value={interval}
            onChange={(e) =>
              void update({
                subscriptionUpdateIntervalHours: Number(e.target.value),
                ruleResourceUpdateIntervalHours: Number(e.target.value),
              })
            }
            aria-label={t('settings.update.intervalCard')}
            style={{ width: '170px' }}
          >
            <option value="0">{t('settings.update.intervalManualOnly')}</option>
            <option value="6">{t('settings.update.intervalHours', { n: 6 })}</option>
            <option value="12">{t('settings.update.intervalHours', { n: 12 })}</option>
            <option value="24">{t('settings.update.intervalHours', { n: 24 })}</option>
            <option value="72">{t('settings.update.intervalDays', { n: 3 })}</option>
            <option value="168">{t('settings.update.intervalDays', { n: 7 })}</option>
          </Select>
        </SetRow>
      </SetBlock>

      {/* 2. GitHub 加速器 */}
      <SetBlock>
        <SetRow
          label={t('resources.ghProxy')}
          tip={`${t('settings.update.ghAccelSub')} ${t('settings.update.ghNote')}`}
        >
          <Select
            id="gh-mirror-sel"
            value={ghProxy}
            onChange={(e) => {
              const v = e.target.value;
              if (v === 'none') {
                setGhCustomMode(false);
                void update({ ghProxyPrefix: '' });
              } else if (v === 'custom') {
                setGhCustomMode(true);
              } else {
                // 选中推荐预设：直接落盘该镜像前缀（走通用 config 保存路径）。
                setGhCustomMode(false);
                void update({ ghProxyPrefix: v });
              }
            }}
            aria-label={t('resources.ghProxy')}
            style={{ width: '340px' }}
          >
            <option value="none">{t('resources.direct')}</option>
            {GH_PROXY_PRESETS.map((p) => (
              <option key={p} value={p}>
                {t('settings.update.ghMirrorOption', { url: p })}
              </option>
            ))}
            <option value="custom">{t('common.customEllipsis')}</option>
          </Select>
        </SetRow>
        {ghProxy === 'custom' && (
          <SetRow label={t('resources.customDomainLabel')}>
            <TextInput
              id="gh-custom-input"
              className="mono"
              value={config.ghProxyPrefix ?? ''}
              onChange={(e) => void update({ ghProxyPrefix: e.target.value })}
              placeholder="https://cdn.example/ · cdn.example"
              style={{ width: '340px' }}
            />
          </SetRow>
        )}
      </SetBlock>

      {/* 3. 应用版本 6 态更新机 */}
      <Card className="core-card" id="app-update-card">
        <CardH>{t('settings.update.appVersionCard')}</CardH>

        {us === 'idle' && (
          <div className="us-state" data-us="idle">
            <div className="core-ver">
              <Dot variant="ok" />
              <div style={{ flex: 1 }}>
                <b>Polaris</b>{' '}
                <span className="cv-tag">
                  {appVersionInfo?.appVersion ? `v${appVersionInfo.appVersion}` : '—'}
                </span>
                <CardSub>{t('settings.update.upToDate')}</CardSub>
              </div>
              {/* 检查 / 下载 / 安装**三段均已接线**（`update_check` → `update_download` → `update_install`，
                  commands/updater.rs）：检查经 http.rs client 拉 GitHub releases 并做版本比对；下载复用
                  CoreDownloader（sha256 强校验 + 原子落盘）；安装两段式（advisory 确认后起脚本 + app.exit）。
                  ⚠️ 旧注释「下载/安装侧仍是真机门，点下载会如实报未接线」与实现相反，已订正。
                  真正待真机验的是**装完能不能起来**（ad-hoc 签名 / SmartScreen），不是「有没有接线」。 */}
              <Button variant="ghost" size="sm" onClick={checkUpdate}>
                <span>{t('settings.about.checkUpdate')}</span>
              </Button>
            </div>
          </div>
        )}

        {us === 'checking' && (
          <div className="us-state" data-us="checking">
            <div className="core-ver">
              <Spinner />
              <div style={{ flex: 1 }}>
                <b>{t('settings.about.checkingUpdate')}</b>
              </div>
            </div>
          </div>
        )}

        {us === 'available' && updateInfo && (
          <div className="us-state" data-us="available">
            <div className="core-ver">
              <Dot variant="flow" style={{ background: 'hsl(var(--flow))', boxShadow: '0 0 0 3px hsl(var(--flow)/0.18)' }} />
              <div style={{ flex: 1 }}>
                <b>{t('settings.update.foundNew')}</b> <span className="cv-tag">{updateInfo.version}</span>
                {/* 预发布标记。本卡的检查是**全仓唯一**会返回预发布的 App 更新入口
                    （上面 `checkUpdate()` 的 `updateApi.check(true)`）：启动自动检查与托盘检查更新
                    由后端 `PUSH_UPDATE_INCLUDE_PRERELEASE` 钉死只看正式版，顶部常驻横幅同口径
                    （`AppUpdateBanner` 的 `check(false)`）。
                    不标出来的话，用户在这里点「下载」拿到的是什么档次的版本只能靠 tag 文本自己猜 ——
                    而「是不是预发布」在 GitHub 上是一个**独立的布尔**，与 tag 怎么命名无关
                    （`updater.rs` 的内核通道注释同样点过这一条：档次不可从字符串反推）。
                    `isPrerelease` 一路从 `AppUpdateInfo` 传到这里，此前零消费。

                    **与「无摘要」徽标的位置约定**（两者同为 `Pill variant="warn"`，会同框出现）：
                    徽标紧挨着它限定的那个东西。**预发布 = 版本档次** ⇒ 贴在版本号后面；
                    **无摘要 = 这份字节的校验状态** ⇒ 留在行尾（摘要腿既有的槽位，本批不动它）。
                    两枚都堆到行尾就成了一坨警告色，看不出谁在说谁。 */}
                {updateInfo.isPrerelease && (
                  <>
                    {' '}
                    <Pill variant="warn">{t('settings.update.prereleaseTag')}</Pill>
                  </>
                )}
                <CardSub>
                  {new Date(updateInfo.publishedAt).toLocaleDateString()} ·{' '}
                  {(updateInfo.fileSize / 1024 / 1024).toFixed(1)} MB
                </CardSub>
                {/* 两条明示**语义正交，可以同时出现**：版本档次（这个版本成不成熟）与校验状态
                    （这份字节能不能验真）是两回事，一个版本完全可能既是预发布又没带摘要。
                    顺序按「先版本、后这份下载」：上一行说的就是版本号，紧跟着谈它的档次，
                    再谈即将取回的那份字节。样式沿用摘要腿那份（marginTop/lineHeight），
                    两条叠在一起时行距一致，不会看出是两批人写的。 */}
                {updateInfo.isPrerelease && (
                  <CardSub style={{ marginTop: 4, lineHeight: 1.7 }}>
                    {t('settings.update.prereleaseNote')}
                  </CardSub>
                )}
                {releaseDigestMissing && (
                  <CardSub style={{ marginTop: 4, lineHeight: 1.7 }}>
                    {t('settings.update.digestMissingBefore')}
                  </CardSub>
                )}
              </div>
              {releaseDigestMissing && (
                <Pill variant="warn">{t('settings.update.digestMissingTag')}</Pill>
              )}
            </div>
            {updateInfo.releaseNotes && (
              <details className="us-notes" onToggle={revealOnToggle}>
                <summary>{t('settings.update.releaseNotes')}</summary>
                <CardSub style={{ marginTop: 6, lineHeight: 1.7, whiteSpace: 'pre-line' }}>
                  {updateInfo.releaseNotes}
                </CardSub>
              </details>
            )}
            <div style={{ display: 'flex', gap: 9, marginTop: 12 }}>
              <Button variant="flow" size="sm" onClick={downloadUpdate}>
                <svg viewBox="0 0 24 24" width="14" fill="none" stroke="currentColor" strokeWidth={1.8}>
                  <path d="M12 3v11M8 10l4 4 4-4M4 19h16" />
                </svg>
                <span>{t('settings.update.download')}</span>
              </Button>
              <Button variant="ghost" size="sm" onClick={() => void skipVersion()}>
                <span>{t('settings.update.bannerSkip')}</span>
              </Button>
            </div>
          </div>
        )}

        {us === 'downloading' && (
          <div className="us-state" data-us="downloading">
            <div className="core-ver">
              <Spinner />
              <div style={{ flex: 1 }}>
                <b>{t('settings.update.downloadingApp')}</b> <span className="cv-tag">{updateInfo?.version}</span>
              </div>
              <span className="mono">{progress}%</span>
            </div>
            <ProgressBar value={progress} style={{ marginTop: 10 }} />
            <div className="card-sub mono" style={{ marginTop: 6 }}>
              {((progress / 100) * ((updateInfo?.fileSize ?? 0) / 1024 / 1024)).toFixed(1)} /{' '}
              {((updateInfo?.fileSize ?? 0) / 1024 / 1024).toFixed(1)} MB
            </div>
          </div>
        )}

        {us === 'downloaded' && (
          <div className="us-state" data-us="downloaded">
            <div className="core-ver">
              <Dot variant="ok" />
              <div style={{ flex: 1 }}>
                <b>{t('settings.update.downloadDone')}</b> <span className="cv-tag">{updateInfo?.version}</span>
                {/* 姊妹腿（与摘要腿同一条纪律，理由也同源）：`available` 是「要不要下」的决策点，
                    这里是「要不要重启装上去」的决策点 —— 后者不可逆，离用户真的跑这些字节最近。
                    只挂在 `available` 就等于：用户下完包、切个屏回来，界面上再没有任何字说这是预发布。
                    **说明文案不跟着重复三遍**：摘要腿的 before/after 说的是两件不同的事
                    （「将要取回未署摘要的包」vs「即将执行未经校验的字节」），而预发布的说明三处一字不差，
                    抄三遍只是噪声；档次这个事实由徽标持续持有，解释留在做决定的那一屏。 */}
                {updateInfo?.isPrerelease && (
                  <>
                    {' '}
                    <Pill variant="warn">{t('settings.update.prereleaseTag')}</Pill>
                  </>
                )}
                <CardSub>{t('settings.update.restartToInstall')}</CardSub>
                {downloadUnverified && (
                  <CardSub style={{ marginTop: 4, lineHeight: 1.7 }}>
                    {t('settings.update.digestMissingAfter')}
                  </CardSub>
                )}
              </div>
              {downloadUnverified && (
                <Pill variant="warn">{t('settings.update.digestMissingTag')}</Pill>
              )}
              <Button variant="flow" size="sm" onClick={() => void installUpdate()}>
                <span>{t('settings.update.restartAndInstall')}</span>
              </Button>
            </div>
          </div>
        )}

        {/* 便携版：包已下载好，只差用户手动解压覆盖 —— 不用 err 色、不给「重试」（重试只会重下同一个 zip）。 */}
        {us === 'manual' && (
          <div className="us-state" data-us="manual">
            <div className="core-ver">
              <Dot variant="ok" />
              <div style={{ flex: 1 }}>
                <b>{t('settings.update.downloadedManual')}</b> <span className="cv-tag">{updateInfo?.version}</span>
                {/* 第三条腿：`manual` 与 `downloaded` 互斥，转 manual 后上面那枚徽标整块消失，
                    而这里恰是「你自己解压覆盖当前程序」的一刻 —— 与摘要腿完全同形的理由。 */}
                {updateInfo?.isPrerelease && (
                  <>
                    {' '}
                    <Pill variant="warn">{t('settings.update.prereleaseTag')}</Pill>
                  </>
                )}
                <CardSub style={{ lineHeight: 1.7, wordBreak: 'break-all' }}>{errMsg}</CardSub>
                {/* 姊妹腿：`manual` 与 `downloaded` 是**互斥**的两个态，一旦转 manual，
                    `downloaded` 那条明示就整块消失 —— 而这里恰恰是「你自己去解压覆盖」的那一刻，
                    离用户真的执行这些字节最近。同一个判据必须两条腿都挂。 */}
                {downloadUnverified && (
                  <CardSub style={{ marginTop: 4, lineHeight: 1.7 }}>
                    {t('settings.update.digestMissingAfter')}
                  </CardSub>
                )}
              </div>
              {downloadUnverified && (
                <Pill variant="warn">{t('settings.update.digestMissingTag')}</Pill>
              )}
            </div>
          </div>
        )}

        {us === 'error' && (
          <div className="us-state" data-us="error">
            <div className="core-ver">
              <Dot variant="err" />
              <div style={{ flex: 1 }}>
                <b style={{ color: 'hsl(var(--err))' }}>{t('settings.update.failed')}</b>
                <CardSub>{errMsg}</CardSub>
              </div>
              <Button variant="ghost" size="sm" onClick={downloadUpdate}>
                <span>{t('common.retry')}</span>
              </Button>
            </div>
          </div>
        )}

        {/* 自动下载 **App** 更新。
            ⚠️ 本行此前绑的是 `autoUpdateCore` —— 那是**内核**（sing-box 二进制）自动更新的开关，
            与「App 安装包自动下载」是两件完全不同的事：用户在这里拨一下，改的是内核换核策略，
            而内核那张卡上又没有任何开关能反映它 ⇒ 一个字段两处语义、且都不对。本批拆开：
            这里绑 `autoDownloadUpdate`（App 侧），内核自动更新移到下方 sing-box 卡里绑 `autoUpdateCore`。
            语义取「默认关」（`!!`，与 types.ts 对新字段的声明同口径），不用 `!== false` —— 后者会把
            存量用户（无该键）一律显示成开，而这条腿会真的去下载，默认值必须保守。
            ✅ **后端已接线**（本轮订正过期标注）：`runtime/startup_tasks.rs` 的
            `should_auto_download_update`（`autoDownloadUpdate === true`，缺省关）+ `spawn_auto_download`
            —— 启动检查到新版本时后台下载安装包，**只下载不安装**。故 desc 改写为真实行为，
            不再写「暂未生效」。 */}
        {/* desc **随开关态变**（原型 `:4204-4207` 直接改写 `#auto-dl-desc` 的文案）：这是「开关自身
            即反馈」不成立时的补偿设计 —— 只看开关位置说不出「关掉之后会怎样」。本仓此前是一句静态
            文案（只描述开态），关掉后那句话就成了假的。 */}
        <SetRowSection>
          <SetRow
            label={t('settings.update.autoDownloadApp')}
            tip={
              config.autoDownloadUpdate
                ? t('settings.update.autoDownloadAppDesc')
                : t('settings.update.autoDownloadAppDescOff')
            }
          >
            <Switch
              id="auto-dl-swt"
              checked={!!config.autoDownloadUpdate}
              onChange={(v) => {
                void update({ autoDownloadUpdate: v });
                // 原型 `:4208` 的第二条腿：desc 改写之外还发一条 toast。两条都要——desc 在行内、
                // 用户的视线此刻在开关上，toast 才是「我刚才那一下产生了什么后果」的即时回执。
                toast.success(
                  v
                    ? t('settings.update.autoDownloadOnToast')
                    : t('settings.update.autoDownloadOffToast'),
                );
              }}
              aria-label={t('settings.update.autoDownloadApp')}
            />
          </SetRow>
        </SetRowSection>
      </Card>

      {/* 4. sing-box 内核（内核版本变更横幅置于卡片上方——对齐 上游 把它挂在内核相关设置顶部）。
             ⚠️ 旧注释「当前换核链路是桩、无生产者，故实际不会渲染」与实现相反（且与
             `CoreVersionBanner.tsx:7-18` 的组件头注直接矛盾）：换核链路 core_replace_manual /
             core_rollback / core_reset_factory / core_update_run **均为真实现**，
             `EVENT_CORE_VERSION_CHANGED` 在 `swap_core_with_restart` 换核成功即发射，
             `pendingChangeNotice` 由同一处写入 ⇒ 本横幅会真实触发。已订正。 */}
      <CoreVersionBanner />
      <Card className="core-card">
        <CardH
          tip={`${t('settings.update.coreCardSub')} ${t('settings.update.coreCardSub2')}`}
        >
          {t('settings.update.coreCard')}
        </CardH>

        {coreVer && (
          <div className="core-ver" style={{ marginTop: 12 }}>
            <Dot variant="ok" />
            <div style={{ flex: 1 }}>
              <b>{t('settings.update.coreCurrent')}</b> <span className="cv-tag">{coreVer.current}</span>
              {/* 来源说明按 build **三态**给（后端 `classify_core_build` 探测失败如实报 unknown，
                  不回落 official 伪装）。此前只写了 fork 一态：official/unknown 的用户看不到任何来源信息，
                  而 unknown 恰恰是「在线更新可能不适用」的那一类，最该说明。 */}
              <CardSub>
                {coreVer.build === 'fork'
                  ? t('settings.core.forkBlocked')
                  : coreVer.build === 'unknown'
                    ? t('settings.coreManagement.srcNoteUnknown')
                    : t('settings.coreManagement.srcNoteOfficial')}
              </CardSub>
            </div>
            {/* 来源徽章。此前恒渲染 `<Pill variant="ok">运行中</Pill>` —— 那句既与本卡主题（内核来源/版本）
                无关，又是**假断言**：它不看 proxyStatus，内核没跑时照样显示「运行中」。
                换成 build 三态徽章：official=ok / fork=warn / unknown=default（中性，不给绿也不给黄）。 */}
            <Pill
              variant={
                coreVer.build === 'official' ? 'ok' : coreVer.build === 'fork' ? 'warn' : 'default'
              }
            >
              {coreVer.build === 'official'
                ? t('settings.coreManagement.sourceOfficial')
                : coreVer.build === 'fork'
                  ? t('settings.coreManagement.sourceFork')
                  : t('settings.coreManagement.sourceUnknown')}
            </Pill>
          </div>
        )}

        {/* 「已暂存」条**刻意移出 `coreVer &&`**：staged 来自 `core_update_get_auto_status`（+
            `onAutoStatusChanged` 订阅），与 `core_get_version_info` 是两条独立的取数腿。此前嵌在
            版本信息里 ⇒ 版本探测失败（coreVer 为 null）时，即便后端已暂存一个内核、并把事件推上来，
            这条也渲染不出来 = 用户既看不到暂存版本，也点不到「立即应用」。
            ⚠️ 事件发射端（AUTO_UPDATE_STATUS）由更新域另批补；订阅与渲染分支本身在此已可用。 */}
        {staged && (
          <div className="core-ver" style={{ marginTop: 8 }}>
            <Dot variant="idle" />
            <div style={{ flex: 1 }}>
              <b>{t('settings.update.coreStaged')}</b> <span className="cv-tag">{staged.version}</span>
              <CardSub>{t('settings.update.coreStagedDesc')}</CardSub>
            </div>
            <Button
              variant="flow"
              size="sm"
              disabled={coreBusy}
              onClick={() =>
                void runCoreOp(
                  async () => {
                    const r = await coreUpdateApi.applyStaged();
                    return { ok: r.result === 'applied', error: r.error ?? r.result };
                  },
                  t('settings.core.applied'),
                )
              }
            >
              <span>{t('settings.coreManagement.applyNow')}</span>
            </Button>
          </div>
        )}

        {/* 内核在线更新入口 —— **结构与「应用版本」卡逐格同构**（陈先生 2026-07-30 裁定）。
            此前这里是「卡底一条按钮带 + 结果另起一段 CardSub」，而应用版本卡是「状态即行、按钮在行尾」；
            同一屏两种形态不是设计取舍，是这条入口后补时留下的拼接痕迹。现按 `.core-ver` 状态行统一：
            一个点/spinner + 标题 + 说明 + 行尾动作，四个态各占一行、互斥。
            fork 核置灰理由仍前置在按钮 tip 上（后端有零网络硬闸，让用户看到「为什么不能点」）。 */}
        {cus === 'idle' && (
          <div className="core-ver" style={{ marginTop: 12 }}>
            <Dot variant="idle" />
            <div style={{ flex: 1 }}>
              <b>{t('settings.coreManagement.checkCoreUpdate')}</b>
              <CardSub>
                {coreChannel === 'prerelease'
                  ? t('settings.coreManagement.channelPrereleaseNote')
                  : t('settings.coreManagement.channelStableNote')}
              </CardSub>
            </div>
            <Button
              variant="ghost"
              size="sm"
              disabled={coreForkBlocked || coreBusy}
              data-tip={coreForkBlocked ? t('settings.core.forkBlocked') : undefined}
              onClick={() => void checkCoreUpdate()}
            >
              <span>{t('settings.coreManagement.checkCoreUpdate')}</span>
            </Button>
          </div>
        )}

        {cus === 'checking' && (
          <div className="core-ver" style={{ marginTop: 12 }}>
            <Spinner />
            <div style={{ flex: 1 }}>
              <b>{t('settings.coreManagement.checkingCore')}</b>
            </div>
          </div>
        )}

        {cus === 'available' && coreLatest && (
          <div className="core-ver" style={{ marginTop: 12 }}>
            <Dot
              variant="flow"
              style={{ background: 'hsl(var(--flow))', boxShadow: '0 0 0 3px hsl(var(--flow)/0.18)' }}
            />
            <div style={{ flex: 1 }}>
              <b>{t('settings.coreManagement.foundCore')}</b>{' '}
              <span className="cv-tag">{coreLatest.version}</span>
              {coreLatest.crossBand && (
                <CardSub>
                  {t('settings.coreManagement.crossBandRisk')}
                </CardSub>
              )}
            </div>
            <Button variant="flow" size="sm" disabled={coreBusy} onClick={() => void runCoreUpdate()}>
              <span>{t('settings.coreManagement.updateNow')}</span>
            </Button>
          </div>
        )}

        {cus === 'updating' && (
          <div className="core-ver" style={{ marginTop: 12 }}>
            <Spinner />
            <div style={{ flex: 1 }}>
              <b>{t('settings.core.swapping')}</b>
            </div>
          </div>
        )}

        {coreUpdMsg && <CardSub style={{ marginTop: 8 }}>{coreUpdMsg}</CardSub>}
        {/* 失败如实回显（fork 硬闸 / 网络失败 / 20s 兜底超时 / 跨带被拦），不吞不改写。 */}
        {coreUpdErr && (
          <CardSub style={{ marginTop: 8, color: 'hsl(var(--err))' }}>{coreUpdErr}</CardSub>
        )}

        {/* 手动换核 / 回滚 / 重置出厂：三条链路均已接线（core_replace_manual / core_rollback /
            core_reset_factory），落位于 <config_dir>/core_update/（用户可写）⇒ **零提权**。
            返回值不再 fire-and-forget：runCoreOp 统一处理 needConfirm 二次确认 + 失败回显。
            「已知坏版本 alpha.42」pill 早已删除（恒定渲染的假数据，B5 反伪造），勿复活。 */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 9, marginTop: 12, flexWrap: 'wrap' }}>
          <Button
            variant="ghost"
            size="sm"
            disabled={coreBusy}
            onClick={() =>
              void runCoreOp(
                () => coreUpdateApi.replaceManual(),
                t('settings.core.replaced'),
              )
            }
          >
            <span>{t('settings.coreManagement.manualSwap')}</span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className={cn(armed === CORE_ROLLBACK_KEY && 'confirming')}
            // 无备份时禁用 + 诚实原因（这不是「未接线」，是真的没有可回滚的东西）。
            disabled={coreBusy || !coreVer?.hasBackup}
            data-tip={
              coreVer?.hasBackup
                ? undefined
                : t('settings.core.noBackup')
            }
            // 原地二次点击（原型 :4075）：回滚换掉正在跑的内核、断连接，此前**零闸门**，
            // 而同排的「恢复出厂内核」却要确认 —— 那是实现自己的双标，不是设计。
            onClick={() =>
              confirmTwice(CORE_ROLLBACK_KEY, () => {
                void runCoreOp(
                  () => coreUpdateApi.rollback(),
                  t('settings.core.rollbackSuccess'),
                );
              })
            }
          >
            <span>
              {armed === CORE_ROLLBACK_KEY
                ? t('settings.core.rollbackConfirm')
                : t('settings.coreManagement.rollback')}
            </span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={coreBusy}
            onClick={() =>
              openDialog({
                kind: 'confirm',
                payload: {
                  title: t('settings.core.resetFactoryTitle'),
                  message: t('settings.core.resetFactoryConfirm'),
                  danger: true,
                  onConfirm: async () => {
                    closeDialog();
                    await runCoreOp(
                      () => coreUpdateApi.resetFactory(),
                      t('settings.core.resetFactoryDone'),
                    );
                  },
                },
              })
            }
          >
            <span>{t('settings.coreManagement.resetFactory')}</span>
          </Button>
        </div>
        {coreMsg && <CardSub style={{ marginTop: 10 }}>{coreMsg}</CardSub>}

        {/* 内核自动更新（`autoUpdateCore`）—— 本批从「应用版本」卡里那个错绑的「发现新版本时自动下载」
            拆回它真正的归属。语义按 types.ts 的声明：**默认关**、读取端 `=== true` 判定，故用 `!!`。
            fork 核置灰（后端 fork 硬闸同样拦自动路径），原因写进 title。
            ✅ **后端已接线**（本轮订正过期标注）：`runtime/core_update_scheduler.rs` 的
            `auto_update_core_enabled`（`autoUpdateCore === true`，缺省关）+ 24h due 闸 —— 周期检查，
            新核**暂存**，仅在代理未运行时落位（绝不主动断流），跨大版本只提示不自动更新。
            故 desc 改写为真实行为，不再写「暂未生效」。
            解释一下与下一行的关系：本行决定「自动更新做不做」，下一行决定「自动更新最远能跨到哪」。 */}
        <SetRowSection>
          <SetRow
            label={t('settings.coreManagement.autoUpdate')}
            tip={t('settings.coreManagement.autoUpdateNoSchedulerHint')}
          >
            <Switch
              id="auto-core-swt"
              checked={!!config.autoUpdateCore}
              disabled={coreForkBlocked}
              tip={
                coreForkBlocked
                  ? t('settings.core.forkBlocked')
                  : undefined
              }
              onChange={(v) => void update({ autoUpdateCore: v })}
              aria-label={t('settings.coreManagement.autoUpdate')}
            />
          </SetRow>

        {/* 更新通道。sing-box 的 alpha/beta/rc 在 GitHub 上是 `prerelease=true`，而候选集此前写死
            `filter(!prerelease)` ⇒ 跑 alpha 的用户不但看不到 beta，连「有没有更新」都恒为否
            （剩下的正式版比 alpha 旧，版本闸判无更新）。陈先生 2026-07-30 报的正是这一格。
            **比对逻辑本身没问题**：`compare_semver` 完整实现 semver 预发布优先级
            （alpha.31 < alpha.32 < beta.1 < rc.1 < 1.14.0），缺的只是候选集里有没有它们。
            与下一行**正交**：本行决定看不看预发布，下一行决定跨不跨 minor，开这个不放开那个。 */}
          <SetRow
            label={t('settings.coreManagement.channel')}
            tip={t('settings.coreManagement.channelDesc')}
          >
            <Select
              style={{ width: 132 }}
              value={coreChannel}
              disabled={coreForkBlocked}
              onChange={(e) =>
                void update({ coreUpdateChannel: e.target.value as 'stable' | 'prerelease' })
              }
              aria-label={t('settings.coreManagement.channel')}
            >
              <option value="stable">{t('settings.coreManagement.channelStable')}</option>
              <option value="prerelease">
                {t('settings.coreManagement.channelPrerelease')}
              </option>
            </Select>
          </SetRow>

        {/* restrictCoreUpdateToCompatibleMinor：已有真实消费者 —— `core_update_run` 的跨带硬闸读它
            （`!== false` 即生效，与后端同口径）。此前「全仓零消费者」的注释与 disabled 均已过时。 */}
          <SetRow
            label={t('settings.update.restrictMinor')}
            tip={t('settings.update.restrictMinorDesc')}
          >
            <Switch
              checked={config.restrictCoreUpdateToCompatibleMinor !== false}
              onChange={(v) => void update({ restrictCoreUpdateToCompatibleMinor: v })}
            />
          </SetRow>
        </SetRowSection>
      </Card>

      {/* 5. 规则资源自动更新 */}
      <Card pad style={{ marginBottom: 16 }}>
        <SetRow
          label={t('settings.update.ruleResourceAutoCard')}
          tip={t('settings.update.ruleResourceAutoDesc')}
          ctrlClassName="rule-resource-auto-control"
        >
          {/* 状态是当前调度结果，不是静态说明：与开关组成同一行尾控件簇，避免标题区一套布局、
              下方状态再另起一套布局。间隔为仅手动时用 warn，避免开关亮着却给出“正在自动刷新”的假绿。 */}
          {resAutoStatus !== 'off' && (
            <Pill variant={resAutoStatus === 'active' ? 'ok' : 'warn'}>
              {resAutoStatus === 'active'
                ? t('settings.update.ruleResourceAutoStatusOn')
                : t('settings.update.ruleResourceAutoStatusManual')}
            </Pill>
          )}
          {/* ruleResourceAutoUpdate 已有真实调度器（`runtime/rule_resource_scheduler.rs`）。
              **正向语义、缺省为开**：仅显式 false 才停，与后端同口径。 */}
          <Switch
            id="res-auto-swt"
            checked={ruleResourceAutoUpdateChecked(config)}
            onChange={(v) =>
              void update({
                ruleResourceAutoUpdate: v,
                ruleResourceUpdateIntervalHours: config.subscriptionUpdateIntervalHours,
              })
            }
            aria-label={t('settings.update.ruleResourceAutoCard')}
          />
        </SetRow>
      </Card>

      {/* 6. 订阅更新 */}
      <SetBlock header={t('settings.update.subBlock')}>
        {/* autoUpdateSubscriptionOnStart：**旧注释与 disabled 提示均已过时且与事实相反**。
            `subscription_scheduler.rs::select_due` 第一行就是 `autoUpdateSubscriptionOnStart != Some(true)
            → 直接返空`——它正是整个订阅调度器的总开关（启动补更 8s 腿 + 30min 周期腿都受它门控）。
            且 `store.rs:263` 把它 seed 成 true，即存量用户订阅一直在自动更新，UI 却把开关锁死并声称
            「开启后不会自动更新订阅」——用户想关都关不掉。属与 #8/#18 同类的陈旧禁用，一并解禁。
            （本项不在 parity 清单 9 条内，是修 #18 时在同一张卡上发现的第 10 处，已在收尾报告点名。） */}
        {/* desc 按 `select_due` 的门链条件化，不再恒写「并按统一后台检查间隔定时刷新」：
            间隔选「仅手动」(0) 时**周期巡检腿整轮返空**，只剩启动补更腿会跑（那条走
            `ignore_staleness=true`，不受间隔影响）。旧文案对这一格是假话，用户开着开关、
            选了「仅手动」，会以为后台还在按时刷新。判据抽在 settings-logic 并直测。 */}
        <SetRow
          label={t('settings.update.subAutoOnStart')}
          tip={
            `${
              subAutoStatus === 'startup-only'
                ? t('settings.update.subAutoStartupOnly')
                : t('settings.update.subAutoWithInterval')
            } ${t('settings.update.subPerSubNote')}`
          }
        >
          <Switch
            checked={!!config.autoUpdateSubscriptionOnStart}
            onChange={(v) => void update({ autoUpdateSubscriptionOnStart: v })}
          />
        </SetRow>
        {/* 原「更新周期」只读行已删（陈先生 2026-07-30）：它恒显「跟随后台检查间隔」或「仅手动」，
            而上一行的 `tip` 已按同一个 `subAutoStatus` 把这件事说完了 —— 两行说同一句话，
            后一行还占一整行。删的是复述，不是信息。 */}
        {/* 文案按 `resolveSubscriptionViaProxy` 的真实判据写：`follow` 取的是**该订阅自己的**
            `updateViaProxy`（per-sub，默认直连），与分流规则毫无关系；`proxy`/`direct` 是全局覆盖，
            忽略 per-sub 字段。旧文案「跟随分流」是事实错误——用户会以为订阅拉取按路由规则走，
            于是「为什么我的订阅规则明明走代理，它却直连」永远对不上账。 */}
        <SetRow
          label={t('settings.update.subChannel')}
          tip={t('settings.update.subChannelDesc')}
        >
          <Segmented<SubProxyPolicy>
            ariaLabel={t('settings.update.subChannel')}
            value={subPolicy}
            onChange={(v) => void update({ subscriptionProxyPolicy: v })}
            options={[
              { value: 'follow', label: t('settings.update.subFollow') },
              { value: 'proxy', label: t('settings.update.subViaProxy') },
              { value: 'direct', label: t('settings.update.subDirect') },
            ]}
          />
        </SetRow>
      </SetBlock>
    </section>
  );
}

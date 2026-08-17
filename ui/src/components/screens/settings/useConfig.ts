/**
 * useConfig —— settings 屏共享的配置 hook。
 *
 * 9 个子页都需读取 UserConfig + 把改动写回。这里统一：
 *  - 挂载时拉取一次 config.get，loading/error 兜底；
 *  - 暴露 update(patch) 局部合并 + 立即 save（防抖可选，当前直写）；
 *  - 提供 setValue(path, value) 细粒度写（嵌套 tunConfig / dnsConfig 用）。
 *
 * 设计纪律：与 api-client.configApi.get/save 对齐（Polaris 签名不变）。
 *
 * **U-7 接线点**：设置页所有写都汇到本 hook 的 `update`（9 个子页共用同一个函数引用，
 * 见 `SettingsPage.tsx:33/64-82`），故「本次改动是否命中需重启 App 的键」只需在这里判一次。
 * 判定本身是纯函数，落在 `@/domain/app-restart-keys`（单一可替换点 + 单测钉住）。
 *
 * **配置暂存接线点（P6）**：同一条汇流腿也是设置页的暂存闸门。46 个 `update({...})` 调用点
 * **一个 class 判定都不写** —— 分流按键在这里做一次（`splitPatchByRoute` → `editRoute`），
 * 因为 `SettingsNetwork` 的 `update({ [key]: next })` 一处就跨 `mixedPort`(Class B) /
 * `controlPort`(Class A) 两个 class，键只有运行期才知道。总开关关时 `staged` 恒空、
 * 落盘的那份与今天逐字节相同（`config-patch-route.test.ts` 钉住）。
 *
 * **暂存回显的收口点（本轮）**：本 hook 自持的 `config` state 是**磁盘副本**，只装磁盘上（我们相信）
 * 有的那份；对外交出去的是 `effectiveConfigOf(磁盘副本, staged 条目)`。
 *
 * 为什么必须这么分，而不是把暂存值直接写进那份 state：`configApi.onChanged` 会静默重拉整份覆盖
 * （订阅调度器写 etag、托盘切模式、后端自愈都会触发），暂存值写在 state 里就会被那次覆盖抹掉 ——
 * 用户改完设置、任一 config 变更事件到达，开关就弹回原位而暂存条上还记着一条。与「节点列表不回显」
 * 完全同型的静默回退。判断只在这一处做（返回值那一行），**不下放到 9 个设置子页**。
 *
 * 顺带修掉同源的一条写侧渗漏：`update` 的落盘基准过去取「已合并暂存值的本地态」，故第二次改一个
 * 直落盘键时会把前一次暂存的键一起写进 config.json（与本文件自称的 FR-1「零磁盘写」相悖）。
 * 基准改成纯磁盘副本后，落盘的那份只含 `direct`。总开关关着时 `direct === patch`、`staged` 恒空、
 * `effectiveConfigOf` 返回入参本体 ⇒ 整条腿与今天逐字节等价。
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { configApi, windowApi } from '@/ipc/api-client';
import { toast } from '@/lib/error-handler';
import { useDialogStore } from '@/components/dialogs/dialog-store';
import { useAppStore, effectiveConfigOf } from '@/store/app-store';
import { useStagedConfigStore } from '@/store/staged-config-store';
import { useStagingActive } from '@/store/use-staging-active';
import { splitPatchByRoute } from './config-patch-route';
import { STAGED_SETTING_SECTION_LABELS } from './settings-logic';
import {
  appRestartRequiredChanges,
  appRestartRequiredDiff,
  restartKeysStillPending,
  type AppRestartRequiredKey,
} from '@/domain/app-restart-keys';
import type { UserConfig } from '@/contracts/types';

/** 弹窗里列出「哪几项要重启」时用的标签键 —— 直接复用设置页那三行自己的 label，文案不另起一套。 */
const RESTART_KEY_LABEL: Record<AppRestartRequiredKey, string> = {
  hardwareAcceleration: 'settings.general.hardwareAcceleration',
  windowEffects: 'settings.general.windowEffects',
  rememberWindowSize: 'settings.general.rememberWindowSize',
};

/**
 * 「已落盘、但要重启 App 才生效」的确认弹窗（U-7）。复用既有 confirm 基建（`dialog-store` + `ConfirmDialog`）。
 *
 * 文案三段，缺一不可：
 *  1. **哪些项**没生效（否则用户不知道弹窗在说谁）；
 *  2. **选「稍后」不等于没保存** —— 改动已经写进 config.json，只是本次运行仍按旧值跑。
 *     不写这句，「稍后」会被读成「放弃保存」，用户会去把开关来回拨；
 *  3. 代理在跑时补一句**重启会断连** —— 后端重启腿恒经 `ExitRequested` 停核（`commands/window.rs::app_restart`），
 *     这是真会发生的副作用，不许含糊。
 *
 * 只在 `configApi.save` **成功之后**调用：保存失败时用户看到的是 toast + 回滚，此时说「已落盘」是撒谎。
 */
function promptAppRestart(keys: AppRestartRequiredKey[], t: TFunction): void {
  const items = keys.map((k) => t(RESTART_KEY_LABEL[k])).join(t('common.listSeparator'));
  // 读 app-store 而非本 hook 的 config：代理运行态不在 UserConfig 里。用 getState() 取瞬时值即可
  //（弹窗是一次性快照，不需要订阅）。
  const proxyRunning = useAppStore.getState().proxyStatus?.running === true;
  const paragraphs = [
    t(
      'settings.restartApp.message',
      { items },
    ),
    t('settings.restartApp.persistNote'),
  ];
  if (proxyRunning) {
    paragraphs.push(
      t('settings.restartApp.proxyNote'),
    );
  }
  useDialogStore.getState().open({
    kind: 'confirm',
    payload: {
      title: t('settings.restartApp.title'),
      // 段间空行由 ConfirmDialog 的 `white-space: pre-line` 渲染。
      message: paragraphs.join('\n\n'),
      confirmLabel: t('settings.restartApp.confirm'),
      cancelLabel: t('settings.restartApp.later'),
      onConfirm: () => {
        // ConfirmPayload 契约：onConfirm 自负关闭。先关再发重启——重启腿若失败（IPC 不通），
        // 留一个关不掉的模态比失败本身更糟。
        useDialogStore.getState().close();
        void windowApi.restartApp().catch((e) => {
          toast.error(
            t('settings.restartApp.failed'),
            e instanceof Error ? e.message : String(e),
          );
        });
      },
    },
  });
}

export interface UseConfigResult {
  config: UserConfig | null;
  loading: boolean;
  /**
   * **仅**加载失败（显式 reload / 首次挂载拉取）。保存失败不写这里 —— 见 `update` 的注释：
   * 消费方（SettingsPage）用本字段决定「整屏塌成错误屏」，保存失败塌屏会把用户正在编辑的表单卸载掉。
   */
  error: string | null;
  /** 局部 patch（顶层字段合并）；嵌套字段需先解构再传完整对象 */
  update: (patch: Partial<UserConfig>) => Promise<void>;
  /** 强制重拉（如内核版本变化后） */
  reload: () => Promise<void>;
}

export function useConfig(): UseConfigResult {
  const { t } = useTranslation();
  // 暂存闸门的两个入参。开关取 store 而非编译期常量：`editRoute` 判的就是它，两处不同源会造出「半开」态。
  const stagingEnabled = useStagingActive();
  const stage = useStagedConfigStore((s) => s.stage);
  /** 暂存条目。**订阅**而非快照读：撤销一条（popover 逐项撤销）也要让设置页当场退回磁盘值。 */
  const stagedEntries = useStagedConfigStore((s) => s.entries);
  /** **磁盘副本**（不含暂存值）—— 见头注「暂存回显的收口点」。落盘基准与 U-7 差集都取它。 */
  const [config, setConfig] = useState<UserConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  /**
   * 代际计数（对齐 app-store.loadConfig 的同款守卫）：每次本地 update 自增，使**在飞的旧 get** 回填
   * 被丢弃。
   *
   * 没有它会回跳用户正在输入的值：受控输入框每键都 `update`（→ save → 广播 → 静默重拉），某拍回声的
   * get 携带的是早一拍的快照，却在后续键入之后才 resolve → `setConfig` 整体覆盖 → 输入值跳回、光标
   * 错位（真机可复现于端口 / 旁路列表这类每键写盘的字段）。乱序 resolve 时甚至会停在旧值。
   */
  const generation = useRef(0);
  /**
   * `config` state 的同步镜像。存在的唯一理由：`update` 的副作用（save / U-7 弹窗）**不能写在
   * `setConfig(prev => …)` 的 updater 里** —— React 18 StrictMode 会故意双跑 updater，
   * 那样每次保存都会发两次 `config:save`、弹两个一模一样的重启弹窗。
   * 拿 ref 当「上一份 config」的读取口，副作用就留在 updater 之外、只跑一次。
   *
   * 与 state 的一致性：**所有** `setConfig` 调用点都必须同步改这里（load 回填 / update 乐观写 / 失败回滚），
   * 少改一处就会拿陈旧 base 去合成 patch。同 state，它装的是**磁盘副本**，不含暂存值。
   */
  const latestConfig = useRef<UserConfig | null>(null);
  /**
   * `t` 的同步镜像。`load` 的依赖数组必须保持为空（它被挂载 effect 与事件订阅 effect 共同依赖，
   * 一旦随 `t` 重建，切换语言会连带重拉配置并重挂订阅），但下面的重启提示又需要当前语种的文案。
   */
  const tRef = useRef(t);
  tRef.current = t;
  /**
   * 本次进程**启动时**后端真正读到的三键值 —— 「重启会不会改变什么」的唯一正确基线。
   * 只拉一次（进程内不变）。拉失败保持 null ⇒ `restartKeysStillPending` 退回只看「值变了」的旧行为。
   */
  const startupFlags = useRef<Partial<UserConfig> | null>(null);

  useEffect(() => {
    let alive = true;
    void windowApi
      .startupConfigFlags()
      .then((f) => {
        if (alive) startupFlags.current = f;
      })
      .catch(() => {
        /* 拿不到就退回旧判据（宁可多提示一次，不静默）——不必打断任何 UI。 */
      });
    return () => {
      alive = false;
    };
  }, []);

  // silent=true：不动 loading/error，用于事件驱动的后台重拉——否则每次回声都会把 9 个子页闪成骨架屏。
  const load = useCallback(async (silent: boolean) => {
    const mine = generation.current;
    if (!silent) {
      setLoading(true);
      setError(null);
    }
    try {
      const cfg = await configApi.get();
      // 期间发生过本地写 → 本次回填必是旧快照，丢弃（本地乐观值更新，且那次写自己的回声会再来一趟）。
      if (mine !== generation.current) return;
      const prev = latestConfig.current;
      latestConfig.current = cfg;
      setConfig(cfg);
      /**
       * U-7 的第二条腿：**不经本 hook `update`** 的配置变更同样可能改到需重启的键。
       * 已知入口是备份导入 —— `backup_import_apply`（`commands/misc.rs`）的 `generalSettings` 类别按
       * 排除法覆盖 config 其余全部键（含这三个），在 Rust 侧整类替换后直接落盘 + 广播，
       * 压根不走 `update` ⇒ 只在 `update` 里判会让「导入一份备份把硬件加速关了」完全静默。
       * 托盘写入、后端自愈同理，故判据挂在**广播回声**这条汇流腿上，而不是逐个入口去补。
       *
       * 三处防重：① 只在 `silent`（事件驱动）路径判 —— 显式 reload / 首次挂载不是「有人改了配置」；
       * ② `prev` 为空（首次回填）不判，否则一进设置页就按默认值比一次；
       * ③ 本 hook 自己的 `update` 已乐观把 `latestConfig` 写成 `next`，其回声到达时差集为空 ⇒
       *    不会与 `update` 末尾那次提示重复弹。
       *
       * 判据用 `appRestartRequiredDiff` 而非 `appRestartRequiredChanges`：两边都是完整 config，
       * 键缺席意味着「取默认值」而不是「本次没碰」（见该函数注释）。
       */
      if (silent && prev) {
        const externalKeys = restartKeysStillPending(
          appRestartRequiredDiff(prev, cfg),
          startupFlags.current,
          cfg,
        );
        if (externalKeys.length > 0) promptAppRestart(externalKeys, tRef.current);
      }
    } catch (e) {
      // 静默重拉失败不打断当前界面（保留已显示的值），错误只在显式 reload 时呈现。
      if (!silent) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (!silent) setLoading(false);
    }
  }, []);

  const reload = useCallback(() => load(false), [load]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // 别处改了 config（托盘切模式/切节点、其它屏保存、后端自愈）→ 静默重拉。
  // Settings 这份 config 是**独立于 app-store 的第二副本**（本 hook 自持 state），不订阅就会一直
  // 停在打开那一刻的快照。事件是**无载荷信号**（后端 emit `{}`）：曾经带过的 newValue 经
  // strip_privacy_secrets 脱敏、且没走 config_get 那侧的 bypassLANList 补齐，与本 hook 的契约不同源，
  // 四个消费方因此没有一个读它 —— 既然全员重拉，那份载荷就是纯白做的深拷贝，已在后端删掉。
  // 回调必须零参（不得读 payload）：Rust 侧 `commands/config.rs` 的 `config_changed_payload_tests`
  // 把本文件 include_str! 进测试判据锁住这条形态，改成读参数的形态会让 `cargo test -p polaris` 转红。
  useEffect(() => {
    const off = configApi.onChanged(() => void load(true));
    return off;
  }, [load]);

  const update = useCallback(
    async (patch: Partial<UserConfig>) => {
      const prev = latestConfig.current;
      if (!prev) return;
      // 本地写 → 作废所有在飞 get（它们携带的都是本次写之前的快照）。
      generation.current++;
      // 暂存分流（P6）。开关关时 `staged` 恒空、`direct` 与 `patch` 逐字段相同 ⇒ 下面整条腿与今天等价。
      const { staged, direct } = splitPatchByRoute(patch, stagingEnabled);
      /** 真正会落盘的那一份：暂存走的键不在里面（FR-1「零磁盘写」）。
       *  基准 `prev` 是**纯磁盘副本** —— 取「合并过暂存值的本地态」会把上一次暂存的键一起写进盘。 */
      const persisted = { ...prev, ...direct };
      // U-7：本次改动是否命中「需重启 App 才生效」的键。**必须在写之前、拿 prev 当基准算**
      // （失败回滚后 latestConfig 会变回 prev，事后再算就分不清了）；提示则要等落盘成功才发。
      // 判据面取 `direct` 而非 `patch`：弹窗承诺的是「已经写进配置文件」，进了暂存的键还没写。
      // 三个需重启键（hardwareAcceleration / windowEffects / rememberWindowSize）都不是
      // `UserConfig` 字段 ⇒ 恒落 direct 腿，开关两侧这一段行为相同。
      const restartKeys = restartKeysStillPending(
        appRestartRequiredChanges(prev, direct),
        startupFlags.current,
        persisted,
      );
      // 乐观更新：先把**直落盘**那半合进磁盘副本，失败回滚由下面的 catch 兜底。
      // 暂存那半不进这里，它由下面的 `stage()` 进条目表，经返回值那行的 `effectiveConfigOf` 回显 ——
      // 写进磁盘副本会被 onChanged 的整份重拉抹掉（那正是本轮要修的静默回退）。
      latestConfig.current = persisted;
      setConfig(persisted);
      for (const [key, value] of staged) {
        // 设置键按**键路径**寻址（不是集合实体）：同一个键重复编辑覆盖同一条，计数不虚高。
        // label：**聚合键**（一个键背后是一整段设置，如 `dnsConfig`）取段级译名，否则回落键名。
        // 此前一律用键名，于是明细里显示「修改设置 · dnsConfig」——「键名足以认出」的前提是
        // 键与开关一一对应，而聚合键恰恰不满足（见 `STAGED_SETTING_SECTION_LABELS` 头注）。
        const sectionKey = STAGED_SETTING_SECTION_LABELS[key];
        stage({
          id: `setting:${key}`,
          kind: 'setting',
          label: t('home.stagedSetting', {
            key: sectionKey ? t(sectionKey) : key,
          }),
          entityPath: [key],
          nextValue: value,
        });
      }
      // 整份 patch 都进了暂存 ⇒ 零 IPC 写、零磁盘写，也就没有「落盘成功」可言（含 U-7 提示）。
      if (Object.keys(direct).length === 0) return;
      try {
        await configApi.save(persisted);
      } catch (e) {
        // save 抛错时回滚到 prev 并**走 toast**，不写 error。
        //
        // 为什么保存失败不能进 `error`：SettingsPage 的判据是 `error || !config` → 整屏被替换成
        // 「配置加载失败 + 重试」。受控输入框每键都 update，一次瞬时保存失败就会把用户**正在编辑的
        // 子页整个卸载**（已填内容随组件一起消失），且文案还说错了原因（说是加载失败）。
        // 保存失败的正确形态是「表单留在原地 + 一条可读的失败提示」，故用 toast。
        //
        // 回滚只撤**这次落盘失败的那一半**：同批进了暂存的键根本没参与这次 save，条目也还在 store 里
        // （回显仍由 `effectiveConfigOf` 给出），把它们一并回退会让开关弹回原位而暂存条上却还记着一条。
        // 磁盘副本退回 `prev` 本身即可 —— 不新建对象，引用与今天完全一致。
        latestConfig.current = prev;
        setConfig(prev);
        toast.error(
          t('common.saveFailed'),
          e instanceof Error ? e.message : String(e),
        );
        return;
      }
      // 只有真的落了盘才提示 —— 弹窗第二段承诺的是「改动已经写进配置文件」，保存失败时说这句是撒谎。
      if (restartKeys.length > 0) promptAppRestart(restartKeys, t);
    },
    [t, stagingEnabled, stage],
  );

  // 对外交出的是「用户现在应该看到的那份」= 磁盘副本 + 暂存重放。条目为空（总开关关着）时
  // `effectiveConfigOf` 返回**入参本体**，引用与渲染开销都与今天完全一致。
  return { config: effectiveConfigOf(config, stagedEntries), loading, error, update, reload };
}

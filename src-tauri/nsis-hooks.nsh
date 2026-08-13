; Polaris NSIS 卸载钩子 —— 清理「外置到 ProgramData 的提权 helper 服务」。
;
; 挂载点：tauri.conf.json 的 `bundle.windows.nsis.installerHooks`。
; 移植自 上游 `build/installer.nsh` 的 `customUnInstall`（同一失败面、同一提权手法）。
;
; ── 为什么需要它 ──
; helper 在**运行期**被外置安装到 `C:\ProgramData\Polaris`，并注册为 LocalSystem 服务 `PolarisHelper`
; （真值源：`crates/helper/src/platform/windows/mod.rs` 的 `SERVICE_NAME` / `DEFAULT_SUPPORT_DIR`）。
; 这两样都**不在 NSIS 的安装清单里**（安装器没装过它们，是 app 自己装的）⇒ Tauri 默认卸载器只删
; 安装目录、注册表卸载项与快捷方式，管不到 SCM 服务与 ProgramData ⇒ 不补此钩子，用户走「设置 /
; 控制面板 → 卸载」之后机器上会留一个**孤儿 LocalSystem 服务**常驻 + helper 二进制与 token 残留，
; 且服务名/落点用户完全不知情，无从自行清理。
;
; ── 范围：只清提权那一半，用户数据不碰 ──
; 用户数据**已由 Tauri 模板自己处理**（实证：tauri-cli 2.11.4 内嵌模板的 Section Uninstall 里
; `${If} $DeleteAppDataCheckboxState = 1 ${AndIf} $UpdateMode <> 1` → `RmDir /r "$APPDATA\${BUNDLEID}"`
; + `"$LOCALAPPDATA\${BUNDLEID}"`）。Polaris 的配置目录 `<app_config_dir>/polaris` 与更新包缓存
; `<app_cache_dir>/updates` 都落在 `com.polaris.app` 之下 ⇒ 已被那个复选框覆盖。
; 本钩子**刻意不重复删一遍** —— 那会把用户明确没勾选的数据也删掉。
; （上游的对应钩子额外清了 `%APPDATA%\上游`，那是因为 electron-builder 的
;   `deleteAppDataOnUninstall:false` 让它没有等价机制，不是本仓的情况。）
;
; ── 三条卸载路径的处置 ──
;   1. **应用内更新**（updater 以 `/UPDATE` 跑旧版卸载器）→ 整体跳过。外置 helper 与 app 解耦，
;      更新只换 app 文件、服务原样常驻；此处若动服务 = 每次更新断流 + 弹一次 UAC，正好违背外置初衷。
;   2. **控制面板 / 设置里直接卸载**（app 未参与）→ 提权一次，清服务 + ProgramData。**本钩子的唯一目标场景。**
;   3. **应用内「完全卸载」**（`runtime/uninstall.rs`）→ 它先经 helper 自卸把服务与 ProgramData 清掉，
;      再唤起本卸载器 ⇒ 届时下面的探测两条都不命中 ⇒ 跳过，**不弹第二次 UAC**。
;
; ── 提权 ──
; `installMode: currentUser` 的卸载器默认以**普通用户**运行，而 `sc delete` 与删 ProgramData 需管理员。
; 经「外层普通 PS 唤起内层提权 PS」完成（`Start-Process -Verb RunAs -Wait`），全程只弹一次 UAC。
; **best-effort**：用户取消 UAC 时退出码非 0，此处**不阻断卸载**（宁可残留，也不让卸载卡死）。
; 兜底是下次安装时 helper 安装脚本自身的幂等清理（停删同名旧服务）。

; 安装器文案 i18n：运行时按 `$LANGUAGE` 的 LCID 选简中/繁中，其余英文。
;
; 为什么用运行时判断而不是 LangString：LangString 必须在对应语言被 `MUI_LANGUAGE` 加载**之后**定义，
; 而本文件由模板在 `!include MUI2.nsh` 之后、`!insertmacro MUI_LANGUAGE` 之前 include（实证：
; tauri-cli 2.11.4 内嵌模板 `{{#if installer_hooks}} !include "{{installer_hooks}}"` 位于语言块之前）
; ⇒ 在此定义 LangString 会踩「language table 缺该 string」的编译期告警。纯数字比较不依赖任何编译期
; 语言常量，本宏在函数体内展开、届时 `$LANGUAGE` 已是当前 LCID。
;
; ⚠️ 只有三种语言：应用本体支持 5 语（en/zh-CN/zh-TW/ru/fa），但 NSIS 官方不发行波斯语 `.nlf`，
; 且本仓 `bundle.windows.nsis.languages` 目前只配了 English/SimpChinese/TradChinese。
; ru/fa 用户在此 fallback 英文 —— 这是 NSIS 侧的上限，不是本文件的疏漏。
!macro PolarisSelectLang OUT EN ZHCN ZHTW
  StrCpy ${OUT} "${EN}"
  ${If} $LANGUAGE == 2052
    StrCpy ${OUT} "${ZHCN}"
  ${ElseIf} $LANGUAGE == 1028
    StrCpy ${OUT} "${ZHTW}"
  ${EndIf}
!macroend

; 用 POSTUNINSTALL 而不是 PREUNINSTALL：本清理与 app 安装目录里的文件互不相干（helper 在
; ProgramData，不会锁住 $INSTDIR 里的任何东西），放到最后可以保证「UAC 弹窗 / 提权失败」绝不
; 干扰正常的卸载主体流程。
!macro NSIS_HOOK_POSTUNINSTALL
  ; 🔴 **编译期自曝**（不是装饰）：Tauri 模板对 hook 的插入是
  ;     `!ifmacrodef NSIS_HOOK_POSTUNINSTALL` + `!insertmacro NSIS_HOOK_POSTUNINSTALL`
  ; —— 宏名**拼错一个字母就静默跳过，且构建照常绿**，产出一个「看起来修好了、实际没有钩子」的
  ; 安装包，而这正是本文件要修的那个缺陷（卸载后留孤儿 root 服务）原样复发。
  ;
  ; 产物侧也验不了：NSIS 用 LZMA 实体压缩，字符串表进了压缩体 —— 对 setup.exe 直接 grep
  ; 连产品名 `Polaris` 都 0 命中（已做正向对照，方法本身是瞎的）。
  ;
  ; ⚠️ **下面这行 `!echo` 目前验不了任何东西**（2026-08-05 实测订正，别再照着它推结论）：
  ; 加它的初衷是「CI 日志里看得到 = 钩子插上了」。实测 run 30996066681：日志里 0 命中，但那**不构成
  ; 反证** —— 正向对照显示 `Running makensis to produce …` 之后整整 60 秒零输出，tauri-bundler
  ; 成功时根本不透传 makensis 的 stdout。通道是死的，命中与否都没有信息量。
  ;
  ; 留着它：零成本，且构建**失败**时 tauri 会把 makensis 输出打出来，届时这行仍是有用线索。
  ;
  ; 🔴 **要真正证明钩子被插入，用变异探针**：在本宏体内故意写一行非法 NSIS 指令 → 跑一次 Windows
  ; 打包腿。构建**失败**即证明宏体被展开（= 钩子确实插上了）；构建照常**成功**则说明宏根本没被插入
  ; （`!ifmacrodef` 判假），那正是本文件要防的静默失效。做完记得改回来。
  ; 之所以需要这么绕：NSIS 宏是插入点纯文本展开，**未被插入的宏体连语法都不会被检查** ——
  ; 所以「构建通过」这件事对本钩子是否存在**一个字节的信息都不提供**。
  !echo "[polaris] NSIS_HOOK_POSTUNINSTALL 已插入 —— 卸载时将清理 PolarisHelper 服务与 ProgramData"

  ; 本宏展开在 Section Uninstall 末尾。寄存器仍显式 Push/Pop 保存：宏被插进别人的 Section，
  ; 不该对「此处之后没人再读 $R4-$R9」这个当前恰好成立的事实下注。
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  Push $R8
  Push $R9

  ${If} $UpdateMode <> 1
    ; ProgramData 的绝对路径：`SetShellVarContext all` 下 `$APPDATA` 即 `C:\ProgramData`。
    ; 取完立刻还原成 current —— 不给本宏之后的任何代码留下被改过的上下文。
    SetShellVarContext all
    StrCpy $R9 "$APPDATA\Polaris"
    SetShellVarContext current

    !insertmacro PolarisSelectLang $R8 \
      "Checking Polaris privileged helper service..." \
      "检查 Polaris 提权 helper 服务..." \
      "檢查 Polaris 提權 helper 服務..."
    DetailPrint "$R8"

    ; 用 System32 绝对路径调系统命令（`$SYSDIR` = System32），不依赖 PATH ——
    ; 部分设备 PATH 缺 System32 会导致命令未找到，且可被 cwd 劫持。
    nsExec::ExecToStack '"$SYSDIR\sc.exe" query PolarisHelper'
    Pop $R4 ; 退出码：0 = 服务存在，1060 = 不存在
    Pop $R5 ; 输出（丢弃）

    ; 服务在 **或** ProgramData 目录还在 —— 两者都需要管理员才能清，任一命中就提权。
    ; 判据取「或」而不是只看服务：应用内卸载中途失败可能留下「服务已删、目录还在」的半清理态，
    ; 只看服务会漏掉它，而那个目录里躺着 helper 二进制与 token。
    ${If} $R4 == 0
    ${OrIf} ${FileExists} "$R9\*.*"
      !insertmacro PolarisSelectLang $R8 \
        "Removing PolarisHelper service and ProgramData (one admin authorization required)..." \
        "清理 PolarisHelper 服务与 ProgramData（需一次管理员授权）..." \
        "清理 PolarisHelper 服務與 ProgramData（需一次系統管理員授權）..."
      DetailPrint "$R8"

      InitPluginsDir
      ; 清理命令写进临时 .ps1，避免多层引号嵌套。`$$` 输出字面 `$`，
      ; 使 `$env:ProgramData` 留到**提权后的那个 PS 进程**里展开。
      FileOpen $R6 "$PLUGINSDIR\polaris-helper-uninstall.ps1" w
      FileWrite $R6 `& "$SYSDIR\sc.exe" stop PolarisHelper$\r$\n`
      FileWrite $R6 `Start-Sleep -Milliseconds 500$\r$\n`
      FileWrite $R6 `& "$SYSDIR\sc.exe" delete PolarisHelper$\r$\n`
      FileWrite $R6 `Start-Sleep -Milliseconds 500$\r$\n`
      FileWrite $R6 `Remove-Item -Recurse -Force -Path "$$env:ProgramData\Polaris" -ErrorAction SilentlyContinue$\r$\n`
      FileClose $R6

      ; 外层普通 PS 唤起内层提权 PS：`-Verb RunAs` 触发 UAC，`-Wait` 阻塞至清理完成，
      ; nsExec 再阻塞至外层结束 ⇒ 卸载器不会在清理还没跑完时就退出。
      nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -Command "Start-Process '$SYSDIR\WindowsPowerShell\v1.0\powershell.exe' -Verb RunAs -WindowStyle Hidden -Wait -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File','$PLUGINSDIR\polaris-helper-uninstall.ps1'"`
      Pop $R7 ; 退出码丢弃：用户取消 UAC 时非 0，best-effort 不阻断卸载
    ${EndIf}
  ${EndIf}

  Pop $R9
  Pop $R8
  Pop $R7
  Pop $R6
  Pop $R5
  Pop $R4
!macroend

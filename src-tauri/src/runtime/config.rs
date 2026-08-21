//! 配置运行时：`polaris-store` + `polaris-config-engine` 的运行时装配。
//!
//! Polaris 锚点：`main/services/ConfigManager.ts`。
//! - `loadConfig` → [`ConfigManager::load_full`]（read → sanitize → migrate → validate → 填默认 + currentConfig 缓存）
//! - `saveConfig` → [`ConfigManager::save_full`]（再跑 sanitize+validate + 原子 tmp→rename 写盘 + 刷缓存）
//! - `get(key)` / `set(key, value)` → currentConfig 投影取值 / 原地改 + 异步落盘
//!
//! 纯逻辑纪律：`store::ConfigStore` / `sanitize` / `validate` / `migrate` 全在 domain crate，
//! 本层仅注入 [`store::StdFs`]（std::fs，0o600）+ 持有 currentConfig 缓存（Polaris ConfigManager.currentConfig）。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use polaris_store::fs::{random_tmp_suffix, StdFs};
use polaris_store::{ConfigStore, LoadResult, StoreError};
use serde_json::Value;

/// [`ConfigManager::with_current`] 闭包内的**重入探针**（debug 构型，release 完全编译掉）。
///
/// # 为什么要有牙，而不是只写一行注释
///
/// `with_current` 的闭包跑在 `cache` 的**读锁**里，闭包内再碰 `ConfigManager` 就是死锁面（细节见该
/// 方法文档）。而这个坑的失效形态**极不友好**：
/// - 取写锁那条（`save_full` / `set_value`）是**必然**自死锁 —— 一写就挂，尚算显形；
/// - 取读锁那条（`current` / 嵌套 `with_current`）平时**看起来是好的** —— std 的 `RwLock` 在无写者
///   排队时递归读通常拿得到，只有「恰好有另一条腿在写配置」的那一瞬才永久阻塞。也就是说它能过
///   全部单测、过 code review、过真机冒烟，然后在用户改配置的那一刻挂死。
///
/// 靠文档防这种坑等于没防。故 debug 构型下用一个 thread-local 深度计数把「在闭包里又回来读/写配置」
/// **就地打成 panic**：坏用法在写出来的当天、在单测里就炸，而不是在生产里挂死。
/// release 构型下 [`ReentrancyProbe`] 是零字段 ZST、`enter`/`Drop` 皆空 —— 无 TLS 访问、无分支。
#[cfg(debug_assertions)]
mod reentrancy {
    use std::cell::Cell;

    thread_local! {
        /// 当前线程正处在几层 `with_current` 闭包里（>0 = 读锁在手，禁止再碰 ConfigManager）。
        static DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// 进入闭包时 +1、离开（含 panic 展开）时 -1。用 `Drop` 而非手工配对：闭包 panic 时也必须归零，
    /// 否则一个失败测试会把同线程后续所有配置读全打成 panic，故障从一条变成一片。
    pub(super) struct ReentrancyProbe;

    impl ReentrancyProbe {
        pub(super) fn enter() -> Self {
            DEPTH.with(|d| d.set(d.get() + 1));
            Self
        }
    }

    impl Drop for ReentrancyProbe {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }

    /// `ConfigManager` 每个入口开头调一次：若正在 `with_current` 闭包里 → 立刻 panic。
    pub(super) fn deny_inside_projection(entry: &str) {
        assert!(
            DEPTH.with(Cell::get) == 0,
            "ConfigManager::{entry} 在 with_current 闭包内被调用 —— 读锁正持在手上，\
             取写锁必然自死锁、递归读会在有写者排队时永久阻塞。闭包内只做纯投影，\
             把第二次配置读平铺到闭包外面。"
        );
    }
}

#[cfg(not(debug_assertions))]
mod reentrancy {
    pub(super) struct ReentrancyProbe;
    impl ReentrancyProbe {
        #[inline(always)]
        pub(super) fn enter() -> Self {
            Self
        }
    }
    #[inline(always)]
    pub(super) fn deny_inside_projection(_entry: &str) {}
}

use reentrancy::{deny_inside_projection, ReentrancyProbe};

/// [`ConfigManager::update`] 闭包的裁决：写不写盘，以及调用方要 return 的那个值。
///
/// 两个变体**都带 `R`**：「不写」在真实站点上通常是**以另一种方式成功了**
/// （净零序、无命中、内容等价），不是失败 —— 失败走 `Err(StoreError)` 或调用方自己塞进 `R`。
#[derive(Debug)]
pub enum Decision<R> {
    /// 落盘，然后把 `R` 与已落盘的配置一起还给调用方。
    Write(R),
    /// **不落盘、不广播**，直接把 `R` 还给调用方（闭包对 cfg 的改动一律丢弃）。
    Skip(R),
}

/// 配置运行时（`State`-managed，单实例）。
///
/// 持有配置目录路径 + currentConfig 缓存（`RwLock<Value>`，读多写少）。
/// FS 经 [`StdFs`]（std::fs 实现，写入 0o600）——纯逻辑 crate 的 trait 注入点。
pub struct ConfigManager {
    /// 配置目录（`<app_config_dir>/polaris/`）。
    dir: PathBuf,
    /// config.json 绝对路径。
    path: PathBuf,
    /// currentConfig 缓存（Polaris ConfigManager.currentConfig；首次 load 填充）。
    cache: RwLock<Option<Value>>,
    /// **读改写串行化锁** —— 只由 [`ConfigManager::update`] 持有，因果全在那里。
    ///
    /// **与 [`Self::cache`] 是两把互不相干的锁**，这一点是本设计成立的前提：临界区内会调
    /// `load_full`（末尾取 `cache` 写锁）与 `save_full`（先取读锁拿旧 icon id、末尾取写锁刷缓存），
    /// 若本锁与 `cache` 是同一把，那两次调用就是自死锁。
    write_lock: Mutex<()>,
}

impl ConfigManager {
    /// 新建（dir = `<app_config_dir>/polaris/`）。不立即读盘——lazy load（首次命令触发）。
    pub fn new(dir: PathBuf) -> Self {
        let path = dir.join("config.json");
        Self {
            dir,
            path,
            cache: RwLock::new(None),
            write_lock: Mutex::new(()),
        }
    }

    /// 配置目录（供其他运行时复用，如 mesh state / helper token）。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// config.json 路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 加载配置（read → sanitize → migrate → validate + 填默认），刷新 currentConfig 缓存。
    ///
    /// 维度7 #7：坏 JSON/坏字段绝不崩溃，回落默认配置；损坏的磁盘真实文件绝不覆盖。仅新装默认值
    /// 与迁移链已确认的改写会在本层 best-effort 落盘，保证带标记迁移真正一次完成。
    pub fn load_full(&self) -> Result<Value, StoreError> {
        deny_inside_projection("load_full");
        let LoadResult {
            config,
            loaded_from_disk,
            migration_delta,
            was_missing,
            error,
        } = ConfigStore::load(&StdFs, &self.path);
        // 加载或校验失败 → 回落默认（LoadResult 已处理），但记日志保留 error 上下文。
        if let Some(e) = &error {
            log::warn!("config load fallback (loaded_from_disk={loaded_from_disk}): {e}");
        }
        // 新装（文件本不存在）→ 落盘一次默认配置；迁移有改写 → 同步落盘迁移值与幂等标记。
        // 若只把迁移后的 Value 放进 cache、忽略 migration_delta，重启后还会从旧磁盘形态重复迁移；
        // 更糟的是「用户关闭预热」这类一次性默认纠偏无法证明已经完成。损坏配置的 fallback 同时满足
        // was_missing=false + migration_delta.changed=false，仍保持“不覆盖损坏原件”的安全边界。
        //
        // 第 4 参是**原子写的 12hex tmp 后缀**（`randomBytes(6).toString('hex')` 等价），
        // 不是品牌名/应用名。此处曾误传字面量 `"polaris"` → debug 撞 `tmp_path` 的
        // `debug_assert` **首启即崩**（本行正是 P0 的触发点：config.json 不存在才走到）；
        // release 下则静默产出永不被清扫的 `config.json.polaris.tmp`。
        if was_missing || migration_delta.changed {
            if let Err(e) = ConfigStore::save(&StdFs, &self.path, &config, &random_tmp_suffix()) {
                log::warn!(
                    "config load persist failed (was_missing={was_missing}, migrated={}): {e}",
                    migration_delta.changed
                );
            }
        }
        // 刷缓存（持有写锁）。
        if let Ok(mut guard) = self.cache.write() {
            *guard = Some(config.clone());
        }
        Ok(config)
    }

    /// 读 currentConfig 缓存（不触盘）。缓存未暖 → 触发一次 load_full（Polaris getCurrentConfig 懒加载）。
    ///
    /// **恒返回 owned `Value` = 恒一次整份深拷贝**（读锁只护到 clone 为止）。200 节点级配置下这不是
    /// 小数目，故**每帧 / 每 tick 调用的路径一律改用 [`with_current`](Self::with_current) 做投影**；
    /// 本方法留给「确实要整份 owned 配置」的调用点（改完要 `save_full` 的写腿、要跨 `await` 搬运给
    /// 异步任务的腿、要整份 `from_value::<UserConfig>` 的起核腿）—— 那些地方即便换成 `with_current`
    /// 也得在闭包里 clone 出整份，零收益且平白多一条闭包内禁忌。
    pub fn current(&self) -> Result<Value, StoreError> {
        deny_inside_projection("current");
        if let Ok(guard) = self.cache.read() {
            if let Some(c) = guard.as_ref() {
                return Ok(c.clone());
            }
        }
        self.load_full()
    }

    /// currentConfig 缓存的**持锁投影入口**：读锁一直持到 `f` 返回，`f` 只取它真正要的那几个字段。
    ///
    /// 与 [`current`](Self::current) 的唯一差别是**谁付整份深拷贝的账**：`current()` 恒 clone 整份配置
    /// （含 `servers` 数组与全部规则）再把 owned 值交出去；本方法一次都不 clone。缓存未暖 / 锁中毒 →
    /// 与 `current()` 同款回落：先 `load_full()` 读盘，再对结果跑 `f`（此时读锁已释放，不会撞
    /// `load_full` 内部的写锁）。
    ///
    /// # ⚠️ 闭包内禁忌（唯一、但是硬的）
    ///
    /// `f` 执行期间 `self.cache` 的**读锁是持着的**，故闭包内**禁止再调用 `ConfigManager` 的任何方法**：
    ///
    /// - `save_full` / `set_value` / `load_full` 要 `cache.write()` —— 同线程「持读锁再取写锁」是
    ///   **必然自死锁**：`std::sync::RwLock` 既不可重入也不支持读锁升级，那个 `write()` 永远等不到
    ///   自己手里的读锁释放。
    /// - `current` / `get_value` / `with_current` 只要 `cache.read()`，看似无害，**同样禁止**：std 的
    ///   `RwLock::read` 文档明写「本线程已持有该锁时可能 panic」，且在**有写者排队**时，写者优先的实现
    ///   （Linux futex 版即是）会让这次递归读**永久阻塞**。即：平时怎么测都不复现，只在「恰好有另一条
    ///   腿在写配置」的那一瞬变成死锁 —— 最难查的那类。
    ///
    /// 所以 `f` 只该做**纯投影**：从 `&Value` 取字段 → 转成 owned 值返回。不做 I/O、不回调进运行时的
    /// 其它子系统（那些子系统日后完全可能自己去读配置），也无处 `await`（本方法是同步的）。
    /// 需要「投影 + 再读一次配置」的调用点，把两次读**平铺**成先后两句，不要嵌套。
    ///
    /// 这条禁忌**在 debug 构型下是有牙的**：闭包执行期间挂着 [`reentrancy`] 探针，闭包内再调
    /// `ConfigManager` 任一入口会立刻 panic（而不是等某次「恰好有人在写配置」时挂死）。
    pub fn with_current<T>(&self, f: impl FnOnce(&Value) -> T) -> Result<T, StoreError> {
        deny_inside_projection("with_current");
        if let Ok(guard) = self.cache.read() {
            if let Some(c) = guard.as_ref() {
                let _probe = ReentrancyProbe::enter();
                return Ok(f(c));
            }
        }
        // 缓存未暖 / 锁中毒 → 读盘一次。注意读锁已随上面的 `if let` 作用域释放（`load_full` 要写锁）。
        let cfg = self.load_full()?;
        // 回落腿并不持读锁，探针仍照挂：调用方无从得知本次走了哪条腿，禁忌必须两条腿一致，
        // 否则「冷缓存下能跑、暖起来就死」是更坏的形态。
        let _probe = ReentrancyProbe::enter();
        Ok(f(&cfg))
    }

    /// 保存配置（再跑 sanitize+validate + 原子写）+ 刷缓存。上游 `saveConfig`。
    ///
    /// 顺带在此唯一汇流点做**图标缓存驱逐 reconcile**：diff 旧/新 `customAppPresets` 的 id 集，
    /// 删掉已移除自定义应用的 `<userData>/icons/<id>.*` 本地缓存。挂在这里（而非某个屏幕调 evict
    /// 命令）覆盖所有令 app id 消失的写路径（删除 / 备份整类替换 / 工厂重置），避免跨文件缝。
    /// best-effort：unlink 失败仅记日志，绝不影响配置保存本身。
    pub fn save_full(&self, config: &Value) -> Result<(), StoreError> {
        deny_inside_projection("save_full");
        // 旧 id 集须在刷缓存前从当前缓存取（此刻仍持旧配置）；缓存未暖则无旧态可 diff（冷启无删除发生）。
        let old_ids = self
            .cache
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(crate::icon_cache::custom_app_ids));
        // 同上：第 4 参是随机 12hex tmp 后缀，非品牌名。每次保存都须取新值——
        // 恒定后缀会让并发 saveConfig 撞同一个 tmp 路径，原子写的隔离性即失效。
        ConfigStore::save(&StdFs, &self.path, config, &random_tmp_suffix())?;
        // LOW-4：只有 `customAppPresets` 的 id 集**实际变化**才跑 read_dir + unlink reconcile。
        // `set_value` 走此汇流点保存**任何**键（mixedPort / 开关 / 规则…），绝大多数与自定义应用无关；
        // 无条件 reconcile 会让每次保存都白遍历一遍 `<userData>/icons/`。先比 id 集，未变即跳过整个
        // 磁盘遍历。变化时行为不变（reconcile 仅删「旧有新无」，共享 / 复用 id 保留，unlink best-effort）。
        if let Some(old) = old_ids {
            let new_ids = crate::icon_cache::custom_app_ids(config);
            if old != new_ids {
                crate::icon_cache::reconcile_removed(
                    &crate::icon_cache::icons_dir(&self.dir),
                    &old,
                    &new_ids,
                );
            }
        }
        if let Ok(mut guard) = self.cache.write() {
            *guard = Some(config.clone());
        }
        Ok(())
    }

    /// **原子读改写** —— 把「读一份配置 → 改它 → 落盘」变成一个不可分割的动作。
    ///
    /// # 它修的缺陷
    ///
    /// 本仓 29 个生产写入点全是 `load_full()` / `current()` → mutate → `save_full()` 的**分离**三步，
    /// 中间没有任何互斥。于是：
    ///
    /// - 两个写入者交错 ⇒ **丢更新**（后写的那份基于旧读，把前者的改动整份覆盖掉）；
    /// - `config_save` 的 `baseVersion` 乐观并发闸被**架空**：它在第 1 步比对版本、第 3 步才写，
    ///   任何别的写入者落在这两步之间都能让那次比对失去意义。
    ///
    /// 而这确实可达：订阅自动更新写验证器、诊断抓包恢复、热切 commit 都跑在 tokio 任务里，
    /// 与命令处理天然并发。
    ///
    /// # 闭包返回「写不写」+ **调用方自己的返回值**，而不是 `Result`
    ///
    /// 这里的形状是被全仓 30 个站点的普查逼出来的，别按直觉改回 `Result<Option<T>, E>`：
    /// 「不写」这条出口**既不唯一、也不都是错误**。逐站点数下来，读与写之间有 2–4 条不写的出口，
    /// 而其中好几条是**带不同载荷的成功**：
    ///
    /// - `server_delete_batch` 无命中 → `ApiResponse::ok(0u32)`
    /// - `rule_resources_delete` NotFound → `ApiResponse::ok(json!({…}))`
    /// - `rules_reorder` 净零序 → `ok_void()`（**刻意不 save 不广播**）
    /// - `perform_subscription_update` 内容等价 → `update_ok(0,0,0,true,…)`
    /// - `proxy.rs` 热切 commit → `false`
    ///
    /// 一个笼统的 `Ok(None)` 装不下它们；而把它们塞进 `E` 则要么得给共享 crate 加
    /// `From<StoreError> for String`，要么得借 `StoreError::Validation` 转手 —— 后者的 Display 是
    /// `"config validation failed: {0}"`，会把 `"服务器不存在: xxx"` 污染成
    /// `"config validation failed: 服务器不存在: xxx"`，是真的用户可见变化。
    ///
    /// 故：闭包返回 [`Decision<R>`]，`R` 就是调用方要 return 的那个东西（任意类型）。
    /// 读或写失败 ⇒ `Err(StoreError)`，调用方照它今天的写法映射（30 个站点今天都把读失败与写失败
    /// 收敛成同一句 `ApiResponse::err(format!("{e}"))`，故合并不丢信息）。
    ///
    /// `Write` 腿连**已落盘的那份配置**一起返回（`Some(cfg)`）：调用方拿它去
    /// `broadcast_config_changed`，不必再读一次（再读又是一次可被别人插入的窗口）。
    /// `Skip` 腿给 `None` —— **它必须不广播**：净零改动多发一次 `configChanged` 就多一次
    /// `switch_mode` 评估。
    ///
    /// # 整份替换也走这里（不需要第二个原语）
    ///
    /// 闭包拿到的是 `&mut Value`，故「整份替换」就是 `*cfg = next.clone()`
    /// （备份导入、`config_save` 落用户提交的全量配置都是这一形态）。**不要**为它另开一个跳过读的
    /// 入口：那等于又造一条不持锁的写路径，而本方法存在的全部意义就是「只剩一条」。
    ///
    /// # 读的是 `load_full`，不是 `current`（勿改）
    ///
    /// 29 个站点里 **27 个原本用的是 `load_full()`**（真重读磁盘），只有 2 个用 `current()`（缓存）。
    /// 本方法因此**自己拥有那次 `load_full`**。有人把它改成基于 `current()` 会**偷偷把不变式换成
    /// 「与缓存一致」** —— 而缓存只由本进程的写刷新，外部改动（用户手改 config.json、另一个进程）
    /// 一律看不见，于是「原子读改写」退化成「原子地基于一份可能过期的快照改写」。
    ///
    /// # 锁的边界：到落盘为止，**不含广播**
    ///
    /// 调用方必须在本方法**返回之后**才 `broadcast_config_changed`。那条广播会
    /// `spawn(switch_mode_with(...))`，而 `switch_mode` 有几条腿回读 `config.current()`；把广播圈进
    /// 临界区（或改成同步等待它）就是把一个会回读配置的调用放进持锁区间。这里是后来者最容易
    /// 好心扩大锁范围的地方。
    ///
    /// # 不重入（静态、全构建配置）
    ///
    /// `write_lock` 是私有字段、**只在本方法内被取一次**，故临界区里任何被调方都拿不到它 ——
    /// 这是构造性结论，不依赖 debug-only 的 [`reentrancy`] 探针。临界区内两次 `ConfigManager`
    /// 调用（`load_full` / `save_full`）取的是 `cache`，另一把锁，且各自作用域内取放、不跨持有。
    ///
    /// 本方法**刻意不武装**那个探针：`load_full`/`save_full` 自身都调 `deny_inside_projection`，
    /// 武装后会当场自炸。反过来，本方法**继承**了那道保护 —— 在 `with_current` 闭包里调 `update`
    /// 会由它们打成 panic，正是想要的。
    ///
    /// # 锁中毒
    ///
    /// 闭包 panic 会毒化 `write_lock`；此处**恢复**而非传播。落盘是原子的（tmp→rename），
    /// 一次 panic 留下的要么是完整旧文件要么是完整新文件，没有撕裂态；而让一次闭包 panic
    /// 永久锁死此后所有配置写入，比那次 panic 本身糟得多。
    pub fn update<R>(
        &self,
        f: impl FnOnce(&mut Value) -> Decision<R>,
    ) -> Result<(R, Option<Value>), StoreError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cfg = self.load_full()?;
        match f(&mut cfg) {
            // 不写：闭包对 `cfg` 的改动一律丢弃（它拿的是本方法的局部副本）。
            Decision::Skip(r) => Ok((r, None)),
            Decision::Write(r) => {
                self.save_full(&cfg)?;
                Ok((r, Some(cfg)))
            }
        }
    }

    /// 取单键（currentConfig 投影）。上游 `configManager.get(key)`。
    ///
    /// Polaris 的 get 支持 dotted path（如 'servers'）；此处投影顶层键（与 Polaris ConfigManager.get
    /// 主路径一致，复杂路径交由渲染端处理）。
    pub fn get_value(&self, key: &str) -> Result<Value, StoreError> {
        deny_inside_projection("get_value");
        let cfg = self.current()?;
        Ok(cfg.get(key).cloned().unwrap_or(Value::Null))
    }

    /// 置单键（currentConfig 原地改 + 落盘 + 广播由调用方触发）。上游 `configManager.set(key, value)`。
    pub fn set_value(&self, key: &str, value: Value) -> Result<Value, StoreError> {
        deny_inside_projection("set_value");
        let mut cfg = self.current()?;
        // 原地替换 / 插入顶层键。
        if let Some(obj) = cfg.as_object_mut() {
            obj.insert(key.to_string(), value);
        }
        self.save_full(&cfg)?;
        Ok(cfg)
    }

    /// 取配置目录下某子路径（mesh state / helper token 等复用）。
    #[must_use]
    pub fn join(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.dir.join(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "polaris-config-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// **P0 复现路径**：config.json 不存在（首次启动/新装）→ `load_full` 必须把默认配置写盘。
    ///
    /// 修复前此处传字面量 `"polaris"` 作 12hex tmp 后缀 → `store::fs::tmp_path` 的
    /// `debug_assert!` 触发 → **debug 构型直接 abort**（非 unwind，`#[should_panic]` 都接不住），
    /// 即「首启必崩」；release 则写出永不被清扫的 `config.json.polaris.tmp`。
    #[test]
    fn load_full_on_missing_config_writes_default_to_disk() {
        let dir = temp_dir("missing");
        let path = dir.join("config.json");
        assert!(
            !path.exists(),
            "前提：config.json 必须不存在（这才是新装路径）"
        );

        let mgr = ConfigManager::new(dir.clone());
        let cfg = mgr.load_full().expect("新装路径 load_full 应成功");

        // ① 真的落盘了（was_missing 腿跑通）。
        assert!(
            path.exists(),
            "新装必须把默认配置写盘（P0：此前这一步在 debug 下 abort）"
        );
        // ② 落盘内容是合法 JSON 且与返回值一致。
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .expect("落盘内容应是合法 JSON");
        assert!(on_disk.is_object(), "落盘配置应是 JSON 对象");
        assert!(cfg.is_object());
        // ③ tmp→rename 已完成：目录里不得残留任何 .tmp（尤其不得有 config.json.polaris.tmp）。
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "原子写后不得残留 tmp，实得 {leftovers:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 二次 load（文件已在）不得重写盘，且缓存命中。
    #[test]
    fn load_full_is_idempotent_and_does_not_leave_tmp() {
        let dir = temp_dir("idem");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();
        let first = std::fs::read_to_string(dir.join("config.json")).unwrap();
        mgr.load_full().unwrap();
        let second = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert_eq!(first, second, "二次 load 不应改变磁盘内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迁移不能只存在于进程内 cache：带标记的一次性默认纠偏必须在首次 load 后立即落盘；用户之后
    /// 显式改回 false 时，新进程读取到标记并尊重该值。
    #[test]
    fn load_full_persists_migration_marker_then_respects_user_value() {
        let dir = temp_dir("migration-persist");
        let path = dir.join("config.json");
        let mut legacy = polaris_store::default_config();
        legacy["keepTrayMenuWarm"] = Value::Bool(false);
        legacy
            .as_object_mut()
            .unwrap()
            .remove("keepTrayMenuWarmDefaultMigrated");
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let mgr = ConfigManager::new(dir.clone());
        let migrated = mgr.load_full().expect("升级配置应成功迁移");
        assert_eq!(migrated["keepTrayMenuWarm"], true);
        assert_eq!(migrated["keepTrayMenuWarmDefaultMigrated"], true);

        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .expect("迁移结果应立即落为合法 JSON");
        assert_eq!(on_disk["keepTrayMenuWarm"], true);
        assert_eq!(on_disk["keepTrayMenuWarmDefaultMigrated"], true);

        mgr.set_value("keepTrayMenuWarm", Value::Bool(false))
            .expect("用户关闭预热应落盘");
        let restarted = ConfigManager::new(dir.clone())
            .load_full()
            .expect("二次启动应读取已标记配置");
        assert_eq!(restarted["keepTrayMenuWarm"], false);
        assert_eq!(restarted["keepTrayMenuWarmDefaultMigrated"], true);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Write` / `Skip` 两条腿各走一遍。
    ///
    /// 变异对照：把 `Skip` 腿改成照样 `save_full` → 「不写」那两条断言转红（真实后果是净零改动
    /// 多一次广播 + 多一次 `switch_mode`）；把 `Skip` 腿改成回传 `Some(cfg)` → 「必须不回传配置」
    /// 转红（调用方正是据它决定要不要广播）。
    #[test]
    fn update_writes_or_skips_and_never_leaks_skipped_edits() {
        let dir = temp_dir("update-outcomes");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();

        // ① 正常改动：落盘 + 把值和已落盘那份一起带回。
        let (tag, saved) = mgr
            .update(|cfg| {
                cfg.as_object_mut()
                    .unwrap()
                    .insert("mixedPort".into(), Value::from(7801u64));
                Decision::Write("created-id")
            })
            .expect("不该报错");
        assert_eq!(tag, "created-id", "闭包算出的值必须能带出来给调用方");
        assert_eq!(
            saved.expect("Write 腿必须回传已落盘那份（调用方要拿它去锁外广播）")["mixedPort"],
            7801
        );
        assert_eq!(mgr.current().unwrap()["mixedPort"], 7801);

        // ② `Skip`：不是错误、**一个字节都不该写**，且不回传配置（回传就等于邀请调用方去广播）。
        let (payload, skipped) = mgr
            .update(|cfg| {
                cfg.as_object_mut()
                    .unwrap()
                    .insert("mixedPort".into(), Value::from(9999u64));
                // 真实形态：净零改动照样要把「成功」的载荷还给调用方（如 `ApiResponse::ok(0u32)`）。
                Decision::Skip(0u32)
            })
            .expect("跳过不是错误");
        assert_eq!(payload, 0, "Skip 腿同样要能带出调用方的返回值");
        assert!(
            skipped.is_none(),
            "Skip 腿必须不回传配置 —— 回传就等于让调用方多广播一次 configChanged"
        );
        assert_eq!(
            mgr.current().unwrap()["mixedPort"],
            7801,
            "Skip 必须不写 —— 闭包对 cfg 的改动一律丢弃"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **不丢更新** —— 本原语存在的全部理由。
    ///
    /// 两个线程各自把 `mixedPort` 加 1。分离三步（`load_full` → mutate → `save_full`）下二者会读到
    /// 同一个初值、后写的整份覆盖前者 ⇒ 终值 +1（丢了一次）。原子后必为 +2。
    ///
    /// 闭包里那次 `sleep` 是**逼出交错**用的：没有它，两个线程可能天然错开而让本条在无锁实现下
    /// 也偶然变绿（那种门等于没门）。
    ///
    /// 变异对照（实跑）：把 `update` 体内的 `write_lock.lock()` 那三行删掉 → 终值 7801 而非 7802，转红。
    #[test]
    fn concurrent_updates_do_not_lose_each_other() {
        use std::sync::Arc;
        let dir = temp_dir("update-concurrent");
        let mgr = Arc::new(ConfigManager::new(dir.clone()));
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), Value::from(7800u64));
        mgr.save_full(&cfg).unwrap();

        let bump = |m: Arc<ConfigManager>| {
            std::thread::spawn(move || {
                m.update(|c| {
                    let cur = c["mixedPort"].as_u64().unwrap();
                    // 持锁期间睡一下，逼出「两个线程都已读、都还没写」这个交错。
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    c.as_object_mut()
                        .unwrap()
                        .insert("mixedPort".into(), Value::from(cur + 1));
                    Decision::Write(())
                })
                .expect("update 不该失败");
            })
        };
        let a = bump(Arc::clone(&mgr));
        let b = bump(Arc::clone(&mgr));
        a.join().unwrap();
        b.join().unwrap();

        assert_eq!(
            mgr.current().unwrap()["mixedPort"],
            7802,
            "两次 +1 必须都在；7801 = 有一次被整份覆盖掉了（丢更新）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **起核载荷新鲜的地基**：`save_full` 之后 `current()` 必须立刻返回新值。
    ///
    /// `proxy_start` / `proxy_restart` 改成用 `state.config().current()` 取起核配置之后，
    /// 「写盘 → 立刻点启动用的是写后的配置」这条用户可见承诺**全部押在这条性质上**：
    /// 若 `current()` 还回旧缓存，起核就仍会用写之前那份 —— 与改之前的缺陷一模一样，
    /// 只是从「渲染端副本陈旧」换成「后端缓存陈旧」。
    ///
    /// 变异对照：删掉 `save_full` 末尾那段刷缓存（`*guard = Some(config.clone())`）→ 本条转红。
    #[test]
    fn current_reflects_the_write_immediately_after_save_full() {
        let dir = temp_dir("current-after-save");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), Value::from(7899u64));
        mgr.save_full(&cfg).expect("save_full 应成功");
        assert_eq!(
            mgr.current().expect("current 应可读")["mixedPort"],
            7899,
            "save_full 之后 current() 必须已是新值 —— 起核载荷就是从这里取的"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `save_full` 每次都取新 tmp 后缀 → 连续保存不得残留 tmp、内容以最后一次为准。
    #[test]
    fn save_full_persists_and_leaves_no_tmp() {
        let dir = temp_dir("save");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        for port in [7890u64, 7891, 7892] {
            cfg.as_object_mut()
                .unwrap()
                .insert("mixedPort".into(), Value::from(port));
            mgr.save_full(&cfg).expect("save_full 应成功");
        }
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk["mixedPort"], 7892, "应落最后一次保存的值");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "连续保存不得残留 tmp，实得 {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LOW-4：`customAppPresets` 未变的保存不得驱逐仍在册应用的缓存图标（改无关键仍保留图标）。
    #[test]
    fn save_full_keeps_icon_when_presets_unchanged() {
        let dir = temp_dir("icon-keep");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut().unwrap().insert(
            "customAppPresets".into(),
            serde_json::json!([{ "id": "custom-keep", "name": "K" }]),
        );
        mgr.save_full(&cfg).unwrap(); // 暖缓存：旧集 = {custom-keep}
        let icons = crate::icon_cache::icons_dir(&dir);
        crate::icon_cache::write_icon(&icons, "custom-keep", "png", b"\x89PNG").unwrap();
        // 改无关键（customAppPresets 不动）→ id 集未变 → reconcile 跳过 → 图标须存活。
        cfg.as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), Value::from(7890u64));
        mgr.save_full(&cfg).unwrap();
        assert!(
            icons.join("custom-keep.png").exists(),
            "preset 未变时不得驱逐仍在册图标"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `with_current` 的投影结论必须与 `current()` 的整份快照**逐字段一致**（缓存已暖路径）。
    ///
    /// 这条是把 `current()` 换成 `with_current` 的等价性根据：换的是「谁付深拷贝」，不是读到的内容。
    /// **变异锁**：把 `with_current` 实现成「先 `load_full()` 再跑 `f`」（即忽略缓存）→ 本测仍绿，
    /// 但 `with_current_does_not_touch_disk_when_cache_is_warm` 转红。
    #[test]
    fn with_current_projection_matches_current_snapshot() {
        let dir = temp_dir("with-current-eq");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), Value::from(7899u64));
        cfg.as_object_mut()
            .unwrap()
            .insert("selectedServerId".into(), Value::from("srv-1"));
        mgr.save_full(&cfg).unwrap();

        let snapshot = mgr.current().unwrap();
        let projected = mgr
            .with_current(|v| {
                (
                    v.get("mixedPort").cloned(),
                    v.get("selectedServerId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                )
            })
            .unwrap();
        assert_eq!(projected.0.as_ref(), snapshot.get("mixedPort"));
        assert_eq!(projected.1.as_deref(), Some("srv-1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存已暖时 `with_current` **不得触盘**：把盘上文件删掉后仍能投影出内存里的值。
    ///
    /// 这正是热路径（STATUS 每帧 / 心跳每 tick）走它的前提 —— 若它退化成每次 `load_full`，
    /// 省掉的深拷贝会被一次磁盘读 + sanitize + validate 换成更贵的开销。
    #[test]
    fn with_current_does_not_touch_disk_when_cache_is_warm() {
        let dir = temp_dir("with-current-warm");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut()
            .unwrap()
            .insert("mixedPort".into(), Value::from(7123u64));
        mgr.save_full(&cfg).unwrap(); // 暖缓存
        std::fs::remove_file(dir.join("config.json")).unwrap();

        let port = mgr.with_current(|v| v.get("mixedPort").cloned()).unwrap();
        assert_eq!(
            port,
            Some(Value::from(7123u64)),
            "缓存暖时必须走内存投影，不得回落读盘"
        );
        assert!(
            !dir.join("config.json").exists(),
            "缓存暖路径不得因一次投影就把默认配置写回磁盘"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 缓存**未暖**（冷启首次读）时 `with_current` 必须回落 `load_full` 并对读到的配置跑投影。
    #[test]
    fn with_current_falls_back_to_load_when_cache_is_cold() {
        let dir = temp_dir("with-current-cold");
        // 先用一个实例把非默认值落盘，再用**全新实例**（缓存冷）读。
        {
            let seed = ConfigManager::new(dir.clone());
            let mut cfg = seed.load_full().unwrap();
            cfg.as_object_mut()
                .unwrap()
                .insert("mixedPort".into(), Value::from(7456u64));
            seed.save_full(&cfg).unwrap();
        }
        let cold = ConfigManager::new(dir.clone());
        let port = cold
            .with_current(|v| v.get("mixedPort").and_then(Value::as_u64))
            .unwrap();
        assert_eq!(port, Some(7456), "冷缓存须经 load_full 读到盘上的值");
        // 回落腿也须顺带暖上缓存（与 `current()` 同款懒加载语义）。
        assert_eq!(
            cold.current()
                .unwrap()
                .get("mixedPort")
                .and_then(Value::as_u64),
            Some(7456)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **读锁必须在 `with_current` 返回前释放**：返回后紧接着的写腿（`set_value` 要 `cache.write()`）
    /// 不得阻塞。
    ///
    /// 用带超时的独立线程跑，是因为「锁没释放」的失败形态是**永久阻塞**而不是断言失败 —— 直接在测试
    /// 线程里跑会把整个 `cargo test` 挂死（看起来像 CI 卡住，而不是一条红测）。
    ///
    /// **变异锁**：把实现改成「读锁跨越 `load_full`」（即把 `f(c)` 与后续写腿放进同一个 guard 作用域）
    /// → 本测超时转红。
    #[test]
    fn with_current_releases_read_lock_before_returning() {
        use std::sync::mpsc;
        let dir = temp_dir("with-current-unlock");
        let mgr = std::sync::Arc::new(ConfigManager::new(dir.clone()));
        mgr.load_full().unwrap();

        let (tx, rx) = mpsc::channel();
        let m = std::sync::Arc::clone(&mgr);
        let h = std::thread::spawn(move || {
            let seen = m.with_current(|v| v.get("mixedPort").cloned());
            // 紧接着取写锁：若上面的读锁被 `with_current` 带出来了，这里同线程自死锁。
            let wrote = m.set_value("mixedPort", Value::from(7001u64)).is_ok();
            let _ = tx.send(seen.is_ok() && wrote);
        });
        let ok = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("with_current 返回后写腿必须立刻可取写锁（超时 = 读锁被带出闭包）");
        assert!(ok, "投影与随后的写入都应成功");
        h.join().unwrap();
        assert_eq!(
            mgr.current()
                .unwrap()
                .get("mixedPort")
                .and_then(Value::as_u64),
            Some(7001)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── with_current 闭包禁忌的「牙」（debug 构型探针）─────────────────────────────
    //
    // 三条测试穷举闭包内**可能**回到 ConfigManager 的三种形态（读 / 写 / 嵌套投影）—— 只测一条会被
    // 「只在 current() 上加探针」这种半吊子修法蒙混过关。三条都不能靠真死锁来验（那是挂死不是红测），
    // 故判据是探针 panic。

    /// 闭包内调 `current()`（**读**腿）→ 探针 panic。
    ///
    /// 这条是三条里最危险的：不加探针时它**平时不显形**（无写者排队 → 递归读通常拿得到），
    /// 只在恰好有另一条腿写配置时永久阻塞。
    ///
    /// **变异锁**：删掉 `with_current` 里的 `ReentrancyProbe::enter()`，或删掉 `current()` 开头的
    /// `deny_inside_projection` → 本测（无 panic）转红。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "with_current 闭包内被调用")]
    fn nested_current_inside_projection_panics_in_debug() {
        let dir = temp_dir("reentrancy-read");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();
        let _ = mgr.with_current(|_| mgr.current());
    }

    /// 闭包内调 `set_value()`（**写**腿）→ 探针 panic。不加探针时这条是**必然自死锁**。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "with_current 闭包内被调用")]
    fn nested_write_inside_projection_panics_in_debug() {
        let dir = temp_dir("reentrancy-write");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();
        let _ = mgr.with_current(|_| mgr.set_value("mixedPort", Value::from(1u64)));
    }

    /// 闭包内**嵌套** `with_current` → 同样 panic（自己人也不例外）。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "with_current 闭包内被调用")]
    fn nested_with_current_inside_projection_panics_in_debug() {
        let dir = temp_dir("reentrancy-nested");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();
        let _ = mgr.with_current(|_| mgr.with_current(|v| v.is_object()));
    }

    /// 探针必须**随闭包退场归零**——含闭包 panic 的退场。否则一次失败测试会把同线程后续所有配置读
    /// 全打成 panic：故障从一条变成一片，而真正的根因被淹没。
    ///
    /// **变异锁**：把 `ReentrancyProbe` 的 `Drop` 换成手工「闭包返回后 -1」→ panic 腿不再归零，
    /// 本测最后那次 `current()` 转红。
    #[cfg(debug_assertions)]
    #[test]
    fn probe_depth_unwinds_on_closure_panic() {
        let dir = temp_dir("reentrancy-unwind");
        let mgr = ConfigManager::new(dir.clone());
        mgr.load_full().unwrap();
        let boom = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = mgr.with_current(|_| panic!("闭包内业务 panic"));
        }));
        assert!(boom.is_err(), "前提：闭包确实 panic 了");
        // 探针归零 ⇒ 后续正常读写照常可用（不归零则这两行 panic）。
        assert!(mgr.current().is_ok(), "闭包 panic 后深度必须已归零");
        assert!(mgr.with_current(|v| v.is_object()).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// LOW-4：id 集**实际变化**（移除某 preset）时 reconcile 照跑——被移除项图标驱逐、仍在册保留。
    #[test]
    fn save_full_reconciles_icons_when_preset_removed() {
        let dir = temp_dir("icon-evict");
        let mgr = ConfigManager::new(dir.clone());
        let mut cfg = mgr.load_full().unwrap();
        cfg.as_object_mut().unwrap().insert(
            "customAppPresets".into(),
            serde_json::json!([{ "id": "custom-keep" }, { "id": "custom-drop" }]),
        );
        mgr.save_full(&cfg).unwrap();
        let icons = crate::icon_cache::icons_dir(&dir);
        crate::icon_cache::write_icon(&icons, "custom-keep", "png", b"\x89PNG").unwrap();
        crate::icon_cache::write_icon(&icons, "custom-drop", "png", b"\x89PNG").unwrap();
        // 移除 custom-drop → id 集变化 → reconcile 跑。
        cfg.as_object_mut().unwrap().insert(
            "customAppPresets".into(),
            serde_json::json!([{ "id": "custom-keep" }]),
        );
        mgr.save_full(&cfg).unwrap();
        assert!(icons.join("custom-keep.png").exists(), "仍在册图标须保留");
        assert!(!icons.join("custom-drop.png").exists(), "移除项图标须驱逐");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

# dd-mrm-rs 迁移计划

> DdModTool（C# WPF）→ Rust + slint 重写。
> 制定：2026-11-04。状态：已完成（M0-M4 全绿，2026-11-05）。
> 知识底座：[docs/](docs/)（需求 / DSON 规格+bit31 / 逆向事实 / 热修改设计）。

## 1. 背景

旧项目 DdModTool（C# WPF，minimal-fix 分支）已达成两个硬成果：

1. **DSON 保真写入**：bit31 逐位保留（树路径匹配算法）、原子写、同内容重写字节
   一致；
2. **热修改 v2.1**（native DLL 引擎）：2026-11-04 用户实机确认成功——多轮、清空、
   换档、护栏拒绝全过，退出写回无还原无崩溃。

重写动机：三种数据表示（bytes→JsonElement DOM→对象，bit31 逻辑埋在 DOM 遍历里）、
零 API 边界（Core 全 internal，测试靠内联编译+反射）、`Gui/Utils` 十一职责垃圾抽屉、
`IndexViewModel` 665 行 god object、引擎资产混在 GUI 源码树靠 `../../../` 回退查找、
`GetAllMods()` 引用陷阱。

决策：Rust 重写；UI 框架 **slint**（内存/存储开销理由）；保持三列表骨架；产品
二进制与配置用 **dd-mrm** 名（不用 ddmodtool）；在全新仓库（本目录）开展。

## 2. 架构

```
dd-mrm-rs (workspace)
├── crates/dson             # 纯 DSON 编解码（lib + dsoncli bin）—— 无 I/O、无游戏知识
├── crates/save             # 领域服务：找档/原子写/备份/预设/mod 发现 —— 依赖 dson
├── crates/hotapply         # 注入器+管道客户端（windows API）—— 独立，无 dson 依赖
├── crates/ui-main          # slint UI，产品二进制 dd-mrm.exe —— 依赖 save + hotapply
├── native/                 # DdHotapply.dll 源（C，保持不重写）+ build/smoke 脚本
└── tests/golden/           # 金样本（3 个真实 persist.game.json）
```

依赖方向严格单向：`dson ← save ← ui-main → hotapply`。共享数据 `ModEntry{title,
version}` 在边界两侧各自定义（hotapply 不反向依赖 dson/save）。

**DLL 不重写**：注入+帧钩+游戏函数调用的安全内核已在实机上验证，C 代码保持原样
（`native/build.ps1` 构建，`cl /MT + ml64`），作为黑盒被 `hotapply` 消费。

## 3. Global Constraints（全局约束；任务与审查以此为准）

- **GC1** 依赖方向严格单向：`dson ← save ← ui-main → hotapply`；共享数据
  `ModEntry{title,version}` 在边界两侧各自定义（hotapply 不依赖 dson/save）。
- **GC2** DLL 不重写：`native/` 保持 C 源，作为黑盒被 `hotapply` 消费。
- **GC3** 保真优先：bit31 逐位保留；未知结构或白名单外文件名 → 拒绝写并报错，
  永不静默降级。
- **GC4** 写盘一律原子（`.tmp` + rename）；写入口径只针对 `persist.game.json` 家族。
- **GC5** `dson` 库零第三方依赖（std only）；dsoncli 的路径由调用者传入，
  不做 Steam 目录发现。
- **GC6** 字节验收基准：金样本同内容重写 diff=0；与旧 C# 工具对照的验收，以预先
  从旧工具生成并提交入库的期望文件为准（测试不调用旧工具）。
- **GC7** 产品二进制 `dd-mrm.exe`，CLI 名 `dsoncli`，crate 名 dson/save/hotapply/
  ui-main；管道名沿用 `\\.\pipe\DdModToolHotapply`。
- **GC8** M5 中"新旧并跑≥1周 / 旧仓库转只读"是用户侧运维动作，不属代码任务；
  本计划代码任务止于发布打包与文档。

## 4. 资产迁移映射

| 旧资产 | 去向 | 状态 |
|---|---|---|
| robojumper dson.md | `docs/dson-spec-robojumper.md`（上游副本） | ✅ 已迁 |
| bit31/树路径匹配/写入算法 | `docs/dson-bit31-notes.md`（M1 实现规范） | ✅ 已迁 |
| RE 事实（三定律/信封结构/状态机/地址表） | `docs/game-re-facts.md` | ✅ 已迁 |
| 热修改 v2.1 设计+协议+运维教训 | `docs/hotapply-design.md` | ✅ 已迁 |
| 需求/验收基准 | `docs/requirements.md` | ✅ 已迁 |
| native DLL 四源文件 | `native/` | ✅ 已迁 |
| 金样本 profile_2(2504B)/profile_7(5184B)/游戏mod管理器自写(4716B) | `tests/golden/` | ✅ 已迁 |
| 旧 C# 写入器行为 | 只作对照参考（M1/M2 验收=与旧输出 diff 为零），代码不迁 | — |
| frida 引擎/中间产物/失败探索 | 不迁 | ✂️ 弃 |

## 5. 任务（原里程碑；每步验收过才进下一步）

### Task 1: M0 脚手架（workspace 四 crate 空壳）
workspace 四 crate 空壳编译通过；`cargo build -p dson` 只编 dson（验证
双 target 隔离）；`dsoncli --help` 可跑。
**验收**：`cargo test` 全绿；`cargo build -p dson` 不触发其他 crate。

### Task 2: M1 dson（编解码核心）
- 解析：DSON → 内存模型（单一表示，杜绝旧项目三种表示问题）；dump（人读结构）。
- 写入：按 dson-bit31-notes.md 实现 bit31 树路径匹配 + 容器 dynamicMode 规则 +
  新条目启发式；对齐/字符串惯例逐字节复刻。
- dsoncli：parse / dump / rewrite / roundtrip（roundtrip=自校验字节一致）。
**验收**：①金样本×3 同内容重写字节一致 ②删末位 mod 重写=最小结构差 ③bit31 逐位
保留与旧工具输出 diff=0 ④白名单外文件名拒绝 ⑤未知结构拒绝写并报错。

### Task 3: M2 save（领域服务）
找档（配置文件路径，首版不自动发现）、原子写（temp+rename）、备份（SaveBak）、
预设、mod 扫描（游戏目录+创意工坊）、离线应用组合服务。
**验收**：离线应用三场景（无改动/加一个/清空）输出与旧工具**逐字节一致**。

### Task 4: M3 hotapply（注入与管道）
windows crate 实现注入器（含分段失败 Detail：no-process/virtualalloc-err/wpm-err/
thread-err/inject-timeout/loadlibrary-returned-0）+ 管道客户端（协议见
hotapply-design.md §4-5）；verify-inject Rust 版（notepad 注入断言日志）。
**验收**：notepad 冒烟过 → 实机 T1-T7（§6 测试表）全过 → 用户确认。

### Task 5: M4 ui-main（slint UI）
三列表骨架 + 成熟模式重设计：master-detail 布局、脏状态与确认、异步操作不冻结
UI、状态栏四态、键盘可达、深浅色跟随。配置 `dd-mrm.json`。
**验收**：requirements.md FR1-FR9 全走通；用户日常操作体验认可。

### Task 6: M5 切换与收尾（发布打包与文档）
新旧并跑 ≥1 周（旧工具只读备份用途；见 GC8）→ 旧仓库转只读归档；发布打包（exe + DLL）。

## 6. 测试策略

- **金样本属性测试**（每 `cargo test` 跑）：字节一致 / 最小差 / bit31 保留。
- **对照测试**：M2 阶段与旧工具输出 diff（旧工具=行为基准，只读引用）。
- **DLL 冒烟**：notepad 宿主装入+模式校验+干净退出（`native/smoke.ps1` 迁移沿用）。
- **实机协议测试**（用户配合，沿用旧规程）：

| # | 场景 | 期望 |
|---|---|---|
| T1 | 列表停留，单加 mod 应用 | ✅ persisted=1，进档生效 |
| T2 | 连续多轮改（加/删/换组合） | 每轮 ✅，以最后为准 |
| T3 | 清空全部 mod | ✅ count=0；字段消失可容忍（已验证） |
| T4 | 换档应用 | ✅ 对应信封 |
| T5 | 战役内/标题屏应用 | 状态栏 not-at-list，拒绝不写 |
| T6 | 游戏未运行应用 | 离线写盘，下次启动生效 |
| T7 | 游戏内已注入换 DLL | 拒装/提示重启游戏（DLL 驻留） |

## 7. 风险与未决

| 风险 | 处置 |
|---|---|
| slint 许可证（GPLv3 / Royalty-Free / 商业三选） | 发布前定；个人使用无碍，M4 前查证 |
| DSON 类型推断边界（Char/Float/IntVector 靠字段名硬编码） | 文件名白名单护栏，只碰 persist.game.json 家族 |
| 游戏更新改 RVA | FRAME_PATTERN 校验拒装 + 文档锁定 MD5 d1bd550c…；新版需重逆 |
| DLL 构建依赖 VS 工具链 | build.ps1 自检 cl/ml64；产物入库省用户构建 |
| bit31 新条目启发式归纳面窄 | 未知结构拒绝写并报告（保真优先于便利） |
| Steam 路径硬编码 | 首版配置文件；发现功能后置 |

## 8. 文档索引

| 文件 | 内容 |
|---|---|
| [docs/requirements.md](docs/requirements.md) | 需求/验收基准 |
| [docs/dson-spec-robojumper.md](docs/dson-spec-robojumper.md) | 上游 DSON 规格（副本） |
| [docs/dson-bit31-notes.md](docs/dson-bit31-notes.md) | bit31 与保真写入规范 |
| [docs/game-re-facts.md](docs/game-re-facts.md) | 逆向事实（三定律/内存布局/地址表） |
| [docs/hotapply-design.md](docs/hotapply-design.md) | 热修改 v2.1 设计+协议+运维 |
| native/ | DLL 源（C）+ 构建/冒烟脚本 |
| tests/golden/ | 金样本×3 |

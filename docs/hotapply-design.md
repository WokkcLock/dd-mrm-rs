# 热修改方案设计（v2.1 native，已实测成功）

> 状态：2026-11-04 用户实机确认成功（6 轮应用：加/清空/换组合，全部 persisted=1，
> 退出写回无还原无崩溃）。本文是该方案的完整设计记录，dd-mrm-rs 迁移的基准。
> 依据见 [game-re-facts.md](game-re-facts.md)。

## 1. 目标与用户约束

- 游戏运行中改存档 mod 列表，不重启游戏；**一个档在一个游戏进程生命周期内可改
  多次**。
- 操作流程允许给用户规定（"退到存档列表再改"），但不麻烦、不关游戏。
- 禁止异步监视进程/轮询；行为同步、可预期。
- 崩溃零容忍：任何校验不过就拒绝并报告，绝不盲写。

## 2. 组成

```
dd-mrm (UI, slint)                    Darkest.exe
  │ 应用按钮                              │
  ▼                                      │
dd-mrm-hotapply (Rust 库)                │
  ├─ 注入器: OpenProcess→VirtualAllocEx→ │
  │   WriteProcessMemory(路径)→          │
  │   CreateRemoteThread(LoadLibraryW)   │──DLL 装入──▶ DdHotapply.dll (C)
  └─ 管道客户端 ═\\.\pipe\DdModToolHotapply══▶│  ├─ 帧钩(入口模式校验)
      (JSON 行协议)                      │  ├─ 应用执行器(帧间隙跑)
                                          │  └─ 命名管道服务器
```

（管道名沿用现名 `DdModToolHotapply`——native/DdHotapply.c 源码即此名；如改名须
两侧同步，见 §4。）

- DLL（`native/DdHotapply.c` + `thunk64.asm`）**保持 C**，不随 UI 重写；MSVC
  `cl /MT` + `ml64` 构建（`native/build.ps1`），无 CRT 外依赖。
- 注入按需发生（点"应用"且游戏在跑时），无守护进程、无开机挂钩。

## 3. 应用流程（DLL 侧）

1. 帧函数（0x3D93C0）入口钩子：先比对 16 字节入口模式（FRAME_PATTERN），不符 =
   版本不对，拒装/拒绝执行。
2. 收到 apply 请求后在**帧间隙**（主线程、渲染/加载逻辑之间）执行：
   a. 读列表屏指针 0x14117D8B0 → 校验非空、页可读写 → 信封数组 +5992。
   b. 遍历 9 信封找目标档：信封名 char[32] **前缀匹配** `profile_N` 或
      `steam_profile_N`（名字不保证 NUL 结尾，尾部可能有堆脏字节——曾因此误判
      envelope-not-found，教训）。
   c. 定位信封 PG（+328）→ applied 容器（+1088，vector 在容器 +520/+528）。
   d. 清空 vector（begin=end；cap 保留），逐条在栈上拼 192B
      `{title[128], version[64]}`（UTF-8 + NUL）调 **fPush**（0x562920）。
      begin 为 NULL 时游戏会分配新块——推完**重读 begin** 再算 count。
   e. 调 **fPersist**（0x40F5B0，参数=列表屏对象+槽位）→ 游戏自己把信封写回盘。
      落盘者是游戏 → FS 缓存天然一致（历史崩溃根因被结构性消除，见
      game-re-facts.md §4）。
3. 回包 `{ok, state, count, first}`；失败态见 §5。

护栏：页属性校验（VirtualQuery）、信封计数=9、条目数上限、帧钩活性心跳、非战役
态（has-campaign/env+325 相关守卫在 fPersist 内部亦有）。

## 4. 管道协议（JSON 行，UTF-8）

> **勘误（2026-11-04，T4 对照 native/DdHotapply.c 核实）**：协议权威 =
> `native/DdHotapply.c`。本文 §4-5 为早期设计稿，与实现有三处出入：
> ① 响应**无 `first` 字段**（仅日志行有）；② `count` 只在 `ok=true` 时出现（含
> `"count":0`）；③ §5 状态表不全——DLL 实际还有 `applied-no-persist` /
> `apply-exception` / `no-envelope-array` / `bad-args` / `bad-cmd` / `ready`，
> 工具侧另有 `engine-off:<detail>`（engine-not-ready 是旧工具侧概念）与
> `bad-entry` / `entry-too-long` / `too-many-entries` / `line-too-large` /
> `timeout` / `dll-not-found`。完整清单以 task-4-report.md 为准。

请求：`{"id":N, "cmd":"apply", "path":"<存档文件绝对路径>",
"entries":[{"title":"...","version":"..."}, ...]}`
响应：`{"id":N, "ok":bool, "state":"...", "count":N, "first":N}`

（title/version 即 192B 条目两段；path 仅用于定位 profile 名，DLL 不读盘。）
另有 `ping`/`fixsize`（FS 缓存纠偏，fPersist 方案下已非必需，保留为可选护栏）。

## 5. 状态与失败态

`applied`（含 persisted=true）/ `not-at-list`（战役内/标题屏，信封不在）/
`game-not-running`（UI 侧判定，走离线写盘）/ `engine-not-ready`（注入失败或管道
不通，Detail 需带失败段：no-process / virtualalloc-errN / wpm-errN / thread-errN /
inject-timeout / loadlibrary-returned-0 —— 曾用分段定位注入黑盒）/
`envelope-not-found`（回包须带 seen=[9 信封名]）/ `bad-vector`（含合法空向量放行
规则）/ `no-frame` / `apply-timeout`。

UI 展示前一律过净化（去控制字符、截断 160 字）——曾因回包带二进制尾巴撑坏 WPF
布局，教训固化。

## 6. 运维事实（实测沉淀）

- **游戏内已注入 DLL 后更换 DLL 文件必须重启游戏**才生效（DLL 已驻留内存）。
- 引擎选择配置：旧 `ddmodtool.json` → 新项目 `dd-mrm.json`（`{"engine":"native"}`；
  frida 路线仅存于旧仓库，不迁移）。
- 日志：DLL 与工具两侧都写 `<项目>/logs/engine.log`（工具侧行带 `tool:` 前缀 +
  UTC ISO 时间）。用户令：日志必须自己落文件，不许让用户复制粘贴。
- DLL 日志目录经共享内存 `Local\DdModToolHotapplyCfg` 传递（CreateFileMappingW——
  P/Invoke 必须显式 `CharSet=CharSet.Unicode`，曾因 ANSI 编组错乱导致 DLL 零日志
  +注入黑盒，教训）。
- 冒烟：`native/smoke.ps1` 用 notepad 当宿主验证装入+模式校验+干净退出；真信封
  匹配只能在游戏内验证。
- 注入验证器（旧 rea/verify-inject）：反射调用注入器打活 notepad，断言日志出现
  init enter → 迁移时用 Rust 重写为 `dd-mrm-hotapply` 的集成测试。

## 7. 多轮修改回路（用户操作规程）

进档 → 退出到存档列表（case22 信封从盘重读）→ 工具应用（盘+信封同步改）→
进档生效。列表停留期可连续改任意次（每次=清+推+落盘，幂等；游戏 mod 管理器自身
就是反复调这条链）。

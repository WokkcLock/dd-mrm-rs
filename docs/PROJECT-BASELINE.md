# 项目现状基线（跨机接续入口）

> 本文是跨电脑/跨会话接续的唯一入口：2026-11-16 写于方案定案后。
> 新环境第一件事：读完本文 + [hotapply-design.md](hotapply-design.md)，即可满血接手。
> 技术事实的完整记录在那两份文档；本文管状态、环境、待办。
> 一页式方案图解：[hotapply-v14-explainer.html](hotapply-v14-explainer.html)。

## 一句话

dd-mrm-rs：Darkest Dungeon 存档 mod 管理器（Rust + slint）。离线改档 +
游戏运行中安全热修改；DSON 字节级保真读写。

## 当前状态（2026-11-16）

- **方案定案并实测通过**：B 路线（外部原子写盘 + DLL size 守卫 + 用户动画仪式），
  代码版本 DLL v14 / 共享内存 cfg v8。E2 全流程用户实测：改 mod → 点应用 →
  仪式 → 进存档生效，零断言零崩溃。
- **git 未提交**：v14 重写全部在工作区（8 改 3 删 4 增，见 git status）。
  提交信息建议见文末。
- 测试：cargo test --workspace 66 全绿；cargo check 零警告。
- 产物：cargo dist 一条命令出 dist/dd-mrm.exe（DLL 已内嵌，16.4MB）。

## 现行方案速览（细节见 hotapply-design.md）

三件套，缺一不可：

1. **生产代码原子写盘**：crates/save 的 EstateFile（bit31 保真往返已验证）。
2. **size 守卫**：游戏运行中，DLL 钩 fs_get_file_info（sub_140247F30），
   在查询出口把陈旧 size 改成磁盘真值（唯一崩溃根因的解药）。
3. **仪式**：用户重看开场动画 → 游戏 refresh 全量重读盘 → 新文件进内存。

游戏未运行 = 直接写盘，零风险零仪式。工具/游戏启动顺序无约束。

## 关键常数（游戏 build：Darkest.exe x64，2026-11-05 更新版）

| 常数 | 值 | 说明 |
| --- | --- | --- |
| 守卫目标函数 | sub_140247F30（RVA 0x247F30） | fs_get_file_info，size/locate 的共同原语 |
| 指纹 16B | 48 89 5C 24 18 55 56 57 41 54 41 55 41 56 41 57 | exe 全体唯一；兼作 14B 补丁边界保证 |
| 体校验 | 0x300 窗口含 B8 11 00 00 与 B8 15 00 00 | 第二重身份（FS 缓存字段位移） |
| 功能偏移 | size @ out+0x440；name @ rdx+0x20；path=第三参宽串 | 无校验兜底，真正的功能依赖 |
| 补丁长度 | 覆写 14B（完整指令边界）；跳转补丁 13B | 指纹命中 ⇒ 边界必有效 |
| cfg v8 | magic 0x5A4D4444 / ver 8 / flags@8 / fs_rva@12 / pattern@16 / remoteRoot@32(wchar 256) / logDir@544(wchar 240) | 映射名 Local\DdModToolHotapplyCfg，1024B |
| 门控 | cur>0 && t>0 && t!=cur 才改写 | 防 Documents 探测的 found=0 被谎报 |

## 实验定案史（各一轮，结论已固）

| 轮 | 结论 |
| --- | --- |
| E1a | refresh 仅进屏触发；savefile 从未开火；会话内写盘者=steam.exe |
| E1c | size 层与内容层分离；-4B 吃 padding 不炸；大变复活断言 |
| E1d | size 校正钩成立（FIX×7 全中、零断言）；门控必须有 |
| E1e | 生产路径改 mod 列表三轮全过（FIX×39 对称）；游戏完整吃进 |
| 云对照 | 写盘者=steam.exe（Remote Storage IPC）；陈旧 size 只在同会话威胁 |
| E2 | DLL v14 全流程用户实测通过（2026-11-16） |

## 新机器接手步骤

1. 装 Rust（stable-msvc）+ 本仓库；cargo test --workspace 应 66 绿。
2. DLL 已跟踪在 crates/hotapply/bin/DdHotapply.dll（build.rs 嵌入 exe）——
   无 VS2022 也能构建运行；只有改 native/ 源码才需要 VS2022 重建。
3. cargo dist 出发布目录；首跑在设置里填三根。
4. 验证热修改：开游戏停主菜单 → UI 改一个 mod 点应用 → 状态栏出仪式文案 →
   dist/logs/ 出现 DLL 日志（BOOT/ARMED/FIX）→ 仪式 → 进存档验证。

## 游戏更新维护手册

症状：点应用报「定位失败/游戏版本不匹配」（安全中止，不写盘不崩）。
流程：objdump/IDA 重找 fs_get_file_info 的 16B 头指纹与体位移 → 改
crates/hotapply/src/locate.rs 的 FS_PATTERN（必要时 DdHotapply.c 默认值）→
native/build.ps1 重编 → cargo dist。上一轮（2026-11-05 更新）就是这么做的。

## 环境事实（原机器，换机后重新确认）

- 游戏：F:\SteamLibrary\steamapps\common\DarkestDungeon\_windows\win64\Darkest.exe
- 存档真实位置：E:\Steam\userdata\<UID>\262060\remote\profile_N\
  （Steam Auto-Cloud：游戏逻辑路径指向 Documents，物理落在 userdata；
  <UID> 真值在本地 dist/dd-mrm.json，该文件 gitignored）
- 日志约定：UI 与 DLL 都写 exe 同目录 logs/；调试产物在 .rea/（gitignored）。
- 逆向工具链：IDA 桥（idat64 + REA，localhost:13337）、objdump（binutils 2.30）。
  配方见 .rea/pseudocode/ 与本地 DSH 记忆（不随仓库走）。

## 待办 / 已知瑕疵

- [ ] git 提交 v14（建议信息见下）
- [ ] README 构建段已更新，但「结构」表之后未复查
- [ ] dist.cmd 仍指向 scripts/dist.ps1（旧链路；cargo dist 是正主）——可删可留
- [ ] MIGRATION-PLAN.md 未审（迁移期文件，大概率可归档）
- [ ] docs/game-re-facts.md 部分事实已由 E 系列更新（如 FS 缓存模型），未回写

## 提交信息建议

hotapply: v14 B-route rewrite — pipe retired, size-guard DLL + disk write + ritual

- native: single fs_get_file_info hook (fsfix), cfg v8 shared-memory (anchors+remoteRoot+logDir)
- hotapply crate: locate/hook/injector slimmed; client/pipe/verify deleted
- ui-main: apply = backup -> ensure_hooked -> atomic write -> ritual text
- track built DdHotapply.dll for no-VS builds; E2 user-verified

# 热修改方案设计（v14 · B 路线，已实测成功）

> 状态：2026-11-16 E2 用户实测通过（游戏运行中改 mod → 仪式 → 进存档生效，
> 零断言零崩溃）。本文是现行方案的完整设计记录，取代所有历史方案文档。
> 游戏侧逆向事实详见 [game-re-facts.md](game-re-facts.md)；
> 项目状态与环境见 [PROJECT-BASELINE.md](PROJECT-BASELINE.md)。

## 1. 目标与用户约束

- 游戏运行中改存档 mod 列表，不重启游戏；一个档可改多次。
- 允许给用户规定操作仪式，但不麻烦、不关游戏。
- 禁止异步监视/轮询/后台进程；行为同步可预期。
- 崩溃零容忍：校验不过就拒绝并报告，绝不盲写。

## 2. 定案结论：写盘 + 守卫 + 仪式

不做任何「重放游戏内部调用」（v17 已废弃）。三件套：

1. **外部原子写盘**：用自家生产代码（crates/save）直接改 persist.game.json。
   DSON bit31 保真往返早已验证；游戏未运行时这条路单独就是完整方案。
2. **size 守卫**：游戏运行中，唯一崩溃根因是 FS 缓存的陈旧 size 触发
   GetStringData 断言（详见 §5）。DLL 钩住 fs_get_file_info，在查询出口
   对比磁盘真值就地改写——同步、单点、零监视。
3. **仪式**：用户在主菜单重看一次开场动画。游戏的 refresh 链会 9 槽全量
   重读盘，把新文件完整吃进内存并照常回写。我们从不碰游戏对象。

## 3. 组成

```
dd-mrm.exe (slint UI)
  │ 点「应用」(worker 线程)
  ├─ 备份存档目录 (zip → SaveBak/)
  ├─ ensure_hooked(): 找 Darkest 进程 → 读 exe 指纹定位 → 写 cfg v8
  │   → CreateRemoteThread+LoadLibraryW 注入 DdHotapply.dll
  └─ 原子写盘 (crates/save, tmp+rename)
                    Darkest.exe
                      └─ DdHotapply.dll: fs_get_file_info 钩
                          └─ 每次存档 size 查询：磁盘真值 vs 缓存值，失配即改
```

- DLL（native/DdHotapply.c + thunk64.asm，MSVC /MT）无管道无通信：
  注入即自活，参数全部来自注入前写好的共享内存（cfg v8）。
- 注入按需发生（点应用且游戏在跑），无守护进程。幂等：同进程二次
  LoadLibrary 只加引用计数，钩子不重复装；游戏重启后下次应用自动重注。

## 4. 应用流程（UI 侧）

1. 快照（含 save_root）→ 备份 → 判定 hot_path：
   - 游戏未运行且热修改开 → 备份后重查进程（TOCTOU 哨），仍无 → 直接写盘，
     无需仪式。
   - 热修改关 → 纯文件写盘（游戏在跑则警告退出覆盖）。
2. hot_path：ensure_hooked 失败 = 报错中止、**不写盘**（宁可不做不可做错）。
3. 成功 → 原子写盘 → 状态栏给仪式指引文案。
4. 用户仪式 → 游戏吃进 → 下次退出存档时 steam.exe 照常回写（实测确认）。

## 5. 为什么必须守卫：断言机制

- 游戏读 persist 走三层：fs_get_file_info 出 size → 按大小分配缓冲 →
  Reader.GetStringData 拷内容。拷贝前检查：array_size（文件内联长度前缀）
  <= buffer_size（来自 size 查询），违反即弹断言对话框，选调试=崩溃。
- size 有长寿命缓存（Steam Auto-Cloud 层之下），游戏自己读成功都不刷新；
  同一会话内外部改文件 → 缓存旧 size < 新实际长度 → 断言。
- 守卫在 fs_get_file_info 出口把出参记录 +0x440 的 size 改成磁盘真值，
  断言前提消失。E1d/E1e 实测：陈旧 3932→真值 3944 等共 46 次校正全中，
  零断言。

## 6. DLL 侧守卫细节

- 钩点：fs_get_file_info（sub_140247F30）。签名（游戏侧）：
  rcx=FS 对象, rdx=名字盒（串在 +0x20）, r8=路径宽串, r9=出参记录 1092B。
- thunk（fsfix_thunk）：完整保存 volatile 语境（含 xmm0-5），调蹦床跑原函数，
  再用保存的 rdx/r8/r9 调 fsfix_op，最后恢复返回。
- fsfix_op 门控：路径含 persist.game 且 cur>0 且 t>0 且 t!=cur 才改写
  （cur=缓存值 t=磁盘真值；防 Documents 探测查询 found=0 被谎报存在）。
- 磁盘真值 DiskTruthW：先按游戏逻辑路径 CreateFileW（进程内自然走
  Auto-Cloud 重定向），失败回退 remoteRoot/profile_N/<basename>
  （N 从名字盒提取；remoteRoot 来自 UI 配置，空=禁用回退，主路径不受影响）。
- 日志：cfg 里的 logDir 下每次 fix 一行 + 计数器；E2 主证据即此日志。

## 7. 共享内存 cfg v8（映射 Local\DdModToolHotapplyCfg，1024B）

| 偏移 | 内容 |
| --- | --- |
| 0 | magic 0x5A4D4444 |
| 4 | version 8 |
| 8 | flags（bit0=锚点有效） |
| 12 | fs_rva（u32，工具现场定位结果） |
| 16 | 指纹 16B |
| 32 | remoteRoot（UTF-16，256 码元，UI 的 save_root） |
| 544 | logDir（UTF-16，240 码元） |

## 8. 定位与指纹（零写死地址）

每次点应用：读当前 exe → 找 16B 头指纹（48 89 5C 24 18 55 56 57 41 54
41 55 41 56 41 57，全体唯一）→ 0x300 窗口体校验（B8 11 00 00 与
B8 15 00 00 位移）→ 算 RVA 经 cfg 传 DLL。编译期默认值只是无锚点场景
的兜底。游戏更新后：函数搬家无感；函数改动 → 指纹失配 → 安全中止 →
按 PROJECT-BASELINE 维护手册换指纹。功能偏移（+0x440/+0x20）无校验
兜底，是真正的版本依赖——但改动它们几乎必然连动指纹/体校验。

## 9. 实测记录

- E1d（Frida 原型）：陈旧 3932→3944 校正 7 次全中，卡名即时更新，零断言。
- E1e（生产路径）：摘/加 mod 三轮（3944→3828→3732→3824），FIX×39 对称，
  皮肤肉眼确认来回切换，零断言零崩溃。
- E2（DLL v14 全流程）：用户实测通过（2026-11-16）。

## 10. 历史方案与死因（一行各）

| 方案 | 一行结论 |
| --- | --- |
| v1/v2 帧一体化（C#/早期） | 吃旧版红利：帧函数每帧调+保存一体；2026-11-05 更新双前提齐灭 |
| v3..v16 泵/两段式系列 | 十七轮失败：脏标志挡写、载入覆盖内存态、断言频发 |
| v17 重放（调包注册表+自消费） | 链路全通但保存守卫 dirty=0 挡写盘，载入洗掉内存改动 |
| v18 refresh 尾执行 | 机制成立但被更简单的 B 取代 |
| v19 裸 A（纯写盘） | 证伪：同会话陈旧 size 必触发断言（E1d） |
| **v14 = B 路线（现行）** | **写盘+守卫+仪式；E1d/E1e/E2 三级实测** |

## 11. 文件地图

| 部分 | 文件 |
| --- | --- |
| 守卫 DLL | native/DdHotapply.c, native/thunk64.asm, native/build.ps1 |
| 定位 | crates/hotapply/src/locate.rs（指纹+体校验） |
| 注入与 cfg | crates/hotapply/src/injector.rs, hook.rs |
| DLL 内嵌 | crates/hotapply/build.rs + bin/DdHotapply.dll |
| 生产写盘 | crates/save（EstateFile/sync_mods/apply.rs） |
| UI 流程 | crates/ui-main/src/app.rs（apply_worker）, model.rs（文案） |
| 实验脚本 | crates/save/examples/{e1c_rename,e1e_mods,read_dlc}.rs |

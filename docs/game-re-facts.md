# Darkest.exe 逆向事实（热修改的依据）

> 依据：旧项目 DdModTool 的 RE 战役（2026-11-02 ~ 11-03），全部结论经过反编译 +
> 运行时取证双重验证。反编译中间产物与 IDA 库未迁移，
> 本文只保留**成立方案所依赖的事实**。
>
> 免责声明：本项目为非官方社区工具，与 Red Hook Studios 无关。本文件仅供学习
> 与互操作性研究，对所有提及的地址/校验值不提供任何保证。
>
> 版本锁定：Darkest.exe x64，ImageBase 0x140000000，
> MD5 d1bd550c9807ea20f5aa3c762975d3ee。所有 RVA 仅对此版本有效；DLL 侧有
> FRAME_PATTERN 入口字节校验，版本不符拒装。

## 1. 存档三定律（架构地基）

1. **boot 是唯一一次全量读盘**：启动时把 9 个档的信封数据从磁盘读进内存
   （SetEnvelopesStates → ResetEnvelope ×9 → SetupProfile ×9，内含 ParseDSON）。
2. **进档零读盘**：进入存档不读 persist.game.json（实测 FS 查找钩零命中）——数据
   全部来自内存信封。
3. **退出存档时内存写回盘**：游戏把自己内存里的（可能是旧的）内容刷回磁盘，
   外部工具的磁盘写入在此被覆盖；且游戏自己的 FS 条目缓存（按路径记 size）不因
   外部写入失效——读旧 size → 截断读 → 越界解析 → 断言/崩溃。

推论：**纯文件方案架构上不可能**，必须改内存；热修改的本质 = 改信封内存。

## 2. 存档数据在内存里的两份拷贝

| 拷贝 | 位置 | 生命周期 | 填充者 |
|---|---|---|---|
| 9 个信封 Envelope[9] | 列表屏对象 `qword_14117D8B0`（RVA 0x117D8B0）+5992 起，每信封 3088B | 列表屏存活期间；进档流程 case24 末尾销毁列表屏 | boot 时 SetupProfile/ParseDSON；case22 重建列表屏时**从盘重读** |
| 战役 PG（PersistGame） | 战役全局对象 `word_142A5BB40`+1864 起（容器 +1864 base/+2408 persistent/+2952 applied/+3496 presented） | 整个战役会话 | case24 从活动信封深拷贝（sub_140411790 ×4 + 标量块） |

信封内 PG 布局（PG 基址 = 信封+328）：

- +0 base 容器（名[512]@+8，vector@+520）
- +544 persistent_ugcs 容器（vector@+1064）
- +1088 applied_ugcs 容器（vector@+1608）← **mod 列表本体**
- +1632 presented_dlc 容器（vector@+2152）
- 标量区 +2176..（game_mode@+2304、estatename@+2432、totalelapsed@+2752、
  ok 旗标@+2756、inraid@+2757）
- 信封级字段：+0 档名 char[32]（profile_N / steam_profile_N，**不保证 NUL 结尾**，
  尾部可能有堆脏字节——匹配必须用前缀）、+325 has-campaign 旗标、+140 周目数、
  +192 区域名。

列表条目结构 192B：`title[128] + version[64]`。四个容器的 vector 布局一致：
`{begin, end, capacity}`，元素连续存放。**空向量 begin=end=NULL 合法**
（清空 mod 后信封重建即此态，写入器与 DLL 都必须放行）。

## 3. 进档数据流（状态机 sub_1403D93C0，RVA 0x3D93C0）

```
存档列表点"进入"(case21 重置战役, flag byte_142A5D24A 恒0走正常路)
  → case22 重建列表屏(vt[8] init → SetEnvelopesStates → 信封从盘重读)   ← 磁盘唯一介入点
  → case23 (动画)
  → case24 信封PG --深拷贝--> 战役PG；末尾销毁列表屏(信封随之消失)
  → state1 挂载链 sub_1403D81D0：
       sub_1403D6D90 (-mod_profile 命令行覆盖，正常不存在)
       sub_1403ED370  base 容器逐条对 mod 注册表(qword_14117D868)建覆盖树挂 VFS
       sub_1403D7600  dlc 迁移后处理
       sub_1403EE140  applied 容器逐条推 "<moddir>/modes/<game_mode>/" + "<moddir>" 挂 VFS  ← mod 生效终点
       sub_14060D940  modes 表重建
       sub_1403EDC90  game_mode 目录挂载
       sub_1403D7960  新战役分支(已有战役被 inraid 等旗标短路)
       发 game_loaded_event
```

运维含义：

- **改了不生效的原因**：进档读的是信封（boot 时的旧内容），磁盘改动对不上的窗口
  全部失效。
- **退列表再进** = case22 信封从盘重读 → 唯一合法的多轮热修改回路：
  退到存档列表 → 工具应用（改盘 + 改信封） → 进档。
- **战役内（镇/raid 屏）信封不存在**，热修改在列表屏停留期之外无法作用。

## 4. FS 缓存与历史崩溃根因（已闭环）

- 游戏 FS 层按路径缓存句柄+size（条目+72 句柄 / +80 size，记录 0x440B）。
  open_RestoreData（sub_140293530）取 size 用缓存值——外部工具改盘后 size 过期，
  按旧 size 分配读取 → 截断 → 解析走出缓冲区 → 相邻堆块（PNG XMP 等）被当数据
  → `GetStringData` 断言 `buffer_size >= array_size`（hierachical file.cpp:718）。
- v2.1 之后落盘由游戏自己的 fPersist（sub_14040F5B0）完成 → 自写自读缓存天然
  一致，此根因被结构性消除。

## 5. 官方应用路径（热修改调用的游戏函数）

| 函数 | RVA | 签名 | 作用 |
|---|---|---|---|
| fPush | 0x562920 | `void fPush(容器对象*, 条目192B*)` | 官方 vector 推入（增长/查重/搬移全在游戏代码内） |
| fPersist | 0x40F5B0 | `i64 fPersist(列表屏*, uint 槽位)` | 把该信封 PG 序列化落盘（内含 `!demo && env+325` 守卫；非战役态才写） |
| 帧函数 | 0x3D93C0 | — | 主状态机每帧入口；DLL 钩它拿主线程时机 |

应用 = 清空目标容器 vector（begin=end，cap 不动）→ 逐条 fPush → fPersist。
全程在帧间隙（帧函数入口处）执行，与游戏资源加载无堆竞争。

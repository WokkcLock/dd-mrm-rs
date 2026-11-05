# DSON bit31 与保真写入——本项目补充发现

上游规格（[dson-spec-robojumper.md](dson-spec-robojumper.md)）只记载 fieldInfo 的
bit0 / bits2-10 / bits11-30。本项目在实测中确认了 **bit31（0x80000000）** 的存在与
处理方式，以及若干写入器必须遵守的规则。这是 dd-mrm-dson 写入器的实现规范。

## 1. bit31 是什么

- 游戏自己写出的存档里，meta2 fieldInfo 的 bit31 **逐条目** 置位或清零。
- 同一逻辑存档内容在不同时刻写出，同一条目的 bit31 可能不同（例如 profile_2 的
  base_root 曾观察到 0，profile_7 曾观察到 1）→ 它是**内容历史状态**，不能从结构
  或字段名推导。
- **游戏读取路径不消费 bit31**：已反编译的读取器（GetDataEntry sub_140237540、
  GetStringData sub_140237B80、列表加载器 sub_140389260 等）对 fieldInfo 全部按位
  掩码取用，bit31 被忽略。写错 bit31 不会导致读取崩溃。
- 尽管如此，写入器仍必须**逐位保留** bit31：目标是字节级保真（同内容重写 = 字节
  一致），且不排除未来代码路径消费它。

> 历史注记：旧项目最初的崩溃归因于"bit31 缺失导致解析错位"，后被证伪——真正的
> 根因是游戏 FS 缓存 size 过期（见 game-re-facts.md §4）。bit31 的意义是保真，
> 不是防崩。

## 2. 写入器算法（已验证，旧项目 commit 5de9a72）

对既有条目：**按树路径匹配源文件逐位复制 bit31**。

- 匹配键 = 从根到该字段的唯一路径（如 `base_root / applied_ugcs_1_0 / 3`）。
- 教训（bug #2，曾引发"新增 mod 后游戏断言"）：不能用「字段名 + 全局第 N 次出现」
  匹配——三棵列表树（外层 applied/persistent、内层 applied/dlc）存在大量同名子键
  `"0".."N"`，新增条目会让计数错位，bit31 整体配对推移，SET 变 CLEAR。
- 容器翻转规则：进入 `applied_ugcs_1_0` / `persistent_ugcs` / `presented_dlc` /
  `dlc` 的内容区后，`dynamicMode` 翻转（容器条目自身不置位，其后新条目置位；
  共四个容器名，旧 C# 源码实证）。

对全新条目（源文件没有对应路径）：启发式——`base_root` 置位；处于 dynamicMode 区
间的置位。此规则由 2 个游戏自写文件归纳，是**归纳而非推导**；遇到无法匹配且启发
式不敢断言的结构时，写入器应拒绝写出并报告（保真优先）。

## 2. 二进制布局（解析与写出的精确语义）

- 魔数：文件头 4 字节 `01 B1 00 00`（LE int32 = 0xB101 = 45313）。
- Header 共 64B：`revision`(i32, 原样保留)、meta1Offset(恒 64)、meta1Size(=numMeta1×16)、
  numMeta1、meta2Offset(=64+meta1Size)、numMeta2、dataOffset(=meta2Offset+numMeta2×12)、
  dataLength，其余字节原样保留（zeroes 区域不得重排）。
- meta1 条目 16B = 4×i32：parentIndex, meta2EntryIdx, numDirectChildren, numAllChildren。
  **只有对象有条目**；先序（preorder）：对象先 append 自身再递归子对象；meta2EntryIdx
  在遍历 meta2 时回填。
- meta2 条目 12B：nameHash(i32), offset(i32), fieldInfo(u32)。
  nameHash = ddHash（`i32 hash = hash*53 + byte`，wrapping，UTF-8 字节，**不含 NUL**——
  金样本实证；曾误记"含 NUL"）。
- **meta2 顺序 == 数据区顺序**（offset 单调不降）。字段 i 的原始区域 =
  `[offset_i, offset_{i+1})`，末字段 `[offset, dataLength)`。
- fieldInfo 位域：bit0=isObject；bits2-10=字段名长（**含 NUL**，=字节数+1）；
  bits11-30=该字段（若为对象）的 meta1 下标，非对象恒 0；bit31 见下。
- 输出布局 = header64 + meta1 + meta2 + data 连续拼接。

## 2b. 类型推断（读取端，复刻旧 C# DataFieldFactory）

对叶子字段区域（区域长 − 名长）：
- ==1 → Bool（1 字节，不对齐）。
- 否则名后按 `(regionStart+nameLen)` 补齐到 4；取对齐处 i32 v、剩余长 remain：
  remain==0 → Int；v==remain → String（i32 长度含 NUL + UTF-8 + NUL）；
  v∈{0,1} 且 remain==4 → TwoBool（两个 i32 各 0/1）；其他 → 结构不支持，报错拒绝。
- 对象字段（bit0=1）值区为空。

## 2c. 写入对齐规则

- 字段名（UTF-8+NUL）内联数据区开头；Int/String/TwoBool 值前按数据区绝对偏移对齐
  到 4；Bool/Object 不对齐。fp 从数据块起点 0 累计。
- String 值 = i32(字节数+1) + UTF-8 + NUL；TwoBool = 8B；Bool = 1B；Int = 4B。

## 4. 已验证的边界行为

- **清空全部 mod**：写入器把 `applied_ugcs_1_0` 字段整体置 null → 落盘文件里该
  字段消失。游戏实测容忍（列表重建后信封空向量合法，begin=end=NULL）。
- **同内容重写**：解析→不改→写回，字节一致（profile_2/7 金样本验证）。
- **删最后一个 mod**：重写 = 最小结构差（只动该条目相关区域）。
- 旧项目 rt 台（rea/rt）是这套验证的原始驱动器；dd-mrm-dson 的 cargo test 用
  `tests/golden/` 三个样本复刻同等验收。

## 5. 类型系统边界（重要护栏）

- DSON 类型无法从文件自描述推断（上游 Notes 已述）。本项目只支持
  **persist.game.json 家族**的字段类型集合（Object/Bool/TwoBool/String/Int/
  StringVector 等）。
- **绝不读写 hero、persist.raid.json 等其他 16 个存档文件**——它们的 Char/Float/
  IntVector 字段不在支持集内。写入器按文件名白名单拒绝。

## 6. dd-mrm-dson 实现契约（Rust，raw-slice 模型）

- 内存模型：解析产出 arena `Vec<FieldNode>`，节点含
  `{name, is_object, field_info(u32 原始保留), meta1_fp, region: Range<usize>(数据区内), children, parent, path}`；
  arena 顺序 = 原 meta2 顺序（先序）。
- 单一表示：杜绝旧项目 bytes→JsonElement DOM→对象 三表示问题。变更 = 只替换
  `base_root` 子树内的目标节点（Fresh 节点或空 = 删除/置 null 语义）；未触碰字段
  的区域字节**原样直拼**，不重编码。
- 写出：重算全部对象的 meta1 下标与 numDirect/numAll；meta2 offset 按拼接顺序重排；
  Fresh 节点按 §2b/§2c 规则生成字节；bit31 按树路径匹配从源文件复制，新条目用
  §2 启发式，不敢断言 → 拒绝写出并报错。
- 解析断言：offsets 单调；首条目 = 对象（meta1_fp==0）；消费条数 == numMeta2；
  header 布局等式全部成立。违者报错拒绝。
- 库零第三方依赖（std only）；dsoncli 子命令 parse / dump / rewrite / roundtrip，
  roundtrip = 自校验字节一致，路径由调用者传入。

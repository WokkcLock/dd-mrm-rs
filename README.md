# dd-mrm-rs

Darkest Dungeon 存档 mod 管理器（Rust + slint 重写版）。离线改档 + 游戏运行中热
修改，DSON 字节级保真。

状态：**迁移完成（M0-M4），全部门禁绿**——四 crate 均带测试，五个任务经独立实现+审查闭环。

## 构建

```powershell
cargo build -p ui-main --release        # GUI 主程序
cargo build -p dson                    # 纯编解码 CLI（dsoncli）
cargo test --workspace                 # 全部测试
```

产物：`target/release/dd-mrm.exe`（slint 静链，无运行时依赖）。

## 运行 / 部署

- 首跑在设置里填三个目录：存档根（…/262060/remote）、本地 mods、创意工坊
  `…/workshop/content/262060`；配置落在 exe 同目录 `dd-mrm.json`。
- **热应用需要 DdHotapply.dll**：源码在 `native/`（C + asm），一条命令构建：
  `pwsh -File native/build.ps1`（需 VS2022 C++ 生成工具），产物落在
  `crates/hotapply/bin/DdHotapply.dll`。运行时搜索顺序：环境变量
  `DDHOTAPPLY_DLL` → exe 同目录 → exe/eng/native。发布时把 dll 拷到
  dd-mrm.exe 旁边即可。
- 备份 zip 与日志（logs/engine.log）都在 exe 同目录下。

## dsoncli（纯编解码，可单独编译）

```powershell
cargo run -p dson -- parse  <file.dson>   # 元表+树 dump（stdout）
cargo run -p dson -- roundtrip <in.dson>  # 解析→重写→字节比对，报告差异
```

不找 Steam 目录、不写游戏存档——路径用户传，只读输入文件。

## 结构

| crate | 职责 |
|---|---|
| dson        | DSON 二进制编解码（bit31 保真、字节级往返），lib + dsoncli |
| save        | 档发现 / mod 扫描 / 离线应用 / zip 备份（白名单 persist.game.json） |
| hotapply    | 注入器 + 命名管道客户端；协议权威 = `native/DdHotapply.c` |
| ui-main     | slint GUI（中文），产品二进制 |

背景、架构与全局约束见 [MIGRATION-PLAN.md](MIGRATION-PLAN.md)；知识文档在
[docs/](docs/)。

## 许可证

[MIT](LICENSE)。非官方社区工具，与 Red Hook Studios / Steam 无关；改档前自动
备份到 exe 同目录 `SaveBak/`，仍请自行承担存档风险。

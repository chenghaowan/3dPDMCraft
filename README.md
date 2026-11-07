# PDM Lite (3dPDMCraft)

[English](README_EN.md) | 中文

轻量级 CAD 数据管理系统：本地工作目录 + 阿里云 OSS 同步 + 零件级版本历史 + 多设备同步 + 冲突管理。Rust 单二进制，无中心服务器（元数据以不可变事件形式存于 OSS，各设备对账合并）。

## 特性

- **双文件夹工作区**：`native/`（带参源文件 CATPart/sldprt/scdoc…）+ `neutral/`（不带参 STEP/STL/CGR…），按相对路径 stem 归组为"零件"
- **零件修订制**（参照 Siemens Teamcenter 的 Item/Revision/Dataset 模型）：BLAKE3 内容寻址对象永不覆盖，天然去重，历史与回滚零上传
- **冲突管理不做硬锁**：意向声明（48h 自动过期）+ 基线版本 stale 检测 + 三选项解决（用我的 / 用对方的 / 都保留=fork），任何选择都不丢数据
- **neutral 派生物冲突自动解决**（newest-wins），native 冲突才进人工队列
- **单零件 / 整装配 fork**：零拷贝；整装配按 STEP 依赖图连带 fork
- **装配依赖图**：解析 neutral STEP 文本 PRODUCT 实体（启发式），受影响零件传递闭包查询
- **历史保留策略**：保护规则（current / base 引用 / open 冲突 / 最近 N 个修订 / T 天内 / pinned）+ dry-run 先行 + gc 事件广播
- **多工作区**：一台机器多个本地目录，各自绑定不同远程（bucket/前缀可不同）；第二台设备 init 自动接管远端历史（clone 语义）
- **Web UI + 全量 CLI** 双入口

## 构建

```bash
cargo build --release   # 产物 target/release/pdm(.exe)
```

## 快速开始（Web UI）

```bash
pdm serve --port 8420    # 浏览器打开 http://127.0.0.1:8420
```

页面操作：添加/初始化工作区（含 OSS 测试连接）→ 文件/历史/回滚 → 冲突队列三选项 → 依赖图/impact → 裁剪 dry-run/执行。

## CLI

Web UI 能做的命令行都能做（与 `pdm serve` 可并存，SQLite WAL 排队安全）：

| 命令 | 说明 |
|---|---|
| `pdm init <目录> --endpoint E --bucket B --ak K --sk S [--prefix P] [--backend oss\|local] [--store-root <共享目录>] [--user U]` | 初始化工作区；`local` 后端离线可用，`--store-root` 指向共享目录即可模拟双机 |
| `pdm serve [--port 8420]` | 常驻：watcher + 60s 定时拉取 + Web UI |
| `pdm workspaces [--json]` | 工作区列表 |
| `pdm status <ws> [--json]` | 零件/文件/修订/对象统计 |
| `pdm files <ws> [--json]` | 文件表（含 ⚠远端更新 / 待更新标记） |
| `pdm history <ws> --part <零件> [--json]` | 修订历史 |
| `pdm pull <ws>` | 立即拉取远端事件 |
| `pdm rescan <ws>` | 检测本地变更 + 重建依赖图（**无 serve 时的本地变更入口**） |
| `pdm rollback <ws> --part <零件> --seq <n>` | 回滚（指针移动 + 工作区落盘） |
| `pdm conflicts <ws> [--json]` | 未解决冲突队列 |
| `pdm resolve <ws> --id <id> --mine\|--theirs\|--fork [--fork-name <名>]` | 解决冲突（都不删对象） |
| `pdm fork <ws> --part <零件> --new-name <新名> [--seq <n>]` | 单零件零拷贝 fork |
| `pdm fork-tree <ws> --part <装配> [--suffix <后缀>]` | 整装配 fork（含全部传递依赖） |
| `pdm prune <ws> [--execute]` | 历史裁剪（默认 dry-run） |
| `pdm deps <ws> [--json]` / `pdm impact <ws> --part <零件>` | 依赖图 / 受影响装配 |
| `pdm remove <ws>` | 移除注册（不删数据） |
| `pdm test-oss --endpoint E --bucket B --ak K --sk S [--prefix P]` | 探针测试连接 |

## 配置（`<工作区>/.pdm/config.toml`）

OSS 连接、`prune_n`（每零件保留最近 N 个修订，默认 10）、`prune_t_days`（默认 90）、
`auto_prune` / `auto_prune_hours`（自动定期裁剪，默认关/24h）、`user`。

删除文件、忽略规则（`~$*`、`*.sld~` 等）在 `<工作区>/.pdm/ignore.conf`。

## 设计与运维

- 完整设计方案（冲突管理/同步协议/数据模型/保留策略）：[docs/design.md](docs/design.md)
- OSS 生命周期规则（bucket 控制台运维配置）：`objects/` 前缀 180 天转低频 / 365 天转归档；`meta/` 保持标准存储
- 凭证安全：请使用最小化 RAM 子账号（仅目标 bucket + 前缀读写删），不要复用主账号 AK

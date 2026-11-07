---
name: pdm-lite
description: 操作/开发 PDM Lite（3dPDMCraft，Rust 单二进制 pdm）轻量 CAD 数据管理工具：构建运行、CLI 全量命令、工作区初始化、双机同步、冲突解决、fork、历史裁剪、依赖图、发版打包。当用户要求操作 pdm / 3dPDMCraft / PDM Lite（初始化工作区、解决冲突、同步拉取、裁剪历史、依赖图/impact、打包发版）时使用。
---

# PDM Lite 操作手册

## 项目位置与事实源

- 源码：`D:\claw.forvia\3dPDMCraft`（Rust stable，单二进制 `pdm`，v0.2.0 起 CLI 全量）
- 设计方案：`docs/design.md`（v0.8，唯一事实源——先读再动）
- 远程：GitHub `chenghaowan/3dPDMCraft`（gh 已登录）+ Gitee `chenghao_wan/3dPDMCraft`（token 由用户提供，勿落盘）
- OSS 凭证：字段名与 `D:\claw.forvia\ltl-manage\backend\.env` 对齐，可直接复用

## 核心不变量（改代码前必读，详见 design.md §3/§7）

1. 对象内容寻址 `objects/<2>/<2>/<blake3>`，**永不覆盖**；先传对象后写修订行
2. 版本挂在 part（stem 归组 native/Parts/X.CATPart + neutral/Parts/X.step），不在文件
3. 元数据同步 = 不可变事件 JSON（`meta/revisions|tombstones|resolutions|reservations|gc/`），本地 SQLite 只是缓存；**自己写的事件要立刻 mark_applied**
4. 冲突三选项（mine/theirs/fork）**都不删对象**；neutral-only 分歧自动 newest-wins 不进队列
5. 回滚 = current 指针移动 + 工作区落盘（此后新修订 parent 指向回滚点，seq 允许同号并列）
6. 裁剪必过 §7.2 保护规则（current/base 引用/open 冲突/最近 N 个/T 天内/pinned），dry-run 默认
7. **已解决的分歧不可重评**：pull 只对当轮新导入修订做分歧判定 + 同 (local,remote) 对去重（M1 踩过重复建冲突的坑）

## CLI 全量命令（与 pdm serve 可并存，WAL 排队安全）

```bash
pdm serve [--port 8420]                     # watcher + 60s 定时 pull + Web UI
pdm init <目录> --endpoint E --bucket B --ak K --sk S [--prefix P] \
    [--backend oss|local] [--store-root <共享目录>] [--user U]
pdm workspaces|status|files <ws> [--json]   # 查看
pdm history <ws> --part <零件> [--json]
pdm pull <ws>                               # 立即拉取远端事件
pdm rescan <ws>                             # ★ 无 serve 时本地变更唯一入口（含重建依赖图）
pdm rollback <ws> --part <零件> --seq <n>
pdm conflicts <ws> [--json]
pdm resolve <ws> --id <id> --mine|--theirs|--fork [--fork-name N]
pdm fork <ws> --part <零件> --new-name <名> [--seq <历史点>]
pdm fork-tree <ws> --part <装配> [--suffix 方案B]   # 按依赖图连带全部传递依赖
pdm prune <ws> [--execute]                  # 默认 dry-run
pdm deps <ws> [--json] / pdm impact <ws> --part <零件>
pdm remove <ws>                             # 只移除注册不删数据
pdm test-oss --endpoint E --bucket B --ak K --sk S [--prefix P]
```

注意语义：
- `pull` 只应用远端事件；**不跑 serve 时本地变更靠 `rescan` 检测**（watcher 只在 serve 进程里）
- 第二台设备 `init` 自动接管远端历史（clone 语义），不重建基线
- 依赖图只解析 neutral STEP 文本 PRODUCT 名（启发式，同名误连）；CATProduct 不解析
- 本地路径含中文的 JSON 请求体在 Git Bash 里会 GBK 乱码 → 用 Web UI 或 ASCII 后缀

## 冒烟测试流程（双工作区共享 store 模拟两台电脑）

```bash
# wsA/wsB 各放相同内容文件（B 模拟第二台 clone），纯 CLI：
pdm init .smoke/wsA --endpoint d --bucket d --backend local --store-root D:/abs/shared --user alice
pdm init .smoke/wsB --endpoint d --bucket d --backend local --store-root D:/abs/shared --user bob
pdm serve --port 8421 &          # 或不用 serve：每侧改完 pdm rescan + pdm pull
# 关键回归：双端同基线各改 native → 双端冲突 → resolve theirs → 零丢失
#           双端只改 neutral → 0 冲突自动收敛；删除 → 墓碑；fork-tree → 对端同步
# 现场清理：pdm remove <ws> + rm -rf .smoke + 删 %LOCALAPPDATA%/pdm/workspaces.toml
```

发版：`cargo build --release` → `pdm-vX.Y.Z-windows-x64.zip`（pdm.exe + README* + docs + skills）→ GitHub `gh release create` + Gitee API v5（`--data-binary @-` 传 ASCII JSON，中文内联 body 会得到空响应）。

## 已知边界（勿踩）

- 大文件上传整读内存（<几百 MB 无碍），流式分片是后续优化项
- 同机双工作区共享 device_id（机器级身份），意向警示在双机场景才完全生效
- workspace 路径含中文/空格在 CAD 下的兼容性未实测；真双机 OSS 实测未做

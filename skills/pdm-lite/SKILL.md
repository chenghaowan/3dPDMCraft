---
name: pdm-lite
description: 操作/开发 PDM Lite（3dPDMCraft，Rust 单二进制 pdm）轻量 CAD 数据管理工具：构建运行、工作区初始化、双机同步、冲突解决、fork、历史裁剪、发版打包。当用户要求操作 pdm / 3dPDMCraft / PDM Lite（初始化工作区、解决冲突、同步拉取、裁剪历史、打包发版）时使用。
---

# PDM Lite 操作手册

## 项目位置与事实源

- 源码：`D:\claw.forvia\3dPDMCraft`（Rust stable，单二进制 `pdm`）
- 设计方案：`docs/design.md`（v0.8，唯一事实源——冲突管理参照 Teamcenter、事件同步、双文件夹、保留策略都在里面，先读再动）
- OSS 凭证：字段名与 `D:\claw.forvia\ltl-manage\backend\.env` 对齐（OSS_AccessKeyId/OSS_AccessKeySecret/OSS_EndPoint/OSS_BUCKET），可直接复用

## 构建与运行

```bash
cargo build                                    # 调试
cargo build --release                          # 发版
cargo run -- serve --port 8420                 # 常驻：watcher + 同步引擎 + Web UI
cargo run -- init <目录> --endpoint oss-cn-hangzhou.aliyuncs.com --bucket <b> \
    --ak <id> --sk <secret> [--prefix <p>] [--backend oss|local] [--store-root <共享目录>]
```

- UI 仅绑 127.0.0.1:8420：工作区看板 → 配置页（OSS 测试连接/init 引导）→ 文件/历史/回滚/冲突队列/依赖图/裁剪
- `--backend local --store-root <共享目录>`：两台"电脑"模拟（或 NAS 共享盘部署），不碰云

## 核心不变量（改代码前必读，详见 design.md §3/§7）

1. 对象内容寻址 `objects/<2>/<2>/<blake3>`，**永不覆盖**；先传对象后写修订行
2. 版本挂在 part（stem 归组 native/Parts/X.CATPart + neutral/Parts/X.step），不在文件
3. 元数据同步 = 不可变事件 JSON（`meta/revisions|tombstones|resolutions|reservations|gc/`），本地 SQLite 只是缓存；`applied_events` 表保证拉取幂等；**自己写的事件要立刻 mark_applied**
4. 冲突解决三选项（mine/theirs/fork）**都不删对象**；neutral-only 分歧自动 newest-wins 不进队列
5. 回滚 = current 指针移动 + 工作区落盘（之后新修订 parent 指向回滚点，seq 允许同号并列）
6. 裁剪必过 §7.2 保护规则（current/base 引用/open 冲突/最近 N 个/T 天内/pinned），dry-run 默认先行

## 冒烟测试流程（双工作区共享 store 模拟两台电脑）

```bash
# wsA/wsB 各放相同内容文件（B 模拟第二台 clone），然后：
pdm init .smoke/wsA --endpoint d --bucket d --backend local --store-root D:/abs/shared --user alice
pdm init .smoke/wsB --endpoint d --bucket d --backend local --store-root D:/abs/shared --user bob
pdm serve --port 8421 &
# 改文件 → sleep 5（debounce 2s+聚合）→ POST /api/ws/wsB/pull
# 关键回归：双端同基线各改 native → 双端出冲突 → resolve theirs → 零丢失
#           双端只改 neutral → 0 冲突自动收敛；删除 → 墓碑；fork-tree → 对端同步
```

API 一览：`GET/POST /api/workspaces[/init]`、`POST /api/oss/test`、`GET /api/ws/{n}/(status|files|history?part=|conflicts|deps|reservations)`、`POST /api/ws/{n}/(pull|rescan|rollback|conflicts/resolve|fork|fork-tree|prune|deps/rescan)`。

## 发版流程

```bash
cargo build --release
# zip = pdm.exe + README.md + docs/design.md + skills/，命名 pdm-vX.Y.Z-windows-x64.zip
# GitHub: gh release create vX.Y.Z <zip> ；Gitee: API v5 创建 release + attach_files
```

## 已知边界（勿踩）

- CATProduct 是私有二进制格式，依赖图只解析 neutral STEP 文本（启发式：PRODUCT 名匹配，同名零件会误连）
- 同机双工作区共享 device_id（机器级身份），意向警示在双机场景才完全生效
- 大文件上传是整读内存（<几百 MB 无碍），流式分片是 M3 优化项
- workspace 路径含中文/空格在 CAD 下的兼容性未实测

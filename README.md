# PDM Lite (3dPDMCraft)

轻量级 CAD 数据管理系统：本地工作目录 + 阿里云 OSS 同步 + 零件级版本历史 + 双机同步 + 冲突管理。Rust 单二进制，无中心服务器（元数据以不可变事件形式存于 OSS，各设备对账合并）。

## 特性

- **双文件夹工作区**：`native/`（带参源文件 CATPart/sldprt/scdoc…）+ `neutral/`（不带参 STEP/STL/CGR…），按相对路径 stem 归组为"零件"
- **零件修订制**（参照 Siemens Teamcenter 的 Item/Revision/Dataset 模型）：BLAKE3 内容寻址对象永不覆盖，天然去重，历史与回滚零上传
- **冲突管理不做硬锁**：意向声明（48h 自动过期）+ 基线版本 stale 检测 + 三选项解决（用我的 / 用对方的 / 都保留=fork），任何选择都不丢数据
- **neutral 派生物冲突自动解决**（newest-wins），native 冲突才进人工队列
- **单零件 / 整装配 fork**：按依赖图批量零拷贝 fork
- **历史保留策略**：保护规则（current / base 引用 / open 冲突 / 最近 N 个修订 / T 天内 / pinned）+ dry-run 先行 + gc 事件广播
- **装配依赖图**：解析 neutral STEP 文本 PRODUCT 实体，受影响零件传递闭包查询
- **多工作区**：一台机器多个本地目录，各自绑定不同远程（bucket/前缀可不同）
- **本地 Web UI**（127.0.0.1）：工作区看板、init 引导、OSS 测试连接、冲突队列、依赖图、裁剪

## 构建

```bash
cargo build --release   # 产物 target/release/pdm.exe
```

## 使用

```bash
pdm serve --port 8420        # 常驻服务，浏览器打开 http://127.0.0.1:8420
pdm init <目录> --endpoint oss-cn-hangzhou.aliyuncs.com --bucket <bucket> \
    --ak <AccessKeyId> --sk <AccessKeySecret> [--prefix <对象前缀>] [--user <名字>]
```

- OSS 配置字段与 ltl-manage 的 `.env` 完全对齐，可直接复用凭证；建议使用最小化 RAM 子账号（单 bucket + 单前缀）
- 离线测试：`--backend local --store-root <共享目录>`，两台设备指向同一目录即完成同步
- 初始化自动扫描目录建基线修订；第二台设备 init 时自动接管远端历史（clone 语义），不重建基线

## 日常操作（Web UI 或 API）

| 操作 | 说明 |
|---|---|
| 同步 | 60s 自动拉取 + 手动按钮；文件被 CAD 占用时自动降级"待更新" |
| 冲突 | 侧栏队列，三选项解决后自动广播到网盘，对端自动消账 |
| 回滚 | 指针移动 + 工作区落盘，回滚后再编辑基于回滚点 |
| fork | 零拷贝新零件；整装配按依赖图连带 fork |
| 裁剪 | dry-run 清单 → 确认执行 → gc 事件广播各设备 |

完整设计（冲突管理/同步协议/数据模型/保留策略）见 [docs/design.md](docs/design.md)。

## 授权说明

OSS 凭证请使用最小化 RAM 子账号（仅目标 bucket + 前缀的读写删权限），不要复用主账号 AK。

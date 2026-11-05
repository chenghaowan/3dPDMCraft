# PDM Lite 设计方案

**版本**: v0.8（2026-11-06）
**实施状态**: M0 + M1 + M2 已实现并冒烟通过。**catia2stp 转换器已按决策移除**（效果不佳，功能取消）：neutral 保持人工导出，stem 归组约定为永久方案；neutral 冲突按"自动 newest-wins"落地（v0.8 实现）。剩余：真双机 OSS 实测、OSS 生命周期规则（运维配置）、缩略图（远期，需 CAD 内核）。
**变更**: 相比 Action Plan 初稿——① 冲突管理参照 Siemens Teamcenter 重新设计（初稿的"检测后弹窗二选一"升级为意向声明 + 基线版本 + 保双份默认）；② 工作区改为双文件夹布局（带参 / 不带参）；③ 补齐 pull / manifest 同步路径；④ 数据模型从"文件版本制"升级为"零件修订制"；⑤ 新增历史保留策略（§7，存储回收）；⑥ 拉取更新分级策略 + stale 预警（§5）；⑦ 新增方案分支 fork（§6.6），冲突"都保留"统一为实现为隐式 fork；⑧ 定应用形态（常驻进程 + 本地 Web UI）并新增配置页与初始化设计（§8）；⑨ 支持一台机器多个工作区、各自绑定不同远程（§8.1/8.2/8.3：机器级注册表 + 机器级 device_id + 单进程多工作区）。

---

## 1. 目标与非目标

**目标**
- 本地工作目录，CAD 软件无感直接访问
- 一台机器支持多个工作区，各自绑定不同远程（OSS bucket/前缀可各不相同）
- 阿里云 OSS 同步（对象内容寻址、永不覆盖）
- 零件级修订历史（Teamcenter Item Revision 模型的轻量版）
- 多设备同步（双电脑起步）
- 冲突：事前可见（意向声明）+ 事中检测（基线对比）+ 友好解决（保双份、不丢数据）
- 版本回退
- 历史保留策略：受控裁剪旧修订，控制 OSS 空间与本机占用（§7）

**非目标（明确不做）**
- CAD Feature Merge
- 自动冲突解决（native 文件；neutral 派生文件例外，见 §6.4）
- 硬性文件锁（改用带过期时间的软意向声明）
- 权限系统 / 审批流
- 多人协作（M1 只做双电脑，跑稳后再加 user 维度）

---

## 2. 工作区布局：双文件夹（带参 / 不带参）

CAD 数据按"源 vs 派生"分两个顶层目录：

```
Workspace/
  native/                      # 带参（可编辑源文件，source of truth）
    Assembly1.CATProduct
    Housing.CATPart
    Parts/
      Bracket.CATPart
  neutral/                     # 不带参（中性交换/轻量化格式，派生物）
    Assembly1.step
    Housing.step
    Parts/
      Bracket.step
```

| | native（带参） | neutral（不带参） |
|---|---|---|
| 扩展名 | `.CATPart` `.CATProduct` `.sldprt` `.sldasm` `.scdoc` `.prt` | `.step` `.stp` `.stl` `.jt` `.cgr` `.x_t` |
| 地位 | 源文件，人工编辑 | 派生物，由 native 导出 |
| 版本 | 每次保存产生修订 | 跟随对应 native 修订 |
| 冲突 | 人工决策（进冲突队列） | 自动解决（newest-wins + 解决事件广播），不弹窗 |
| 消费端 | 设计师机器 | 后续可设为只读（发布-订阅） |

**零件（part）归组规则（MVP）**：同一 stem 的 native + neutral 文件归属同一零件，按**相对路径 stem** 匹配：
`native/Parts/Bracket.CATPart` + `neutral/Parts/Bracket.step` → part `Parts/Bracket`。
子目录结构保持一致。原计划的 catia2stp 自动导出转换器已移除（2026-11-06 决策：效果不佳），neutral 为人工导出，stem 归组即永久方案。

**设计含义**：冲突处理成本与数据可恢复性按文件夹区分——native 是不可再生的人工成果，冲突必须人决策且永不覆盖丢弃；neutral 可再生成，冲突不值得打断用户。这是双文件夹要求带来的最大架构收益。

**OSS 不区分文件夹**：对象存储始终内容寻址（`objects/<2>/<2>/<hash>`），native/neutral 之分只存在于工作区与元数据（representation kind）层面。

---

## 3. 数据模型（SQLite, schema v2）

概念映射 Teamcenter：**part ≈ Item，revision ≈ Item Revision，representation ≈ Dataset**。版本挂在零件上而不是文件上，否则 `Housing.CATPart` v5 和 `Housing.step` v3 在历史里对不上。

```sql
-- 零件（逻辑主体）
CREATE TABLE parts (
  id          TEXT PRIMARY KEY,          -- uuid
  name        TEXT NOT NULL UNIQUE,      -- 相对路径 stem, 如 "Parts/Bracket"
  deleted_at  TEXT,                      -- 墓碑（删除零件 = 写入 tombstone 事件）
  created_at  TEXT NOT NULL
);

-- 修订（版本主体）。seq = parent.seq + 1；冲突子修订同 seq，展示按 (seq, device_id, created_at)
CREATE TABLE revisions (
  id          TEXT PRIMARY KEY,          -- uuid
  part_id     TEXT NOT NULL REFERENCES parts(id),
  seq         INTEGER NOT NULL,
  parent_id   TEXT,                      -- 父修订；回滚后再编辑 = parent 指向回滚点（Git 式）
  author      TEXT NOT NULL,
  device_id   TEXT NOT NULL,             -- 第一天就有：双机同步 + 冲突展示必需
  message     TEXT DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'current',  -- current | archived | conflict
  pinned      INTEGER NOT NULL DEFAULT 0,       -- 用户钉住（保留策略跳过），发布/里程碑快照用
  created_at  TEXT NOT NULL
);
CREATE INDEX idx_rev_part ON revisions(part_id, seq);

-- 表示（一次修订实际包含的文件内容；未变更的表示继承上一修订）
CREATE TABLE representations (
  revision_id TEXT NOT NULL REFERENCES revisions(id),
  kind        TEXT NOT NULL,             -- native | neutral | thumbnail | cgr
  rel_path    TEXT NOT NULL,             -- 工作区相对路径，正斜杠
  object_hash TEXT NOT NULL,             -- BLAKE3
  size        INTEGER NOT NULL,
  PRIMARY KEY (revision_id, kind, rel_path)
);

-- 本地文件状态（缓存，不参与同步）
CREATE TABLE files (
  rel_path            TEXT PRIMARY KEY,  -- 归一化：正斜杠、去 ..、大小写按磁盘原样
  part_id             TEXT NOT NULL REFERENCES parts(id),
  kind                TEXT NOT NULL,
  current_revision_id TEXT,              -- 已确认同步到的修订
  base_revision_id    TEXT,              -- 本地编辑基线（stale 检测用，见 §6.3）
  local_state         TEXT NOT NULL DEFAULT 'synced',  -- synced | modified | conflicted
  content_hash        TEXT,              -- 脏检查缓存，避免重复 BLAKE3
  updated_at          TEXT
);

-- 对象（OSS 去重账本）
CREATE TABLE objects (
  hash         TEXT PRIMARY KEY,
  size         INTEGER NOT NULL,
  upload_state TEXT NOT NULL DEFAULT 'pending',  -- pending | done（重试队列）
  deleted_at   TEXT,                             -- 保留策略裁剪标记（§7）
  uploaded_at  TEXT
);

-- 编辑意向（软检出，带过期时间，见 §6.3）
CREATE TABLE reservations (
  part_id    TEXT NOT NULL,
  device_id  TEXT NOT NULL,
  author     TEXT NOT NULL,
  expires_at TEXT NOT NULL,              -- 替代 ltl-manage "每日04:00清零"：无时区问题，客户端本地过滤
  created_at TEXT NOT NULL
);
```

关键不变量：
- **先传对象、后写修订/事件**——不允许出现指向缺失对象的修订。
- **对象不可变；删除仅走保留策略**（§7）：日常操作（保存、删除文件、冲突解决）永不删对象——对比 ltl-manage `sync_dir` 删除时顺手物理删 OSS 对象（`api.py:100-111`），PDM 禁止；空间回收只能通过 §7 的受控裁剪路径。
- etag/hash 一律基于**原始明文内容**（ltl-manage `sync_dir/models.py:20-25` 的教训：同步对比必须基于明文）。
- SQLite 开 WAL；watcher 事件与用户操作（回滚/解决冲突）走同一条 tokio mpsc 队列串行化。

---

## 4. 对象存储与元数据同步

### 4.1 OSS 布局

```
solidcraft-pdm/
  objects/aa/aa11bb.../          # 内容寻址，永不覆盖（两设备并发传同 hash = 同 key 同内容，幂等）
  meta/revisions/<revision_id>.json    # 修订清单（不可变，追加式）
  meta/tombstones/<uuid>.json          # 删除墓碑（不可变）
  meta/reservations/<uuid>.json        # 意向（带 expires_at，任意客户端可惰性回收过期项）
  meta/gc/<uuid>.json                  # 裁剪记录（不可变，广播给其他设备对账本地缓存）
```

元数据同步机制：**events manifest**。每台设备把本地产生的事件（新修订 / 墓碑 / 意向）写成不可变 JSON 对象，其他设备定期 `list` 前缀 → 增量拉取 → 合并进本地 SQLite。本地 SQLite 降级为缓存/索引，OSS 是唯一事实源。没有中心服务器，任何一台机器在线即可完成同步。

对比 ltl-manage `sync_dir`：那边是中心服务器架构（PostgreSQL 单行计数器发全局版本号，`api.py:63-71`），脱离服务器即瘫。PDM 用"不可变事件对象 + 客户端合并"替代，代价是失去全局单调版本号，换来无服务器依赖（§3 的 seq 链即为此补的排序方案）。

### 4.2 OSS 模块：从 ltl-manage 移植

`ltl-manage/backend/src/app/modules/file/oss.py`（Python + oss2，服务端模块）不能照搬：语言/运行时不同（Rust + opendal）、部署形态不同（每台工作机客户端）、key 方案不同（那边是 `{prefix}/{sub_dir}/{yymm}/{ts}_{uuid}` 时间戳随机名，面向"传一次、签名 URL 分发"；PDM 必须内容寻址）。

**移植的是三样东西**：

1. **配置层字段名原样对齐**（`.env` 直接复用）：
   `OSS_AccessKeyId` / `OSS_AccessKeySecret` / `OSS_EndPoint` / `OSS_BUCKET`，外加"未配置则禁用并警告，而不是崩溃"的降级模式（`oss.py:15-31`）。
2. **能力面四个操作**，opendal 各一个方法，模块 ~100 行：

   ```rust
   // storage/oss.rs —— 构建一次全局共享（oss.py 每次 _get_bucket() 重建，勿仿）
   pub fn oss_operator(cfg: &OssConfig) -> opendal::Result<Operator> {
       let b = Oss::default()
           .endpoint(&format!("https://{}", cfg.oss_end_point))
           .access_key_id(&cfg.oss_access_key_id)
           .secret_access_key(&cfg.oss_access_key_secret)
           .bucket(&cfg.oss_bucket)
           .root(&cfg.oss_path);
       Operator::new(b)?.finish()
   }
   pub fn object_key(hash: &str) -> String { format!("objects/{}/{}/{}", &hash[..2], &hash[2..4], hash) }
   // op.write(key, reader)  大文件自动分片（STEP 上百 MB 必需）
   // op.read / op.delete / op.presign_read   ↔ oss.py 的 upload/download/delete/presigned_url
   ```
3. **它踩坑踩出来的细节**：presign 带 `response-content-disposition`（下载文件名正确）、上传显式 Content-Type、基于明文计算校验和。

**实现期补充（v0.7）**：local 后端支持 `store_root`（共享目录），两台设备指向同一目录即模拟双机同步，也是 NAS/共享盘部署形态；事件拉取为 60s 定时 + UI 手动按钮，applied_events 表保证幂等。

不采用 Python sidecar 方案：给 Rust 工具挂 Python 运行时依赖，而 opendal 已覆盖全部能力。

凭证安全：AK/SK 落到每台工作机，RAM 策略最小化（单 bucket + 单前缀），不复用主账号 AK（ltl-manage 配置中留有 `OSS_RAM_ROLE_ARN` 字段即为此教训）。

---

## 5. 同步流程

### 上传（本机有变更）
```
watcher 事件 → debounce 静默窗(秒级,大STEP加长) → 二次哈希确认(防CAD保存中截断)
  → BLAKE3 → 与 files.content_hash 比对
  → 未变: no-op（回滚落盘、pull 落盘都靠这条规则防回环）
  → 已变: 对象已传? 否则上传 → 建修订(seq=parent.seq+1) → 写 revisions.json 事件 → 更新 files
```
- debounce 窗口内同 part 的多文件变更**聚合为一个修订**（native+neutral 同时导出 = 一个修订两个 representation）。
- 忽略模式：`~$*` `*.sld~` `*.bak` `.CATAnalysis临时` 自动备份等。
- 上传前对账：发现对方有更新版本或未过期意向 → 非阻塞提示条（不拦截）。

### 拉取（别人的变更，Action Plan 初稿完全缺失的一半）
```
定时/手动 → list meta/revisions|tombstones|reservations → 与本地 last_seen 比对
  → 应用墓碑: 删本地文件+标记（无条件应用，先于一切）
  → 对每个远端修订按"更新分级"处理（见下表）
  → 落盘前记录 hash，watcher 按幂等规则跳过
  → 对账: files.local_state==modified 且 current != base → 进冲突队列（§6）
```

**更新分级**（"别人更新时可以选择是否更新"——可以选，但必须分级：全都可以选会让双机基线越走越远，冲突越积越多）：

| 本地状态 | 策略 | 理由 |
|---|---|---|
| synced 且文件未被 CAD 占用 | **自动更新**（设置中可改为"确认后更新"） | 无本地改动可丢失，拖着不更只会制造冲突 |
| synced 但文件被 CAD 打开 | **推迟**：列表角标"有新版本"，保存关闭后一键更新 | 避免 CAD 应用内文件被换底；os.replace 失败自动降级为此档 |
| modified（有本地改动） | **必须人工选**：立即进冲突队列（保双份三选项） | 本地改动 vs 远端更新，只有人能裁决 |
| 正基于旧版本编辑 | **stale 预警**：开改时立刻提示"对方已更新，你基于旧版修改" | 把"以哪个为准"的决策提前到投入工作量之前（Teamcenter 红旗的时点） |

批量界面：远端更新统一进"待更新"列表，支持全部更新 / 逐个选择，不做逐文件弹窗轰炸。

删除 = 写墓碑事件占一个新位置（`sync_dir` 已验证的模式），不物理删对象。

---

## 6. 冲突管理（核心章节）

### 6.1 ltl-manage 现状问题（代码依据）

| 问题 | 位置 | 后果 |
|---|---|---|
| 上传无条件覆盖，无基线校验 | `sync_dir/api.py:462-477`（`row.version = version` 无任何前置比对） | 两人同改 = 静默 last-write-wins，后传者悄悄覆盖先传者 |
| 无历史：一行一 path | `sync_dir/models.py` SyncFile | 被覆盖方内容**永久丢失**，无从找回 |
| 删除物理删 OSS 对象 | `sync_dir/api.py:100-111` `_remove_stored` | 连"翻旧账"的可能都没有 |
| 意向/锁每日 04:00 清零 | `sync_dir/api.py:177-191` | 时区硬编码；跨天编辑保护失效；过期粒度粗 |
| 中心服务器强依赖 | PostgreSQL 计数器发号 | 服务器挂 = 全员停摆 |

结论：ltl-manage 不是"冲突解决不友好"，而是**没有冲突解决**——只有静默覆盖加事后无解。PDM Lite 的底线因此定为：**任何操作都不静默丢弃内容，任何冲突都有可追溯的双份留存。**

### 6.2 Teamcenter 的做法（参照系）

- **Reservation（检出）**：修改前检出，独占修改权且团队可见；他人只能只读打开；崩溃残留由管理员 Cancel Checkout 解除，CICO 行为可用偏好配置（[Siemens Community: Cancel Checkout](https://community.sw.siemens.com/s/article/Checkout-and-Check-in-Understanding-Cancel-Checkout-in-Teamcenter-and-a-preference-to-configure-the-behaviour)、[Teamcenter Integration: Check-In/Check-Out Policy](https://community.sw.siemens.com/s/article/teamcenter-integration-new-check-in-check-out-policy)）。
- **stale 检测**：NX 集成中基础修订落后时显示红旗状态标记，检入前提醒（[Swoosh Tech: Create Revisions in Teamcenter](https://www.swooshtech.com/2021/02/25/how-to-create-revisions-in-teamcenter)）。
- **冲突 remedy = 新建修订而非覆盖**："Save As new revision" 是各 CAD 集成（NX、TIA Portal gateway）的标准出路，新修订从上一版复制而来（[Siemens Designcenter: Revising a Design](https://blogs.sw.siemens.com/designcenter/nx-tips-and-tricks-revising-designs)、[TIA Portal: Save as new revision](https://docs.tia.siemens.cloud/r/en-us/v21/tia-portal-teamcenter-gateway/workflows-between-the-tia-portal-teamcenter-gateway-and-tia-portal-multiuser-engineering-tia-portal-projects/saving-the-server-project-as-a-new-revision-in-teamcenter)）。
- 检出/检入模型综述见 [PLMCoach: Teamcenter Guide](https://plmcoach.com/siemens-teamcenter-plm-guide)。

**提炼三条核心思想**：① 预防优先——修改意图对团队可见；② 数据永不丢失——冲突的出路是"另存为新修订"，不是覆盖；③ 决策带完整上下文——谁、何时、哪个修订。

### 6.3 PDM Lite 的取舍（无硬锁版 Teamcenter）

不做硬锁（Action Plan 立场不变），但把 Teamcenter 三思想做轻量版：

1. **意向声明（软检出，自动化）**：watcher 检测到文件 modified 时**自动**发布 reservation 事件（不需要用户手动"检出"——这是对 ltl-manage 手动 claim 的关键改进）；带 `expires_at`（默认 48h，编辑中自动续期）替代"每日 04:00 清零"。他人上传时若目标零件有未过期意向 → 非阻塞警示条，不拦截。
2. **基线版本（stale 检测）**：文件进入 modified 时记录 `base_revision_id`；对账发现云端 current ≠ base → 冲突。这就是 Teamcenter 红旗的等价物。
3. **解决时默认保双份**（= "Save As new revision" 的精神）。

### 6.4 冲突解决 UX

**原则：不阻塞、不丢数据、带上下文。**

- **不弹阻塞对话框**。冲突进入侧边栏"冲突队列"，工作区文件保持本地内容不动，用户继续干活。
- 队列条目上下文：零件名、双方 author / device_id / 时间、大小变化、变更的 representation（native 还是 neutral）。时间较新的一方标"推荐"。（缩略图并排对比为远期项，需 CAD 内核或用户自备截图）
- 每条三个动作（数据不变式：三个选项都不删除任何对象）：
  - **[用我的]** 我的修订设为 current，对方 archived
  - **[用对方的]** 下载对方对象覆盖工作区；**我的修改已作为修订留存，可随时找回**（与 ltl-manage 的本质区别）
  - **[都保留]** 我的以 sibling 修订存档，current 保持对方（实现上即隐式 fork，见 §6.6）
- 支持批量（全选"用我的"）与延迟处理（挂着不处理也完全合法）。
- **neutral 冲突不进队列**：自动 newest-wins + 日志 + 解决事件广播（派生物可再生，不值得打扰；v0.8 已实现）。

### 6.5 状态机

```
            watcher 检测变更
  synced ─────────────────────► modified ──上传成功──► synced
    ▲                             │
    │                    对账发现 current ≠ base
    │                             ▼
    └──── 用户选择(我的/对方/都保留) ── conflicted
```

回滚：current 指针指回历史修订（零上传），工作区从该修订落盘，`base=current`；此后再编辑产生的修订 parent 指向回滚点。

### 6.6 方案分支（fork）——多方案数据

"多个方案"（同一零件的 A/B 设计变体）用 **fork** 表达。内容寻址让 fork 近乎零成本：新方案的初始修订直接复用父修订的对象（同 hash，无复制）。

- **单零件 fork（M1）**：fork = 以某修订为起点创建**新零件**，记 `forked_from_revision_id` 血缘（历史 UI 可跳回原零件）；工作区落新文件名（`Housing.CATPart` → `Housing.方案B.CATPart`，stem 即新零件名 `Housing.方案B`），装配引用需手动重挂。
- **冲突的"都保留" = 隐式 fork**：§6.4 三选项中的"都保留"在实现上就是 fork，两个概念统一，不另造机制。
- **不做 Git 式分支切换**：切分支 = 换工作区文件内容，CAD 打开文件时极危险；fork 出独立文件名后各方案并行存在，不需要切换。
- **整装配 fork（暂缓至依赖图可用）**：装配按文件名引用零件，fork 装配必须连带 fork 全部被引用零件并改写引用——依赖装配依赖图（M2 / 原 Phase 3）。此前多方案装配只能人工复制目录，需在 UI 明确告知此限制。

---

## 7. 历史保留策略（存储回收）

### 7.1 存储分布先说清楚（硬件担忧的答案）

| 位置 | 存什么 | 增长性 |
|---|---|---|
| 本机磁盘 | 工作区**当前**文件 + SQLite 元数据（每修订 KB 级） | 小：历史大文件不落本机常驻，回滚时按需下载、验哈希、原子替换后即释放临时文件 |
| OSS 对象 | 每次保存一个全量 CAD 文件 | **大头**：STEP 装配百 MB 级 × 高频保存 |
| OSS 元数据 | 修订/墓碑/意向 JSON 事件 | 可忽略 |

因此：本机不需要"删历史"来腾空间（它本来就只有 current）；真正需要管理的是 OSS 增长。裁剪功能两头都管——OSS 删大对象（主收益），本地裁 SQLite 旧行 + VACUUM（次要）。

### 7.2 保护规则（硬约束，任何删除路径先过这关）

一个修订的对象**可删**当且仅当同时满足全部条件：

1. 不是任何零件的 **current** 修订
2. 不是本地 modified / conflicted 文件的 **base_revision_id**（冲突解决还要用它）
3. 未被未过期 **reservation** 引用
4. 不在该零件**最近 N 个修订**内（默认 N=10）**且** `created_at` 距今 > **T 天**（默认 90）
5. 未被 **pin**（钉住的发布/里程碑快照，Teamcenter baseline 思路）

规则命中即跳过，宁多留勿误删。neutral 表示可先于 native 裁剪（可从 native 重新导出），其保护窗口可以单独设短。

### 7.3 两级删除

- **裁剪对象（默认开启）**：删 OSS 大对象；SQLite 保留**残根**（revisions 行保留 id/seq/status='pruned'，representation 行删除）。历史 UI 显示 "v3 已按策略清理" 而非消失——修订链和工程可追溯性保持完整；回滚到已清理修订时明确提示"需从 native 重新导出"。
- **裁剪元数据（默认关闭）**：连残根和 OSS 事件 JSON 一起删。仅用于敏感场景（如误传文件的彻底清除）；若启用了第二备份（灾备桶），需同步处理，否则等于没删。

### 7.4 触发与执行

- **手动**（M1）：`pdm prune --dry-run` 先出清单（零件 / 修订 / 对象数 / 释放空间合计），确认后执行。**dry-run 是默认，不确认不删。**
- **自动**（M2 ✅）：config.toml 中 `auto_prune = true` + `auto_prune_hours = 24` 后按周期执行（沿用 §7.2 规则与 N/T 参数）。
- **OSS 生命周期规则**（运维配置，非应用代码）：bucket 控制台对 `objects/` 前缀配置"最后修改 180 天转低频 / 365 天转归档"；`meta/` 前缀保持标准存储（事件小且常读）。
- **广播**：裁剪动作写 `meta/gc/<uuid>.json` 事件；其他设备先对齐本地缓存行（对象标记 deleted），避免引用已删对象。
- **顺序与幂等**：OSS 删对象 → SQLite 置残根 → 写 gc 事件；对象已 404 视为成功，重试安全。
- 裁剪后 `VACUUM` 回收 SQLite 空间。

### 7.5 与"永不覆盖"的关系（表述修正）

- **对象不可变（永不覆盖）**：仍然成立，这是并发安全与去重的基石。
- **永久保留不再是承诺**：空间回收只能走本节受控路径（dry-run 清单 + 保护规则 + gc 广播）。ltl-manage 那种"删除文件时顺手物理删对象"（`sync_dir/api.py:100-111`）仍然禁止——那是无历史架构下的无奈，PDM 有历史，删除必须可审计。

---

## 8. 应用形态与配置初始化

### 8.1 应用形态（新增决策）

单二进制 `pdm`，两种运行态：
- `pdm serve`：常驻后台，**管理本机注册的多个工作区**（每个工作区一个 watcher + 同步引擎实例，tokio task 隔离，单个崩溃不影响其他），共享一个 Web UI 与端口；监听 `127.0.0.1:<port>`
- 配置与初始化：CLI `pdm init <path>` 或 UI 引导页，二选一

**多工作区模型**（一人多个本地目录，各对应多个远程）：
- 工作区 = 完全独立的同步单元：各自 `.pdm/`（库 + 配置）、各自 OSS 连接（bucket/前缀可不同）；工作区之间**互不相通**（不同步、不合并），如同两个独立仓库
- `device_id` 是**机器级**的（`%LOCALAPPDATA%/pdm/device_id`，一次生成），所有工作区共用——冲突展示与 tie-break 标识"哪台机器"，与工作区无关
- 工作区注册表：`%LOCALAPPDATA%/pdm/workspaces.toml`（path / name / added_at），serve 启动时逐个加载
- RAM 子账号仍按 bucket+前缀最小化：多远程 = 多条最小授权，各工作区配置各放各的 AK

**UI 选型：本地 Web 页面（axum + 内嵌静态页），不用 Tauri / egui / TUI。** 理由：配置页只是入口——冲突队列（§6.4）、"待更新"批量列表（§5）、历史/回滚、裁剪 dry-run 清单（§7.4）全是表格+按钮类交互，Web 一套技术栈全部复用；单二进制内嵌 HTML 无前端构建链，MVP 成本最低。仅绑定 127.0.0.1，不开局域网。

### 8.2 配置页

两级页面：**工作区列表（首页 / 看板）** → 进入某一工作区的配置页（三区块 + 引导流）。

**首页 · 工作区列表**：每个已注册工作区一行——名称、路径、OSS 连接状态、待更新数 / 冲突数角标、最后同步时间；操作：进入 / 添加新工作区（走 init 引导）/ 移除注册（只删注册，不动数据）。

**工作区配置页，三个区块：**

**① OSS 连接**

| 字段 | 说明 |
|---|---|
| EndPoint | 如 `oss-cn-hangzhou.aliyuncs.com` |
| Bucket | 如 `solidcraft-pdm` |
| AccessKeyId / AccessKeySecret | 字段名与 ltl-manage `.env` 对齐（§4.2）；支持粘贴 `.env` 片段自动解析填充 |
| 对象前缀 | 默认 bucket 根 |

- **[测试连接]**：HEAD bucket + 试传/读取/删除一个 `_probe/<uuid>` 探针对象，报告延迟与权限结论（读/写/删），失败给具体原因（AK 无效 / endpoint 不通 / 无写权限）
- **凭证安全**：页面只显示尾 4 位；落盘到 `.pdm/config.toml`；UI 明示"请使用最小化 RAM 子账号（单 bucket + 单前缀）"（§4.2）

**② 本地目录**
- 输入/选择 workspace 路径，显示磁盘剩余空间
- 状态检测：无 `.pdm/` → 可初始化；已有 → 显示"接管 / 只读查看"
- 含中文或空格的路径给出提示（挂 §11 风险 #9）

**③ 初始化（init）**
一键执行，分步显示进度：
1. 校验目录（拒绝嵌套 workspace、拒绝系统目录）
2. 建 `native/`、`neutral/`、`.pdm/`（SQLite、config.toml、ignore.conf）
3. 建库 + migrations（§3 schema v2）
4. **首扫建档**：递归扫描现有 CAD 文件 → BLAKE3 → 按 §2 stem 规则归组 parts → 每 part 建 seq=1 基线修订 → 上传对象 + 写修订事件；大目录逐文件显示进度与吞吐；可勾选"仅本地建索引，暂不上传"（默认关）
5. 生成一次性 `device_id`（uuid）；写忽略规则初始版（`~$*`、`*.sld~`、`*.bak` 等）到 `.pdm/ignore.conf`（可编辑）

完成页：零件数 / 文件数 / 对象数 / 上传流量合计，并写入机器级工作区注册表（§8.1）。

**重复初始化保护**：目标目录已有 `.pdm/` 默认拒绝，需显式"接管"；同一 OSS 前缀下出现第二个 `device_id` 首次接入时 UI 提示确认（可能是另一台电脑，也可能是误配）。

### 8.3 配置存储（两级）

- **机器级** `%LOCALAPPDATA%/pdm/`：`device_id`（一次生成，全工作区共用）、`workspaces.toml` 注册表（path / name / added_at）
- **工作区级** `<workspace>/.pdm/config.toml`：该工作区的 OSS 连接（bucket/前缀/AK 可各不相同）、UI 端口、更新策略开关（§5 自动/确认式）、裁剪参数 N/T（§7.2）
- 兼容 ltl-manage：支持从工作区同目录 `.env` 读取 `OSS_*` 字段作为初值，迁移零成本

---

## 9. 技术栈（沿用 Action Plan）

Rust stable / tokio / notify(-debouncer-full) / blake3 / opendal(OSS) / sqlx(SQLite, WAL) / serde / uuid / axum（本地 Web UI，rust-embed 内嵌静态页）/ clap（CLI）/ toml。

---

## 10. 里程碑

| 阶段 | 范围 | 验收 |
|---|---|---|
| **M0 单机 ✅（2026-11-06）** | 配置页（工作区列表 + init 引导，§8）；双文件夹布局 + part 归组；watcher+debounce+BLAKE3；SQLite schema v2；OSS 上传（移植 §4.2）；修订历史+回滚；回环抑制 | 全部通过（OSS 探针 694ms 用 ltl-manage 凭证实测） |
| **M1 双机 ✅（2026-11-06）** | events manifest 同步（pull：60s 定时 + 手动）；墓碑删除；意向声明（claim/release + 上传警示）；冲突队列 + 三选项解决 + 解决事件广播；更新分级 + stale 预警（pending/remote_ahead 角标）；单零件 fork（零拷贝）；历史裁剪（dry-run/执行 + gc 广播）；**init clone 语义**（第二台设备拉取远端事件接管历史，不重建基线——§8.2 的实现期补强） | 双工作区共享 store 实测：冲突解决零丢失（败者修订 archived 可找回）、占用文件 os.replace 失败自动降级 pending、fork 引用对象被裁剪引用计数保护 |
| **M2 提效 ✅（2026-11-06，转换器已移除）** | **neutral 冲突自动解决（newest-wins + 广播）**；**自动定期裁剪**（config: auto_prune / auto_prune_hours，§7.4）；**装配依赖图**（neutral STEP 文本解析 PRODUCT 名，启发式；impact 传递闭包查询）；**整装配 fork**（按依赖图批量 fork 可达零件；不改写装配内部引用，CAD 中需重挂）；OSS 生命周期规则 = 运维配置（bucket 控制台，见 §7.4 注） | 实测：依赖图 2 边 + impact 正确；neutral 分歧零冲突自动收敛；native 冲突回归正常进队列；整树 fork 3 零件并在对端同步 |

多人协作（user 维度、审批、权限）在 M1 双机跑稳后启动——维持 Action Plan 的分阶段判断。

---

## 11. 风险与开放问题

1. **同 stem 跨子目录歧义**：`native/Parts/Bracket.CATPart` 与 `native/Fasteners/Bracket.CATPart` 是否同一零件？当前方案按完整相对 stem 归组（`Parts/Bracket` ≠ `Fasteners/Bracket`），需要和实际装配目录结构对一次。
2. **debounce 参数需实测**：CATIA/SolidWorks 大装配保存时长差异大，静默窗口初值 3s + 二次哈希，按实测调。
3. ~~neutral 自动导出的触发时机~~（已随 catia2stp 移除而取消；neutral 为人工导出）。
4. **manifest 规模**：修订事件只增不减，单 bucket list 全量前缀在十万级事件后需要分页/压缩策略，MVP 不做，记为技术债。
5. **时钟偏差**：排序靠 seq 链 + device_id tie-break，不信任跨设备 created_at；展示时注明。
6. **OSS 成本**：每次 CAD 保存 = 新全量对象。由 §7 保留策略兜底（M1 手动裁剪、M2 自动策略）；M0 起记录 bucket 增长速度，用于校准默认 N/T 参数。
7. **裁剪误删**：OSS 对象删除不可逆。防线 = dry-run 默认先行 + 保护规则宁多留勿误删 + gc 事件广播对账；§7.2 五条规则（尤其 current / base_revision 引用场景）需专门测试覆盖。
8. **自动更新撞上 CAD 占用文件**：Windows 上对被占用文件 os.replace 可能失败或换底后 CAD 再保存产生陈旧版本。防线 = 更新分级（§5）：replace 失败自动降级"推迟"，配合已知锁文件特征（SolidWorks `~$*`）；CATIA 无锁文件，以 replace 失败为兜底信号，需在 M0 用真实 CATIA 保存流程验证。
9. **路径兼容**：workspace 路径含中文/空格在 CAD 下的兼容性需实测；init 时检测并提示（§8.2）。

# Rust development SOP

日常迭代先获得与修改范围匹配的反馈，再验证实际行为；完整工作区和四平台
打包留到候选收敛时执行。应用性能分析另见
[performance profiling](performance-profiling.md)，本页只管理开发等待时间。

## 1. 固定构建环境，保留活动缓存

- 工具链与 CI 保持一致，目前为 Rust **1.93.1**。使用 `--locked`。
- 日常使用 dev/test；现有配置保留应用的有限调试信息，关闭依赖的调试信息。
  正式 release 的 LTO 和单 codegen unit 留给发包，不用于每轮界面或逻辑检查。
- 同一工作树固定 target 目录、工具链、features、profile 和增量设置。dev/test
  沿用增量编译默认值；CI 使用已有的 `CARGO_INCREMENTAL=0`。来回切换设置会
  破坏可复用性，不是免费加速。
- 每个活动工作树保留自己的 target。现有 wrapper 刻意隔离 workspace 产物，
  不能只为省空间合并目录；共享前须验证源文件变化后的产物身份与失效规则。
  工具链下载和 Cargo registry 可共用。
- 8 GiB 共享开发机使用既有 `.emp-cargo.sh`：固定工具链、每工作树 target、
  跨会话构建锁、`CARGO_BUILD_JOBS=1`、`RUST_TEST_THREADS=1`。下面示例中的
  `cargo +1.93.1` 在该机由此入口替代，不再套额外的 Cargo 任务或并行编译。
  其他机器可用标准 Cargo 命令；并发数按实测内存余量调整。

先看 `git status --short` 和已有 diff，确认模块及工作树所有权。慢请求、等待
构建锁、编译、链接、测试和外部服务等待分别判断；终端等待不能全算作 Rust 编译。

## 2. 每轮只走需要的路径

| 修改范围 | 日常反馈 | 候选验收 |
| --- | --- | --- |
| 文档 | diff、链接、示例命令检查 | 涉及命令行为时检查对应接口 |
| JS/CSS/文案 | 受影响的现有 Node 合同检查、Shadow 刷新 | 重建一次候选，运行对应正式资产浏览器 E2E |
| 单个 Rust 模块 | 对应 crate 的 check 或已有行为测试 | 跨模块边界检查及受影响 E2E |
| 路由、协议、历史、存储或恢复 | 对应 crate、调用方和失败路径测试 | 假上游或隔离真实引擎的完整工作流 |
| Windows/macOS 启动、更新、系统匣 | 本地可验证的共享逻辑 | 官方原生 CI 包及受影响平台验收 |

快速类型反馈：

```sh
cargo +1.93.1 check --locked -p emp-app
```

需要测试某个集成边界时，直接运行对应测试文件；Cargo 会编译其依赖：

```sh
cargo +1.93.1 test --locked -p emp-codex --test history_contract
```

模块内已有回归可再限定 `--lib` 和测试过滤词。过滤后确认实际运行了目标用例，
零测试不算通过。如果本轮马上需要测试结果，可直接运行测试，不必机械地先
check，再全量 clippy、test、build。`check` 跳过最终代码生成，不能替代可执行
文件或 E2E 验收。

前端现有检查：按变更选择文案、行为或生命周期合同。

```sh
node crates/emp-app/tests/web_copy_contract.cjs
node crates/emp-app/tests/web_features_contract.cjs
node crates/emp-app/tests/web_lifecycle_contract.cjs
```

纯界面调整先用 [Shadow](../scripts/shadow_frontend/README.md)。它直接读取产品
前端源文件，刷新即可看到修改。现有候选后端可继续用于未变化的后端接口；涉及
后端行为时才重建候选。默认真实数据预览保持只读，不能用演示数据证明账单正确。

## 3. 收敛后构建一次，验证实际工作流

需要运行后端或正式内嵌资产时构建 dev 候选：

```sh
cargo +1.93.1 build --locked -p emp-app --bin EMP --timings
```

后续验收复用这次生成的同一候选。标准 Cargo 路径为 `target/debug/EMP`；使用
wrapper 时从其工作树 target 取产物。先确认版本和哈希，不能把旧执行文件当作
刚修改的代码。正式前端使用 `include_bytes!` 嵌入，Shadow 刷新通过后仍需这次
构建验证嵌入后的行为。

例如历史恢复变更使用现有验收入口：

```sh
python3 scripts/shadow_frontend/e2e_native_history_restore.py \
  --emp /absolute/path/to/candidate/EMP --output /tmp/emp-history-e2e.json
```

从 [Shadow 验收说明](../scripts/shadow_frontend/README.md) 选择受影响工作流，
观察真实结果、状态变化和失败后的恢复。使用隔离配置、临时目录、随机端口和
假上游。模型实测、远程部署和正式实例重启需要对应授权；不能为跑测试接管正在
使用的 EMP 或 Codex。E2E 出现新失败时定位和补充相关检查，证据充分后停止扩测。

临时目录使用短路径，避免 Unix socket 超过系统长度限制。后台测试进程清除
继承的桌面会话变量，防止桌面自动分离影响进程回收。假登录 CLI 测试还需要
隔离系统安装的 ChatGPT 引擎：只改 PATH 不足以改变 EMP 的引擎优先顺序。
Linux 可用 Bubblewrap 为测试进程单独遮蔽 `/usr/lib/chatgpt`，保留临时目录和
结果目录可写；不要重命名或卸载用户正在使用的 App 来适配测试。

## 4. 候选与发布

提交前运行格式检查和受影响包的严格 Clippy，保留前面的行为验收结果：

```sh
cargo +1.93.1 fmt --all -- --check
cargo +1.93.1 clippy --locked -p emp-app --all-targets -- -D warnings
git diff --check
```

按实际受影响包调整 `-p`，而非所有变更都固定检查 emp-app。候选代码收敛后，
让官方 [Runtime checks](../.github/workflows/runtime-compatibility.yml) 执行完整
工作区检查。相同 commit、配置和目标平台已有成功证据时复用；新增代码或环境
变化则重新验证受影响范围。CI 偶发失败先判断原因，允许重跑失败 job 时保留
其他成功 job，不重新提交无变化代码来触发全套任务。

Windows、Linux、macOS Intel/Apple Silicon 安装包由官方
[Package](../.github/workflows/package.yml) 原生构建。兼容性电脑只运行候选或正式
产物，不部署 Rust 编译环境。发布前按现有工作流执行标签打包、原生 smoke 和
公开产物校验；本 SOP 不跳过现有发布门禁，也不触发任何发布。

Actions 空间维护区分构建附件与编译缓存，两者配额独立。保留正式 Release、
CI 运行记录和日志；已被正式版替代的候选附件及旧版本的 Actions 副本可在核对后
清理。缓存优先清理已关闭 PR、旧标签和已结束分支的条目，保留主分支、活动开发
分支及当前发布需要的缓存。现有构建附件保留期为一天，避免为省空间清空仍有用
的编译缓存。清理后以条目回读验证删除，账务汇总可能延迟更新。

## 5. 空间清理与测量

活动缓存是可重建的开发资产，不是每次发布后的必删临时文件。清除 `deps`、
`build`、`incremental` 和 `.fingerprint` 会让下次重新编译；仅保留 EMP 执行文件
无法保留 Rust 编译缓存。

磁盘可用空间低于 **20 GiB** 时先检查占用，此值是本地维护提醒，不是自动删除
条件。优先检查已结束工作树的 target、重复下载包和过期验收临时产物；每次删除
仍须已有清理授权、确认精确目录以及进程/构建锁状态。保留活动工作树的缓存，
不清理用户状态、工具链或正在使用的产物。

进一步优化前，用相同机器、工具链、flags 和输入分别记录：

| 场景 | 要回答的问题 |
| --- | --- |
| 自然冷构建 | 首次依赖、代码生成和链接各耗时多少？ |
| 无修改重跑 | Cargo 是否复用产物？是否等锁或无故判脏？ |
| 一次代表性小改 | 实际重编译了哪些 crate，链接占多少时间？ |

记录总耗时、Cargo `--timings` 报告、内存/交换使用和 target 大小。报告位于所选
target 的 `cargo-timings/`。不为制造基线删除当前缓存；自然冷构建或已授权的
隔离构建足够。CI 的 build/test 步骤包含编译和执行，不能直接当作纯编译时间。

只有测量证明瓶颈后才试更换链接器、sccache 或提高 jobs：链接器主要针对链接，
sccache 的命中条件与 Rust 增量编译不同。一次改变一个因素，对照无修改重跑和
代表性小改的耗时、内存及磁盘，再决定是否保留；不增加两套长期构建配置。

参考：[Cargo check](https://doc.rust-lang.org/cargo/commands/cargo-check.html)、
[profiles / incremental](https://doc.rust-lang.org/cargo/reference/profiles.html)、
[build timings](https://doc.rust-lang.org/cargo/commands/cargo-build.html#compilation-options)。

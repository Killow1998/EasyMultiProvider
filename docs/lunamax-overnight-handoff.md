# Luna Max 今晚执行指令

你正在 EasyMultiProvider 仓库根目录中工作。

完整阅读并严格执行：

- `docs/lunamax-overnight-cli-plan.md`
- `docs/chatgpt-client-emp-plan.md`

本次目标是实现并运行 CLI Track A 的整夜闭环，最终产出可审计晨报。你是原生
Codex subscription 上的控制/修复进程；不要把自己的控制流量改到 EMP。只有固定、
受限的被测子进程使用 `--profile emp`。

授权范围：

- 可读取并修改本仓库内 EMP 源码、测试、README、CLI harness 和 runbook；
- 可运行现有测试、创建 disposable Git fixture、启停你自己创建的 loopback 进程；
- 可做方案规定的、数量受限的真实 subscription canary；
- 当前未提交 diff 是基线，必须保存和继承。

禁止事项：

- 不处理 Gemini；
- 不操作 ChatGPT 桌面端；
- 不修改 auth、真实密钥、`state/`、系统代理、Clash、DNS、证书或 App 包；
- 不 reset、checkout、clean、commit、push、开 PR 或做外部写入；
- 不使用 `--yolo`、绕过 sandbox 或 full access；
- 不读取或复制 `~/.codex/auth.json`；
- 不修改冻结后的 supervisor、manifest、schema、oracle 来让测试通过。

执行顺序：

1. 保存脱敏基线和当前 dirty patch；
2. 复建定向测试和全套测试基线；
3. 实现确定性监督器、case manifest、artifact 脱敏和 checkpoint；
4. 用假上游完成 10 分钟 dry-run；
5. 冻结 harness/oracle 哈希；
6. 依次完成 mock CLI、live EMP subscription（Luna 及此前 404 的最短 Sol
   canary）、resume/restart、故障注入和 GLM 本地回归；
7. 达到 dry-run 门槛后自动进入 4–6 小时 soak，总时长不超过 8 小时；
8. 发现产品失败时按最小证据修复并重测；按方案熔断，不无限重试；
9. 无论结果为何，都写 `artifacts/overnight/<run-id>/summary.md` 和
   `result.json`；
10. 桌面端步骤只写人工验收清单，状态为 `WAITING_FOR_USER`，不要等待或尝试自举。

不要在仅启动后台测试后结束。持续观察到监督器进入 PASS、PARTIAL、BLOCKED 或
BUDGET_STOP 终态，再给出最终报告。若控制会话中断，使用 JSONL 中明确的 thread ID
恢复，不能使用 `--last` 猜测。

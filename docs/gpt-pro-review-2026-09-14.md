# GPT Pro 外部审查记录（2026-09-14）

- 来源：用户提供的 GPT Pro 评价，以下原文完整保留。
- 原文声明的审查基线：EMP v0.11.1，提交 `5fcbdbb`。
- 状态：待逐项独立核验。本记录不代表 Point 已确认相关结论、完成修复或验证当前本地修改；版本、上游状态和优先级均为原评价的判断。
- 本次仅归档，不执行原文中的修改建议。

---

李闲，我检查了 GitHub 当前 `main`：**EMP 已经是 v0.11.1，提交 `5fcbdbb`，最后更新于 2026 年 9 月 14 日**。这次是按当前代码、测试和工作流审查，不再沿用前面通知中的 v0.9.98 基线。Codex 最新稳定版仍是 **0.154.0**；下面会区分“稳定版已有变化”和“已合入上游 main、尚不能当作稳定版行为”的变化。 ([GitHub][1])

**结论：EMP 不需要推翻架构，但现在有三处值得优先修的小范围问题，比继续增加 Provider 或直接开启 `tool_search` 更重要。**

| 优先级                     | 改进项                                            | 判断                                         |
| ----------------------- | ---------------------------------------------- | ------------------------------------------ |
| **P0：下一次修复版**           | 外部模型继承了不属于自己的原生能力字段                            | 当前代码中能确认，0.154 新字段已触发这个设计缺口                |
| **P0：扩大 runtime 支持范围前** | 把缓存亲和标识 `session-id` 当成线程身份                    | 与上游新合并的 fork 行为直接冲突                        |
| **P0：工具转换正确性**          | 不同 namespace 的同名工具被静默合并                        | 当前 portable 转换路径能确认；现有 collaboration 修复不覆盖 |
| **P1**                  | 完整建模 deferred tools / `tool_search`，补 WS 作用域检查 | 不应靠一个布尔开关或者盲目重试解决                          |
| **P1**                  | 把真实 Codex 协议测试变成持续集成门槛                         | 已有测试基础，接入比再写一套兼容框架划算                       |

这里的 P0 指“应优先修、或在声明兼容新 runtime 前修”，**不是说目前所有正常会话都会失败**。

## 一、外部模型目录仍在继承原生能力：这次已经出现具体漏项

**位置：`easy_multi_provider/catalog.py` → `_external_entry()`。**

目前的策略是先：

```python
entry = copy.deepcopy(template)
```

然后删除或覆盖一批原生字段。你已经正确处理了 `comp_hash`、套餐限制、服务档位、`use_responses_lite`、部分实验工具等，但**没有处理 `supports_experimental_context`**。而 `build_catalog()` 的模板来自当前原生模型列表的第一项，因此外部模型最终得到什么隐藏能力，还会受到原生模板选择的影响。

这恰好碰上 Codex **#43147**：0.154 引入了 `ModelInfo.supports_experimental_context`，默认 `false`，为内置 Astra 模型启用；上下文管理的启动条件现在会检查该字段。上游还专门区分了“新建子任务”和“携带历史的 fork”的配置继承方式。 ([GitHub][1])

于是，当前代码存在这个确定的推导：

```text
原生模板 supports_experimental_context = true
                     ↓ deepcopy
某个未经验证的外部模型也得到 true
```

**可以确认的是“能力声明错误”；是否真的启动不兼容的上下文管理，还取决于 Codex 的功能开关、认证和后端检查，不能直接说一定会崩。**

### 建议动作

近期先在外部模型目录生成时明确设置：

```python
"supports_experimental_context": False,
```

但长期不建议继续采用“复制全部原生字段，再不断补删除名单”的方式。更稳妥的是：

**外部模型只继承经过审核的编码提示与工具模板字段，其余能力由该外部路由自身决定；原生路由则继续保留上游元数据。**

这两个方向并不矛盾：**原生透传要避免损失信息，外部能力声明要避免凭空增加信息。**

还有一个需要一起处理的后续变化：新合入 main 的 **#44893** 增加了 `available_access_programs`，它是**与当前调用者相关的访问项目元数据**，不是模型的通用属性。现在 `_account_entry()` 也会复制当前原生模型对象，因此将来不能把当前登录账号的这个字段直接复制给所有导入订阅账号；未查询对应账号时应保留“未知”，而不是伪装成已确认可用或已确认不可用。上游也明确区分了缺失、`null` 和空列表。

**这里最值得补的测试**是：以支持实验上下文的原生模型作为模板，生成普通外部模型，确认其不会继承该能力；再改变原生模型顺序，确认外部模型的能力声明不变。新建子任务、fork 和切换模型则应分别验证，不能认为改一个目录字段就自动修复了已激活线程的全部状态。

---

## 二、`session-id` 与 `thread-id` 必须拆开：这是最直接的新 runtime 兼容风险

**位置：`easy_multi_provider/codex_history/models.py` → `HistoryAnchor.from_headers()`。**

当前有一条明确检查：

```python
if header_thread_id and header_session_id and header_thread_id != header_session_id:
    raise HistoryMismatchError("conflicting_thread_identity", source="anchor")
```

随后又允许使用 `session-id` 作为读取线程历史的身份来源。也就是说，这段代码把两个字段当成了同一种身份。

但是，上游新合入 main 的 **#44862** 已明确改变这层语义：临时根任务 fork 可以继承父任务的缓存亲和性，因此发给 Responses 的 `session-id` 可以是父任务的缓存键，而真正的子任务 session/thread 身份仍保留在请求元数据和 `thread-id` 中。HTTP 和 WebSocket 握手都涉及这个变化。**这是已合并变化，但我没有把它视为 0.154 稳定版已经发布的行为。**

上游现在允许的请求可以具有这样的关系：

```text
session-id               = 父任务缓存亲和键
thread-id                = 子任务线程 ID
turn metadata.session_id = 子任务实际 session ID
```

**这是合法的身份分离，不是历史串线。**但它进入 EMP 的上述解析路径后会被拒绝。

这项风险主要落在**需要读取身份锚点的历史重建、跨模型继续以及相关 fork 场景**，不能笼统地说所有原生透传请求都会失败。

### 建议动作

把下面三个概念分开：

```text
thread_id          → 定位 Codex 持久历史
actual_session_id  → 表示实际执行 session
cache_affinity_key → 只影响上游缓存路由
```

修改时最重要的不是简单删掉冲突检查，而是**只比较语义相同的身份字段**。

`thread-id` 与明确的 `metadata.thread_id` 真正冲突时，仍然应拒绝；`session-id` 与它们不同，不应自动被视为冲突。对于旧 runtime 的 `session-id` 历史定位回退，可以保留在明确的旧版本兼容路径中，但不能对新语义继续无条件使用。

同时，**不要为了让校验通过，把转发出去的 `session-id` 强行改成 `thread-id`**，那会破坏上游刚加入的缓存复用。

你已经在 `request_history_anchor()` 中优先使用每个请求的 `client_metadata`，并处理了部分 WebSocket 握手元数据过期问题。因此这里可以沿用现有入口修正身份解析，不需要新增第二套历史读取器。

验收时应覆盖：**父任务和两个 fork 共享缓存键，但各自只能读取自己的历史；真正的线程身份冲突仍拒绝；缺少明确线程身份时不能误读父任务历史。**

---

## 三、portable 工具转换存在静默语义损失，优先级应高于开放 `tool_search`

**位置：`easy_multi_provider/dialects.py` → `_raw_tools()`、`_portable_tools()`。**

当前 `_raw_tools()` 遇到 namespace 会递归提取内部工具，但没有保留外层 namespace。接下来 `_portable_tools()` 按裸 `name` 去重：

```python
if name in seen:
    continue
```

因此下面两个不同的工具：

```text
repository_a.search
repository_b.search
```

可能都变成 `search`，随后第二个被直接丢掉。这里不是普通格式调整，而是**改变了可调用工具集合**；即使两个工具参数不同，也仍然按同一个裸名字处理。

我也检查了 `collaboration_transport.py`。它确实有 `emp_collaboration` 专用 namespace 的冲突检查和恢复逻辑，但只针对该特殊转换，**不能保护任意 MCP namespace 的同名函数**。

### 建议分两步做

**第一步，先停止静默合并。**当不同语义的工具映射到同一名字时，明确返回兼容性错误，而不是保留第一个。错误信息只记录必要的类型、冲突类别等有界元数据，不需要把完整工具定义写进日志。

**第二步，再做可逆的名称映射。**例如将 `(namespace, name)` 映射到符合目标 Provider 命名限制的唯一名称，回包时恢复原始身份。这个映射必须贯穿工具定义、历史中的调用、`tool_choice` 和返回的调用项；`call_id` 则继续承担调用与结果配对，不能拿工具名字替代它。

注意，这不是要求所有协议转换都做到“字节级原样透传”。对于 Chat Completions 等不能表达原始结构的目标，**明确转换或明确拒绝，比表面成功但丢工具更正确**。

### 为什么现在不应该直接开启 `supports_search_tool`

你目前在外部模型目录中固定设置 `supports_search_tool=False`。与此同时，portable 工具定义只支持 `function/custom`，完整 Responses 输出校验也没有包括 `tool_search_call` 这类类型。因此只把目录字段改成 `True`，下游的定义转换、结果校验和下一轮历史处理并没有随之具备能力。

上游 **#44984** 的确报告了关闭 tool search 时 deferred Desktop 工具不可达，但它的具体复现场景是 **Bedrock Runtime + Desktop 远程任务**。它支持“EMP 应验证同类能力缺口”的判断，**并不等于已经实测所有 EMP App 场景都失败**。

所以我会调整前面通知中的优先级：

> **先修工具身份与转换正确性，再验证 `tool_search` 完整链路，最后才向特定路由宣告支持。**

未验证路线继续关闭，同时明确提示“延迟加载的 App 工具未验证/可能不可用”。也不要把这个能力与独立联网搜索 `web.run` 混在一起——你的现有文档已经正确区分了两者。

---

## 四、WebSocket 补作用域防护，但不需要现在重构成多路复用网关

**位置：`transport_continuity.py` 和对应测试。**

目前 `TransportContinuityState` 检查路由、连接、`previous_response_id` 是否匹配，但没有 turn/window 作用域。该模块的三个测试覆盖了同路由延续、路由变化拒绝和无 previous ID 的完整请求，没有覆盖同一连接上 turn/window 变化。

这里也需要修正我前面的一处表述：**#42787 目前仍是开放的用户 bug report 和修复提案，不是已经发布的、适用于所有 Responses Provider 的稳定契约。**不能据此给所有上游强行规定“任何跨 turn 的 previous ID 都违法”。

更合适的改进是：对已验证的 Codex runtime 路径，将每次请求中的 turn/window 身份纳入 continuation 判断；确认作用域改变时，使旧 chain 失效，并通过现有错误机制让 Codex 重新提交完整请求。**不要由 EMP 猜测历史，也不要在已经产生工具效果或部分输出后无条件重放。**

这可以作为第二项身份修复的延伸，而不是另起一个庞大的状态管理系统。当前更需要的是：

```text
连接仍然可用 ≠ 旧 response chain 仍然可用
缓存键相同   ≠ 线程相同
```

已有的 route/连接隔离继续保留。`stream_id` 多路复用可以等明确的上游需求和稳定实现出现后再做，不应抢占这几处确定问题的优先级。

---

## 五、下一步最划算的工程投入：让真实 Codex 协议测试持续运行

你已经有不错的验证基础：0.153 的真实 CLI 本地 fixture 测试，以及 0.154 的真实二进制 `model/list` 验证。文档也诚实地区分了这些验证与“真实 App 菜单刷新、真实上游服务、实机完整体验”。这些测试应该继续利用，而不是重新造一套测试框架。

但当前 `package.yml` 的重点仍是打包、启动冒烟和自更新回滚；工作流没有显式设置上述真实 Codex 测试所需的环境变量，而且 push/PR 的路径过滤里**没有 `tests/**`**。因此仅修改协议测试文件，可能不会触发这个工作流。

**建议增加独立的 runtime compatibility 测试任务，而不是让所有兼容测试都依附昂贵的四平台打包。**

| 测试层          | 建议作为门槛的内容                                            |
| ------------ | ---------------------------------------------------- |
| 普通 PR        | 相关单元测试、协议 fixture；包含 `tests/**` 的变更触发                |
| 固定稳定版 Codex  | 临时 `CODEX_HOME` + 本地假上游；跑真实 CLI/app-server，不需要付费模型请求 |
| 新 runtime 候选 | 固定版本或提交进行预验证；未通过前继续显示“未验证”，不自动扩大支持范围                 |
| App 实机验证     | 单独记录实际 App/runtime 组合，不能由 CLI 测试通过代替                 |

最需要加入真实协议回归的场景，就是上面几项：原生与外部模型切换、fork 缓存身份分离、同名 namespace 工具、WS 新作用域、压缩后的工具轮次继续。

另外，最近合并的 **#45248** 已把 Responses、MCP 和工具 hook 的元数据改成使用**实际发起该步骤时的模型、reasoning effort 和工具集合**，而不是沿用 turn 开始时的配置。因此再补一个“同一 turn 中工具结束后切模型/effort”的场景很有价值：EMP 应使用当前请求的设置，不能把第一次请求的工具集合或设置冻结到整个 turn。([GitHub][2])

---

## 哪些已经做对了，不建议再次重构

**模型目录的 HTTP/WS ETag 通知、App Server 只读观察、区分 App/managed/IDE/PATH runtime、原生和外部元数据隔离的方向，已经有实现和验证记录。**尤其是 catalog 变化与 Base URL 等启动配置变化分开判断，这一点应继续保留。

同样，外部子任务仍应由 Codex 管理执行、权限和生命周期。现在暴露出来的是**EMP 的协议转换和声明边界不够精确**，不是证明 EMP 应接管 Codex 的 agent runtime。

### 我建议接下来的开发顺序

**第一批：修能力与身份。**补 `supports_experimental_context`，限制外部模板字段继承，拆开缓存键与历史身份，并为新账号相关元数据保留未知状态。

**第二批：修工具语义。**先阻止 namespace 同名工具静默合并，再实现必要的可逆映射；`tool_search` 保持受限，等完整调用链测试通过后再开放。

**第三批：补持续验证。**将上述场景接入固定 Codex 二进制的 CI，再补 WS turn/window 防护和新 runtime 候选测试。

**目前 EMP 最需要的不是更多兼容开关，而是让“宣告的能力、实际可表达的工具、读取的历史身份”三者严格一致。**这几处修好，比继续扩展界面功能或增加更多上游名称，更能提升实际使用的稳定性。

本次没有修改仓库，也没有重跑整套测试或使用你的真实账号进行 App 端到端验证；上述“当前代码可确认的问题”和“新 runtime 的兼容风险”是分开判断的。

[1]: https://github.com/openai/codex/releases/tag/rust-v0.154.0 "https://github.com/openai/codex/releases/tag/rust-v0.154.0"
[2]: https://github.com/openai/codex/pull/45248 "https://github.com/openai/codex/pull/45248"

## EMP 999.14.2-exp · aero

实验版：在服务工具栏中打开带图标的「上令下效」，选择统筹模型和执行模型，各自设置推理强度。来源前缀和聚合路由均可选择；保存后，在 Codex 中选择 `aero/配对名称`。

下载完整的系统压缩包或 macOS DMG。安装包内置独立 aero 运行程序，无需自行安装 Python。下载文件持续保留，本实验版不检查或安装自动更新，不改变正式版的最新发布。

Orchestrator 负责规划、只读检查和复核；Executor 使用 Codex 的工具执行。EMP 负责连接、认证和计量，aero 独立管理角色交接。外层转发不重复记账。

本次补齐：

- 加密压缩与长会话继续，保留当前计划和执行阶段。
- Code Mode 与延迟工具发现，统筹读取、执行修改和复核完整往返。
- 原生生图与 Free 账号 MCP 生图，支持生成、编辑和图片落盘。
- 图片／音频输入输出，以及 Voice multipart、SDP 和实时 WebSocket 转发。
- 实时文字流，首段内容在上游生成完成前送达。

验收采用 Codex 0.169.0、隔离配置与合成上游；完整安装包覆盖 Windows、Linux、Intel Mac 和 Apple Silicon Mac。

---

Open **Lead & Execute** in the service toolbar, select source-qualified orchestrator and executor models (including aggregate routes), and set their efforts independently. Select `aero/pairing name` in Codex after saving.

Download the complete native archive or macOS DMG. The independent aero runtime is bundled; system Python is not required. Downloads remain available as prerelease assets. This experiment disables automatic updates and does not replace the latest stable release.

EMP owns routing, authentication and accounting; aero owns role handoffs; Codex owns tool execution and approvals. The relay wrapper does not duplicate usage charges.

This revision adds encrypted compaction with plan/role continuity, Code Mode and deferred tool discovery, native and Free-account MCP image generation/editing, image/audio roundtrips, Voice multipart/SDP/WebSocket forwarding, and live first-token delivery.

Acceptance uses Codex 0.169.0, isolated configuration and synthetic upstreams. Complete packages cover Windows, Linux, Intel Mac and Apple Silicon Mac.

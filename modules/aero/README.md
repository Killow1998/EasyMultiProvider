# aero

aero 将一组 EMP 模型配成统筹与执行角色，在同一 Codex 会话内完成规划、执行和复核。统筹模型先调查任务，再通过 `exit_plan_mode` 交给执行模型；执行模型通过 `enter_plan_mode` 返回复核。Codex 继续负责工具执行、权限审批和公开会话历史。

此目录提供独立的 Aero 模组程序、设置面板与验收入口。Core 负责安装、启停、凭据委派和模型目录，aero 负责角色编排、内部历史与压缩恢复。当前模组版本为 `0.10.0-beta`，使用 Worker API 2、协议 1。

## 使用

在 EMP 的模组页面安装并启用 aero，然后通过服务工具栏的「上令下效」添加模型配对。统筹与执行都从 EMP 当前目录选择，可以使用普通来源或聚合路由。保存后，Codex 的模型列表会出现 `aero/配对名称`。

配对中的推理强度分别保存，留空时跟随所选模型。已有会话继续使用同一个会话 ID；停用模组后不再接收新请求，已接收的任务会完成并保存。

## Worker 接口

源代码入口为 `python3 -B modules/aero/worker.py`，安装包入口为无参数可执行文件。启动后立即向标准输出写入一行握手：

```json
{"module":"aero","version":"0.10.0-beta","module_version":"0.10.0-beta","protocol":1,"host_api":2}
```

控制输入和输出均为 UTF-8 JSON Lines。每条命令返回 `{"value":结果,"error":null}`，失败时返回 `{"value":null,"error":"错误代码"}`。

| 命令 | 输入 | 结果 |
|---|---|---|
| `read_panel` | 无 | 包含样式、图标和文案的面板 JavaScript |
| `aero_start` | `state_dir`、`core_base_url`、`inbound_key`、`upstream_key`、`profiles` | `{"endpoint":"http://127.0.0.1:端口/v1"}` |
| `aero_configure` | `profiles` | `{}`；整体替换后续请求使用的配对快照 |

`core_base_url` 是 Core 的本地 HTTP origin，不含 `/v1`。`profiles` 对应原运行配置的 `models` 映射，允许为空：

```json
{
  "aero/配对名称": {
    "planner": {"model":"Native 中的模型名","effort":"low"},
    "executor": {"model":"服务名/模型名","effort":"high"}
  }
}
```

实际模型名来自 Core 目录；Core 同时传入配对的历史名称。`aero_start` 只接受一次；标准输入 EOF 关闭监听并等待已经接收的请求完成，然后关闭数据库退出。客户端断开仍会取消对应请求。

Core 请求 Aero 时使用 `Authorization: Bearer inbound_key`，每轮通过 `X-Aero-Caller-Key` 传入临时委派凭据。Aero 回调 Core 优先使用该凭据，未提供时使用启动时的 `upstream_key`。凭据仅存在内存中。

Core 通过 `X-Aero-Caller-Id` 提供稳定的调用方标识。`previous_response_id` 仅接受同一调用方、会话和模型下最新提交的已完成响应。Aero 将新增的 `input` 接到已保存的公开历史，再运行该轮。若查不到对应响应，Aero 在开始流式输出前返回 HTTP 400，错误代码为 `previous_response_not_found`。客户端可改用完整 `input`。

## 面板接口

`read_panel` 设置 `window.createAeroSettings`，不需要额外的 Aero 静态资源路由。调用参数为：

```javascript
createAeroSettings({
  getState, cloneState, persistState, openModal, $, esc, tr, help,
  modelName // 可选，默认保留原值
})
```

返回 `open(back?)` 和 `editProfile(id)`。配对保存在 Core 配置的 `aero_profiles`，模型选择读取 `catalog_models`。面板采用 Core 通用弹窗的保存和返回流程，不依赖其他模组加载。

## 历史兼容

Core 将原来的 `配置目录/aero` 作为 `state_dir`，继续使用 `sessions.sqlite3` 和原有 `sessions(id,state)` 表。本机 Codex 沿用 `[session_id, 请求模型名]` 生成会话键。网关调用把调用方标识也加入键，隔离不同设备的状态。会话保存角色、计划、内部工具记录、请求重放和压缩检查点。

已知的 Native 加密检查点从原会话状态继续；缺少可读历史时返回 `409` 和 `X-Aero-History: required`，由 Core 重建后重发 `_aero_visible_input`。公开压缩结果与内部角色状态分别保留。

`fixtures/legacy-compacted-session.json` 使用原始 `89c66f7755583c9cf390408e427446476205da16` 的 Engine 生成，作为旧会话直接续聊与重启重放的固定验收输入。

## 构建

运行时只使用 Python 标准库。构建环境安装 `pyinstaller==6.22.3` 后，在目标系统执行：

```sh
python modules/aero/build.py --output modules/aero/artifacts
```

输出一个 `EMP-mod-aero-系统-架构` 文件及 `.sha256`；Windows 文件带 `.exe`。单文件包含运行时、面板和许可证，安装后不需要系统 Python。发布通道为 `emp-mod-aero-api2-v0.10.0-beta`。构建会检查握手和冷启动退出时间。

仓库打包入口也支持 `cargo xtask package-module aero --output 输出目录`，可通过 `EMP_AERO_PYTHON` 指定安装了 PyInstaller 的 Python。

## 验收

从仓库根目录运行：

```sh
python3 -B modules/aero/verify_worker.py
python3 -B modules/aero/verify_worker.py --binary modules/aero/artifacts/EMP-mod-aero-linux-x86_64
python3 -B modules/aero/verify.py --native-codex
python3 -B modules/aero/verify_features.py
python3 -B modules/aero/verify_lifecycle.py
python3 -B modules/aero/verify_core.py --core /绝对路径/EMP --module modules/aero/artifacts/EMP-mod-aero-linux-x86_64
```

`verify_worker.py` 验证 stdio 协议、旧数据库续聊、重启重放、在线配置、凭据选择和 EOF 排空。增量用例覆盖工具结果、压缩续接及过期、失败或其他调用方的响应 ID。单文件测试清空子进程 PATH，并强制默认 ASCII 编码，覆盖无系统 Python 和中文配置。

`verify.py` 驱动实际 Codex CLI，覆盖直接工具和 Code Mode；`verify_features.py` 覆盖实时文本、Native 压缩、图片与音频；`verify_lifecycle.py` 覆盖断开取消、提交边界和线程退出。

`verify_core.py` 使用隔离的 Core、官方发布接口夹具和真实 Firefox，覆盖模组安装、目录、配对创建与编辑、角色工具往返、原版加密检查点旧会话续聊、压缩重放及停用后完成当前请求。结果和截图保存在 `evidence/core-linux`，包含测试时间与两份程序的 SHA256；Core 配置和模型请求均使用临时合成数据。运行需要 `geckodriver` 和 Firefox，可用 `--no-browser` 只复查 HTTP 链路。

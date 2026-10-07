# 测试与定时任务

[English](testing.md)

在 flow 之上有两样东西：

- **测试套件**（`iphone-use-mcp test`）：把「打开、点一点、看对不对」写成一个文件，随时重跑，退出码可以直接接进 CI。
- **定时任务**（`iphone-use-mcp schedule`）：daemon 按时运行 flow 或测试套件。手机被别人用时会等，跑失败了会通知你。

两者都走 `flow run` 的同一套引擎，没有新的路径去碰手机。

`iphone-use test …` 和 `iphone-use schedule …` 是同样的命令，会自动连到已安装的 daemon；第二台手机加 `--instance 名字`。

## 测试套件

```bash
iphone-use-mcp test examples/tests/settings-smoke.zh-CN.yaml
```

```text
suite: 设置冒烟测试 (4 cases)
  ✓ 首页列出通用                              0.14s
  ✓ 打开通用                                  4.35s
  ✓ 打开关于本机                              0.89s
  ✓ 返回首页                                  2.14s
4 passed, 0 failed (11.31s)
```

| 参数 | |
|---|---|
| `--json` | 输出完整报告：每个用例、每一步及其耗时 |
| `--junit FILE` | 同时写一份 JUnit XML |
| `--artifacts-dir DIR` | 失败用例的证据放在哪（默认 `./iphone-use-test-artifacts`） |
| `--confirm` | 套件会发送、发布、付款或删除时必须加 |
| `--owner NAME` | 本次运行的手机占用者名字（`X-Phone-Owner`） |
| `--validate` | 只解析和校验文件，不连 daemon |

退出码：`0` 全部通过，`1` 有用例失败，`2` 套件无效或手机用不了（锁屏、被别的会话占用、连不上）。手机处于空闲释放状态时，会先重连一次。

### 格式

YAML（`.yaml`、`.yml`）或 JSON（`.json`）：

```yaml
suite: 设置冒烟测试             # 可选，默认取文件名
app: com.apple.Preferences     # 可选，setup 之前先打开它
risk: navigation               # 可选，含义同 flow；side_effect 需要 --confirm
setup:                         # 可选，所有用例之前跑一次
  - {kind: back, after_ms: 300}
cases:
  - name: 打开通用
    steps:                     # 原样的 flow 步骤
      - {kind: tap_locator, locator: {label: 通用, kind: Button}}
    assert:                    # 每一条都要成立
      - present: 关于本机       # 一个标签、一个定位器，或它们组成的列表
        timeout_ms: 8000       # 默认 5000
      - application: 设置
        absent: [{label: 蓝牙}]
```

- **步骤**就是 [flow 步骤](agent-reference.md#flows)：`tap_locator`、`tap_label`、`type`、`key`、`scroll`、`swipe`、`launch_app`、`back`、`alert`、`wait_for`、`pause` 等，用同一个校验器检查。
- **`{kind: flow, id: …, inputs: {…}}`** 把一个保存好的 flow（registry id 或文件）当作用例的一部分运行。引用了 `side_effect` 的 flow，整个套件就需要 `--confirm`。
- **断言**就是一个 `wait_for` 期望：`application`、`present` 和 `absent`，定位字段相同（`label`、`identifier`、`kind`、`value`、`enabled`、`visible`、`focused`）。轮询到成立为止，超过 `timeout_ms` 算失败。

先按顺序执行步骤，再检查断言。用例在第一处失败时停下，下一个用例照常运行。`setup` 失败则整个套件停止，因为后面的用例已经没有意义。

### 用例失败时

会留下 `<artifacts-dir>/<用例>/`（权限 0700），里面有：

- `screenshot.png`：失败那一刻的屏幕。
- `elements.json`：那一刻的控件树。
- `steps.json`：跑过的每一步、耗时，以及 daemon 的回答。
- `error.json`：哪一步或哪条断言失败，为什么。

通常足以分清是 App 坏了还是测试写错了。举个例子：iPhone 13 上「通用」那一行被 iOS 26 底部的悬浮搜索栏挡住，截图一眼就能看出点击落在了搜索栏上。

### 写出稳定的套件

- 用带 `kind` 的 `tap_locator`，少用坐标。
- `scroll` 之后给列表留出停稳的时间再点（`after_ms: 2500`）。列表还在滑动时点一下，只会让它停下。
- App 会停在上次离开的页面。在 `setup` 里先回到一个确定的画面。
- 每种界面语言一个套件，标签按屏幕上写的来（`settings-smoke.en.yaml` 和 `settings-smoke.zh-CN.yaml`）。

## 定时任务

daemon 维护一份定时任务列表，每项是一行 cron 加上要运行的 flow 或测试套件。

```bash
# 工作日 9:00（本地时间）
iphone-use-mcp schedule add --cron "0 9 * * 1-5" --test ~/suites/smoke.yaml --name "早间冒烟"
iphone-use-mcp schedule add --cron "@daily" --flow health/export-all-zh-cn
iphone-use-mcp schedule list
iphone-use-mcp schedule runs [ID]
iphone-use-mcp schedule run ID              # 立即排队运行一次
iphone-use-mcp schedule disable|enable ID
iphone-use-mcp schedule rm ID
```

网页 `http://<mac>:44321/schedules` 上是同一份列表，有「立即运行」「暂停」「删除」按钮，也能在那里新建任务。

**cron** 有五个字段：分、时、日、月、周，按本地时间。支持 `*`、`a-b`、列表、`*/n`，以及 `@hourly`、`@daily`、`@weekly`、`@monthly`。周日写 0 或 7 都可以。

### 到点时会发生什么

开始前，调度器会像一个谨慎的 agent 一样先读 `/agent/status`：

| 手机此时 | 这次运行 |
|---|---|
| 空闲、可以操作 | 以占用者 `schedule-<id>` 的身份开始，结束后交还手机 |
| 被别的会话占用，或有人在看实时画面 | **推迟**，每 3 分钟再试一次 |
| 锁屏 | **等待解锁**，解锁后开始 |
| 空闲释放了 | 先重连；iOS 可能会要求输一次锁屏密码 |
| 到时间窗口结束仍不可用（默认 60 分钟，`--window-mins`） | 记为**错过** |

上一次还没跑完，就不会再叠一次；这一次记为**跳过**。到点时 daemon 没在运行，只要在时间窗口内恢复，任务照样会跑；错过多少分钟都只补跑一次。

**有副作用的任务**：会发送、发布、付款或删除的 flow 或套件，必须带 `--confirm`（`confirm_side_effects: true`）才能定时。这个确认在创建任务时针对这个目标和这组输入给一次。

**运行记录**：每个任务保留最近 20 次运行，记下：

- 什么时候跑的、用了多久；
- 结果：`passed`、`failed`、`missed` 或 `skipped`；
- 一行摘要和错误信息；
- 证据目录 `<state dir>/schedule-runs/<run>/`，里面有 stdout、stderr 和失败用例的证据。

**通知**：失败、错过或跳过时弹出系统通知。在 daemon 的环境里设 `IPHONE_USE_SCHEDULE_NO_NOTIFY=1` 可以关掉。`--webhook URL` 还会往该地址 POST 一个 JSON 事件，可以接 Slack、Discord 或任何接收 webhook 的服务。webhook 地址里常带密钥，所以列表里只显示主机名。

### HTTP 接口

都用平常的 bearer token 或已登录的浏览器；修改类请求还需要 `X-Phone-Control: 1`。

| 调用 | |
|---|---|
| `GET /agent/schedules` | 任务列表，每项带 `next_run_at` 和 `last_run` |
| `POST /agent/schedules` | `{cron, kind: "flow"\|"test", target, inputs?, confirm_side_effects?, name?, webhook?, window_mins?, enabled?}` → `201`；出错返回 `400`，如 `invalid_cron`、`invalid_target`、`confirm_required` 等 |
| `PATCH /agent/schedules/:id` | `{enabled: bool}` |
| `DELETE /agent/schedules/:id` | 删除任务和它的运行记录 |
| `POST /agent/schedules/:id/run` | 立即排队运行 → `202`；已有一次在排队或运行时返回 `409 run_open` |
| `GET /agent/schedules/:id/runs`、`GET /agent/schedules/runs` | 运行记录，最新的在前 |

`flow` 的 `target` 填 registry id 或 flow 文件的绝对路径，`test` 填套件的绝对路径。保存前 daemon 会先用 `iphone-use-mcp` 校验一遍。

### 夜间 flow 检查

`scripts/flow-reverify.py`（launchd 任务 `com.leeguoo.iphone-use.flow-reverify`，每天 03:30）做的不只是运行 flow。它还会：

- 更新 `verified_on`；
- 用 `flow report` 提 issue；
- 开一个 PR 供人审阅。

它还会特意跳过已停放的手机，免得夜里要人输密码。定时任务覆盖的是「运行」这一部分。如果想每天早上确认一组 flow 在你的手机上还能跑通，把它们写成 `{kind: flow, id: …}` 步骤放进一个套件，再定时运行这个套件。官方 flow 源的维护目前仍由夜间检查任务负责。

# iphone-use 完整指南

README 是精简版，这里是全部细节。

## 工作原理

```text
浏览器 <── GET /agent/mjpeg ── iphone-use daemon ── 127.0.0.1:9100 ──┐
浏览器 ── POST /control ─────> iphone-use daemon ── 127.0.0.1:8100 ──┤ iPhone 上的设备 runner
Agent  ── /agent/* ──────────> iphone-use daemon ── 127.0.0.1:8100 ──┘
```

- `scripts/setup-wda.sh` 编译并签名 iphone-use 的**设备 runner**（`runner/IPhoneUseRunner`，一个 XCTest UI 测试包，取代了 WebDriverAgent，HTTP 接口与它兼容），在手机上启动它，用内置的 `iphone-use relay`（走 macOS 自带的 usbmuxd）建两条固定的 loopback 中继：`8100` 控制，`9100` MJPEG 画面。daemon 只和 localhost 说话，后台进程手里不会攥着一个会变的手机 IP。USB 是唯一支持的路径，Wi-Fi 或 `socat` 属于手动实验。
- 浏览器从 `/agent/mjpeg` 拿实时画面（失败时退回 PNG 静帧），通过 `POST /control` 发输入，每条命令都返回成功或失败，不会把一个可能已经断掉的通道当成功。
- agent 用 `/agent/elements` 读文本形式的辅助功能树，用 `/agent/screenshot` 拿 PNG，用 `/agent/input` 做单步动作，或用 `/agent/actions` 跑一批带检查点的步骤。
- daemon 负责 runner 的生命周期：空闲后释放手机、带退避地重建 runner、把每个状态写进 `/agent/status`。
- 下文、状态字段（`wda`、`wda_actionable` 等）和环境变量（`WDA_*`、`PHONE_REMOTE_WDA_*`）里的 “WDA” 沿用旧名，指的就是这个设备 runner。

设计、生命周期、失败状态和安全边界见 **[`docs/direct-device-architecture.html`](direct-device-architecture.html)**。

## 快速开始

### 前置条件

- macOS 15 或更高，装**完整 Xcode.app**（只有 Command Line Tools 不够）。在 Xcode → Settings → Accounts 登录并选一个开发团队；免费 Personal Team 能用，但 runner 的描述文件要定期续。也可以用 App Store Connect API key（`WDA_ASC_KEY_PATH`、`WDA_ASC_KEY_ID`、`WDA_ASC_ISSUER_ID`）签名，不需要 Xcode 账号。
- iPhone 开启**开发者模式**，通过 USB 与 Mac 配对并点过信任。
- 编译、启动、使用 runner 期间手机保持**解锁、亮屏**。它过不了 Face ID 和密码。
- 不需要装任何 Homebrew 包：USB 中继内置在 App 里（`iphone-use relay`）；只有 App 版本太旧时才会退回 Homebrew 的 `iproxy`。
- 只有从源码构建才需要 Rust 工具链。

### 安装并接上第一台手机

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器下载最新 GitHub Release，注册当前用户的 LaunchAgent，写入 loopback 的 runner 地址，安装同版本的 agent skill，把设备 runner 的源码放到 `~/.iphone-use/runner`，并把设置脚本放到 `~/.iphone-use/setup-wda.sh`。它不会证明你的团队、手机、runner 和中继能一起工作，那是下一步的事。手机连上、信任、解锁、亮屏后：

```bash
~/.iphone-use/setup-wda.sh doctor    # 解释当前的 USB / 信任 / DDI / WARP 阻塞项
~/.iphone-use/setup-wda.sh           # 编译、签名、安装、启动 runner，拉起中继
~/.iphone-use/setup-wda.sh status
```

然后打开 **`http://<Mac局域网IP>:44321/setup`**。内置向导把 `/agent/status` 翻译成当前的阻塞项（USB、信任、开发者服务、runner、外部主机），它不会改你的 VPN，也不会替你跑 setup。手机可驱动后进 **`/phone`**，输入 `install.sh` 打印的密码。

配对了多台 iPhone？两处固定同一个 classic UDID：

```bash
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

### 把手机还给自己

runner 运行期间会占着手机。想自己用手机，先暂停它，下次 agent 用之前再恢复：

```bash
~/.iphone-use/setup-wda.sh pause     # 禁用 launchd job，只停 PID 校验通过的进程
~/.iphone-use/setup-wda.sh resume
```

更省事的是网页控制栏里的 **交还** 按钮（或 `POST /agent/mode {"mode":"human"}`）：daemon 停掉 runner，手机归拿着它的人用，状态里 `human_handoff:true`。这期间 agent 的输入请求一律 409 `phone_handed_to_human`，不会在你用着的时候把手机抢回去。交还之后同一个按钮变成 **交给 agent**，点它（或 `{"mode":"agent"}`）手机重新交给远程控制。

daemon 也会自己交还：5 分钟没有 agent 活动（释放后很快又被用到，下次的空闲时长会自动翻倍，最长 1 小时）、也没有人在看画面，它就停掉 runner 并停放 supervisor，注销、重启之后也保持停放。runner 常驻的话，iOS 每杀一次它就被拉起一次，每次拉起 iOS 都要输锁屏密码才能开 UI 自动化，结果没人用的时候手机也整天弹密码。下一次 agent 请求或 `POST /agent/mode {"mode":"agent"}` 会用缓存的 runner 产物把它拉回来（不重新编译），手机锁了就解一下。网页 `/phone` 上停放中的手机会显示 **连接手机**，手机解锁、亮屏后点它即可。`PHONE_REMOTE_IDLE_RELEASE_SECS` 改空闲时长，`0` 表示常驻（v0.6.3–v0.7.3 的行为）。

### 升级

> v0.9 移除了 iPhone 镜像后端；原来设了 `PHONE_REMOTE_BACKEND=mirror` 的安装，升级后一律走 WDA。

```bash
iphone-use upgrade            # 装最新 release（daemon + skill），再刷新其他地方的 skill
iphone-use upgrade --check    # 什么都不改：输出 "iphone-use 0.6.6 -> 0.6.7" 或 "... is up to date"
iphone-use upgrade --json     # 同上，JSON：name、current、latest、update_available、skills
```

安装器会把 `~/.local/bin/iphone-use` 链到 app 里的可执行文件，`~/.local/bin` 在 PATH 上就能直接用。`upgrade` 跑的就是安装那行 `install.sh`（顺带刷新安装器管理的 skill），之后对插件装的 skill 跑 `claude plugin update iphone-use@leeguooooo-plugins`，对 git checkout 跑 `git pull --ff-only`，其他复制来的 skill 会被标为非 release 匹配，提示换成安装器装的那份。退出码 `0` = 升级了 / 已是最新 / 检查成功，`2` = 检查或下载失败。`cargo build` 出来的二进制不会被替换，`upgrade` 只打印安装命令。

daemon 每天检查一次 GitHub，在 `/agent/status` 里报 `version` / `latest` / `update_available`，网页会挂横幅。一次性命令（`iphone-use stop`）每天最多在 stderr 打一行 `iphone-use X is available (you have Y). Upgrade: iphone-use upgrade`（缓存在 `${XDG_CACHE_HOME:-~/.cache}/iphone-use/update-check.json`，超时 2 秒）。设了 `CI`、`IPHONE_USE_NO_UPDATE_CHECK`、`USE_NO_UPDATE_CHECK` 或原来的 `PHONE_REMOTE_NO_UPDATE_CHECK`，每日检查和提示都关掉。安装器具体校验什么见[运维 → 升级](#升级-1)。

## 在浏览器里操作手机

`/phone` 显示手机实时画面：浏览器支持 WebCodecs 时走 `/agent/h264`（daemon 把 WDA 的 JPEG 帧用 Mac 的硬件编码器转成 H.264，码率约 0.7–2.5 Mbit/s），否则退回 `/agent/mjpeg`（25–40 Mbit/s，只适合局域网）。页面把你的点击、拖动、长按、滚动和输入（含中文）变成带 `ttl_ms` 上限、逐条确认的 `POST /control` 命令，不抢 Mac 焦点。**控件**面板列出辅助功能树，可以按精确标签点，不用对像素。

**流程**面板把你的操作录成可重放的 flow 文件：

- 只记录服务端确认成功的动作。优先保存精确的辅助功能标签；坐标手势标为易失。
- 输入的文字变成命名的运行参数，原文丢弃，不写进下载的 JSON。
- 每个动作后录制器比对元素树，能证明出现了新的唯一 identifier 或前台 app 变化时，插入一个可审阅的 `wait_for` 检查点；否则保留一段可见的短等待。它不会把任意标签或值抄进检查点，那里面可能有隐私。
- 审阅、排序、删除、填参数，然后下载合法的 flow v1 JSON 或执行一次。有动作没记下来的录制标为不完整草稿，不能执行。执行前必须填满参数，并勾选"不含不可逆操作"。
- **打开脚本**按和 CLI 相同的限制严格校验后，重新载入保存过的 flow；字面文字会被拒绝，必须是命名参数。

在这里录下来的东西，就是[官方源](#flow-与官方-flow-源)分发的东西。

## 用原生 iOS App 操作手机

`apps/ios` 是原生 SwiftUI App（iOS 17+）。**扫码连接**：在 Mac 上打开 iphone-use 页面，点工具栏的「扫码」，用 App 扫一下就连上（用 iPhone 自带相机扫也行，还能直接在浏览器里控制）。二维码只能用一次、5 分钟内有效；App 会保存一个设备令牌自动续登录，改控制密码会让所有已配对的手机失效。也可以手动填地址和控制密码。App 用系统硬件解码播放 H.264 画面，点击、长按、滑动、按住拖动、输入文字和回主屏幕都发到 `POST /control`，跟网页走同一套接口。被控的那台 iPhone 要保持解锁：iOS 不允许自动化输入锁屏密码，所以手机锁着时 App 几秒内就会提示「请解锁」，解锁后马上连上。遇到 App 禁止截屏的界面（支付、银行类 App），画面不再是一片白，而是显示守护进程按控件树画的线框图。构建：`cd apps/ios && xcodegen generate`，再用 Xcode 打开。

## Agent API

完整参考：**[`docs/agent-api.html`](agent-api.html)**。随包的 skill（[`skills/iphone-use/SKILL.md`](../skills/iphone-use/SKILL.md)）教 agent 这套循环。

### 鉴权与请求头

| 请求头 | 何时 | 含义 |
|---|---|---|
| `Authorization: Bearer <token>` | 每个 `/agent/*` 调用 | 设了 `PHONE_REMOTE_AGENT_TOKEN` 就用它；否则回退到 daemon 密码。 |
| `X-Phone-Control: 1` | 每个改状态的 POST | 鉴权之上的 CSRF / 意图保护，不能代替鉴权。`/control`、`/agent/input`、`/agent/actions`、`/agent/mode`、`/agent/hold`、`/agent/owner` 和 `/agent/inbox` 的 POST 都要。网页和 MCP 客户端会自动加。 |
| `X-Phone-Owner: <会话名>` | 控制请求 | 为本会话认领手机（issue #72）。租约存活期间（每次请求刷新，`PHONE_REMOTE_OWNER_LEASE_SECS` 默认 300）其他会话和不带头的客户端收到 `409 phone_owned`，附 owner 名和剩余秒数。只读调用不受影响。`X-Phone-Owner-Takeover: 1` 强行接管并记日志。 |

### 端点

| 方法 | 路径 | 用途 |
|---|---|---|
| `GET` | `/agent/status` | 就绪与生命周期：`backend`、`device_state`、`drivable`、`wda_actionable`、`recovery_owner`、`setup_blocked_on` / `setup_phase` / `setup_message`、`hint`、viewer 计数、`instance`、`udid`、`owner` / `owner_lease_remaining_secs`、`hold_remaining_secs`、`idle_secs`（距上一次 agent 请求的秒数，轮询状态不算）、`capture_redacted`（正在看的画面因为 App 禁止截屏而是空白的）、`version` / `latest`。 |
| `GET` | `/agent/screenshot` | 当前屏幕 PNG，来自手机。遇到 App 禁止截屏的界面，返回的是把控件树画在空白处的线框图，响应头带 `X-Capture-Redacted: 1`；`?raw=1` 返回原始截图。 |
| `GET` | `/agent/elements` | 扁平化的辅助功能树，带一次性 `snapshot` 令牌、`ax_stats` 可用性块，以及系统弹窗在场时的 `alert` 块。`?since=<snapshot>` 只返回 `delta`。WDA 缺失或繁忙 `503`，source 失败 `502`，不会用空数组伪装 `200`。 |
| `GET` | `/agent/mjpeg` | 鉴权后的实时 MJPEG。 |
| `POST` | `/agent/input` | 一个动作：点按、拖动、长按、滚动、文字、按键、`home` / `spotlight`、`launch_app`、`set_value`、`perform`、`alert`。`?return=delta` 会在动作后于预算内反复采样元素树并返回其变化，以及一个 `settle` 块（`settled`、`reason`：`stable` / `budget_exhausted` / `observation_failed`、`waited_ms`、`captures`、`budget_ms`，必要时还有 `sparse` / `stale`）。观察是尽力而为的：读取慢或失败绝不会把已经生效的动作降级成未知结果。树已稳定、且没有任何行新增、变化或消失时，还会带上 `no_visible_change: true`（滚动或点按没碰到会响应的东西）。 |
| `POST` | `/agent/actions` | 最多 24 个 `action` / `wait_for` / `pause` 步骤，整批预校验，一把 WDA 锁，首个失败即停。返回 `completed`、`applied_actions`、`failed_step`、`outcome`（失败步骤的结果）、`failed_step_outcome`、`batch_outcome`（`nothing_applied` / `partially_applied` / `unknown`）、`retry_safe`（整批语义，也是唯一授权重跑的字段）。 |
| `POST` | `/agent/mode` | `{"mode":"agent"}` 在已配置的手机上拉起 WDA，不换 UDID。`{"mode":"human"}` 停掉 WDA，把手机交还给持有人；之后 agent 输入返回 `409 phone_handed_to_human`，直到 `agent` 把手机要回来。 |
| `POST` | `/agent/hold` | `{"secs":N}`（0 清除，上限 14400）在人工暂停期间阻止空闲释放。释放已经开始时返回 `503 device_release_in_progress`。 |
| `POST` | `/agent/owner` | `{"release":true}` 提前归还 owner 租约。 |
| `GET` | `/agent/apps` | 已安装 App 及其 `version` / `bundle_version` / `system`，外加 `device.ios`，由 daemon 所在 Mac 上的 `devicectl` 读取。缓存 10 分钟；`?bundle=<id>` 过滤，`?refresh=1` 绕过缓存。失败返回 `503 apps_unavailable`（绝不返回空列表）；连着多台手机又没配置 UDID 时返回 `409 target_required`。 |
| `GET` | `/agent/intents` | 语义意图注册表，见[语义意图](#语义意图手机端快捷指令)。 |
| `POST` | `/agent/intent` | 派发一个已注册的 verb，结果落到 `/agent/inbox`。 |
| `GET` / `POST` | `/agent/inbox`、`/agent/inbox/drain` | 查看 / 追加 / 原子清空快捷指令结果队列。 |
| `POST` | `/control` | 浏览器输入，cookie 鉴权，`ttl_ms` 必填且在 1–2500 ms 之间。 |

### agent 必须遵守的语义

- **只看 `drivable:true`**（以及 `wda_actionable:true`）。`device_state` 取值：`ready`、`locked`、`blocked`、`offline`、`releasing`、`released`、`reconnecting`；`mode` 是 `agent` 或 `offline`。
- **至多送达一次。** 派发前过期返回 `408 not_sent`，`retry_safe:true`。派发后传输失败 `502`，派发后超时 `504`，两者都是 `outcome_unknown`、`retry_safe:false`：先看屏幕，再决定要不要再来一次。
- **目标绑定快照。** 元素索引只在同一次 `/agent/elements` 返回的 `snapshot` 下有效，树变了就 `409 stale_element_snapshot`。精确标签点按在零匹配或多匹配时都不动作。脚本里存标签、identifier、locator，不存索引和快照令牌。
- **元素级动作。** `set_value` 直接写字段（先清再填），带 `element` 的 `scroll` 把手势限制在该元素内，`perform` 调用命名能力（`increment`、`decrement`、`adjust`、`toggle`、`menu`、`double_tap`、`two_finger_tap`、`scroll_to_visible`、`pinch`、`rotate`、`force_press`）。`force_press` 需要 3D Touch：iPhone XR / 11 之后的机型上 WDA 在触屏前就拒绝，daemon 返回 `422 force_press_unsupported`（`not_sent`，`retry_safe:true`），改用 `menu`（长按）。`{"type":"scroll","page":true,"dy":N}` 滚动页面本身：daemon 找到页面的滚动视图，从避开输入框、按钮、内层滚动区和工具栏的位置起手（找不到时返回 `422 no_page_scroller`）。盖在 app 上的系统层（通知横幅、灵动岛、锁屏）里的行带 `overlay` 字段（`notification`、`dynamic_island`、`cover_sheet`）。`PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` 让树上标出每行支持哪些动作。
- **会点歪的点击直接拒绝。** 要点的元素中心被别的控件盖住（固定标题栏、键盘、悬浮按钮）时，返回 `409 element_occluded`、`not_sent`：先用 `perform` 的 `scroll_to_visible` 把它滚出来，重新读一次再点；目标被 WDA 标成 `visible:false`（在树里但没画出来，比如 Chrome 压在网页后面的标签页网格）时返回 `409 element_not_visible`。确实要点那个位置就加 `"allow_occluded":true`。按文字点击可以加 `"kind":"Button"` 限定类型，屏幕外的行不会和屏幕上的同名行抢匹配。
- **`set_value` 写完会读回核对。** 字段没变（网页输入框常常不认直接写入）就返回 `409 value_not_applied`、`outcome:"no_effect"`，改成先点输入框再用 `text` 输入。输入框自己加的格式（分隔符、空格）算写入成功。
- **禁止截屏的界面**（PayPay、银行、钱包）从 `/agent/screenshot` 拿到的是线框图，响应头带 `X-Capture-Redacted: 1`。在这类界面上按元素操作，不要对着空白截图做视觉识别。
- **每个 `/agent/*` 响应都带耗时**：JSON 里有 `timing`（`total_ms`、`wda_ms`、`daemon_ms`，以及每个 WDA 调用的次数、毫秒和字节数），另有 `Server-Timing` 响应头；每个请求还会在 `~/.iphone-use/agent-timing.jsonl` 里记一行（只有路径、owner 和耗时，从不记请求内容或屏幕内容）。
- 不认识的动作 `type` 返回 `400 invalid_action` 和支持的类型列表；常见的猜测（`scroll_into_view`、`click`、`fill` 等）还会直接给出该发的请求。
- **系统弹窗是另一层。** 点它的按钮会被确认但不生效。用 `{"type":"alert","button":"…"}` 或 `{"action":"accept"|"dismiss"}`。App Switcher 和控制中心是系统手势，WDA 够不着。
- **`/agent/actions`** 只要有动作已落地，就不会把整批重放标成安全。`tap_locator` 与 `wait_for` 用同一套精确的 label / identifier / kind / value / 状态字段，要求当前唯一命中。

```bash
HOST=http://<Mac局域网IP>:44321; AUTH="Authorization: Bearer $TOKEN"
MUTATION="X-Phone-Control: 1"; OWNER="X-Phone-Owner: my-script"
curl -s -H "$AUTH" "$HOST/agent/status"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"text","text":"你好"}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/actions" \
  -d '{"steps":[{"kind":"action","action":{"type":"shortcut","name":"home"}},{"kind":"wait_for","expect":{"present":[{"label":"搜索"}]},"timeout_ms":3000}]}'
```

### 语义意图（手机端快捷指令）

界面够不着、或点起来太慢的事（电量、健康样本、带原生确认的发消息），走一组精选的 **verb**，由一个桥接快捷指令执行。daemon 通过 WDA 在手机上打开 `shortcuts://run-shortcut`，不经过 Spotlight 和剪贴板；快捷指令把结果 POST 回 `/agent/inbox`。

```bash
python3 deploy/make-bridge-shortcut.py --token "$PHONE_REMOTE_AGENT_TOKEN" \
  --verb ping --verb battery --verb focus_on --verb focus_off
open "iU Bridge.shortcut"     # 接受导入；iCloud 会同步到手机
```

装到手机上可以交给 AI：把生成的文件放进 iCloud 云盘，在手机「文件」里搜 `iU Bridge`，点开后选「添加快捷指令」，不用在 Mac 上点导入框。

**AI 操作时自动勿扰。** 桥接快捷指令和注册表里有 `focus_on` / `focus_off` 时，AI 每次开始操作前，守护进程会先开勿扰（你本来就开着专注模式时不动），交还手机、空闲释放或交给人工时再关掉。开、关时手机上都会弹提醒。第一次运行会问一次是否允许桥接快捷指令发通知，AI 会收到 `agent_focus: waiting_for_permission` 并处理。只发不收，不需要打开下面的回传通道。设 `PHONE_REMOTE_AUTO_FOCUS=0` 可以关掉。

verb 定义在 `~/.iphone-use/intents-registry.json`（从 [`deploy/intents-registry.example.json`](../deploy/intents-registry.example.json) 起步）。快捷指令名必须等于注册表的 `bridge.name`，bearer token 放在快捷指令自己的请求头里。`--self-test` 检查 plist 里那些会静默失败的部分。每个 verb 首次使用要在手机上点一次授权，调用期间快捷指令会到前台。

**回传路径要求手机能连到 daemon**（issue #59）。默认的 `PHONE_REMOTE_HOST=127.0.0.1` 能派发 verb 但收不到回答，所以意图通道默认是关着的，要用得先从下面选一个回传路径：

| 回传路径 | 做法 | 代价 |
|---|---|---|
| 绑局域网 | `PHONE_REMOTE_HOST=0.0.0.0`，同时设密码**和** `PHONE_REMOTE_AGENT_TOKEN` | 最简单；daemon 的鉴权面暴露给整个局域网。 |
| USB 反向隧道 | 把手机侧端口转回 Mac 的 loopback 监听 | 不暴露局域网；多几样要维护的东西。 |

只发不收的 verb 在纯 loopback 下能用。不可信网络上别绑 `0.0.0.0`：WDA 自己的 `8100` / `9100` 没有任何鉴权。

## MCP server

[`iphone-use-mcp`](../crates/mcp/README.md) 随安装的 app 一起交付（`~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`），每个 release 也单独发一份带校验的压缩包。它通过 stdio 说 MCP，并自动给 daemon 请求加 `X-Phone-Control` 和 `X-Phone-Owner`（`PHONE_REMOTE_OWNER`，默认 `mcp-<pid>`）。

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "<agent-token>"
      }
    }
  }
}
```

| 分组 | 工具 |
|---|---|
| 看 | `phone_status`、`phone_capabilities`（这个版本支持什么，以及此刻能不能用；不唤醒手机、不占租约）、`phone_screenshot`、`phone_elements`（带 `registry` 块，列出当前屏幕上这个 app 已安装的 flow） |
| 动 | `phone_tap`、`phone_tap_element`（绑定快照）、`phone_tap_label`（精确标签唯一）、`phone_scroll`、`phone_type`（中文无损）、`phone_key`、`phone_shortcut`（`home` / `spotlight`）——每个都可选传 `observe` |
| 批 | `phone_run_steps`：最多 24 步，含 `tap_locator`、`launch_app`、`picker`、`alert`、长按 / 滑动 / 拖动、`wait_for` |
| 生命周期 | `phone_reconnect`（在已配置的手机上重启 WDA，不换 UDID）、`phone_hold`、`phone_release_owner` |
| Flow | `phone_flow_list`、`phone_flow_info`、`phone_flow_run`、`phone_flow_update`、`phone_flow_publish`、`phone_flow_report` |

这七个动作工具加上 `phone_capabilities`，解析后的 JSON 放在 MCP 的 `structuredContent` 里，文本块只是在 8 KiB 处截断的预览——请解析结构化字段。`phone_run_steps` 两边都给完整的批次结果，解析哪一边都安全。其余工具保持它们原来的返回形态：多数把完整 JSON 放在文本里（包括 `phone_flow_run` 的执行结果，无论成败），`phone_screenshot` 返回图片，而在请求到达手机**之前**就失败的那些错误是说明文字。总的规则是：有 `structuredContent` 就读它，没有再按该工具的约定读 `content`。无法确认结果时会给 `outcome: "unknown"` 与 `retry_safe: false`，这是可供程序分支的形式；判断能否重发一律看显式的 `retry_safe` 布尔值，不要看 `outcome`。完整对照表见 [`crates/mcp/README.md`](../crates/mcp/README.md)。

单步动作工具传 `observe: true`，daemon 会在动作之后观察屏幕稳定下来并把变化一起返回（`settle`、`snapshot`、`delta`）。默认关闭，因为这段等待是动作本身不必付的延迟。`settle.reason` 三态：`stable`、`budget_exhausted`（观察预算用尽，动作本身已经发生）、`observation_failed`（读取链路坏了）；`stale: true` 表示返回的树是上一次成功的读取而不是当前屏幕，`sparse: true` 表示空树或纯容器树，这种树两次相同也不算 stable。

收起键盘、卸载、目标配置仍只走 HTTP。完整 schema 见 [`crates/mcp/README.md`](../crates/mcp/README.md)。

## Flow 与官方 flow 源

**flow** 是一份严格的 JSON（`version: 1`），步骤和 `phone_run_steps` 一样带检查点，外加命名的字符串参数。`iphone-use-mcp` 二进制能校验并运行它，全程不经过模型：

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/search-spotlight.json          # 离线
PHONE_REMOTE_TOKEN=… "$MCP" flow run examples/flows/search-spotlight.json --input 'query=咖啡'
```

**官方源** [`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows) 是唯一支持的源，把 flow 做成可安装的目录，按 app 分组、经过审阅，和 chrome-use 分发 site 包一个思路：

```bash
"$MCP" flow update                        # 同步到 ~/.iphone-use/flows：sha256 + 严格校验，0600
"$MCP" flow list --category health        # id · risk · verified · inputs · name
"$MCP" flow info health/export-all-zh-cn  # 元数据和步骤模板
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn --artifacts-dir ./runs   # 记录本次执行（文件 0600）
"$MCP" flow add my.json --as myapp/daily  # 自己的 flow，update 不会删
"$MCP" flow publish my.json --as myapp/daily --alias 某App --note "iPhone 17 Pro Max, iOS 26"   # 用 gh 开 PR
"$MCP" flow report health/export-all --result @run.json --note "资料按钮改名了"                  # 提 flow-broken issue
```

执行失败时结果里会多一个 **`diagnosis`** 块：daemon 原始的 0-based
`failed_step`、屏幕当时能不能读（`observable`）、原因（`locator_matches_now`、
`locator_no_match`、`locator_ambiguous`、`still_present`、`no_similar_element`、
`no_readable_tree`、`screen_unreadable`、`diagnosis_timeout`、
`step_has_no_locator`），以及最多五
个候选元素，附上它们是按哪些定位字段 `matched` / `differed` 挑出来的（身份
字段 `identifier` 优先于 `label`）。这是执行结束后一次有界（4 秒）的只读：
不重发任何动作，不静默改写 flow，也不改动本次执行的 `outcome` /
`applied_actions` / `retry_safe`。CLI 和 `phone_flow_run` 走的是同一条路径。

`--artifacts-dir DIR` 把本次执行写成可机器读取的记录——schema、flow 名称
与 sha256（算的是本次真正解析的那份文件内容）、执行时所对的各版本（读不到
就写 `unavailable`，绝不猜，也不会为了收集元数据去打扰设备）、真实耗时、
以及投影后的结果。目录在**发出任何动作之前**先验证可写；文件以 0600 独占
创建，不覆盖已有证据、不写穿 symlink，同一秒的两次运行各留各的记录。只落
结构化字段：输入的文字和屏幕文本一律不写盘。如果动作已经执行、写盘才失
败，结果照常完整打印，另加一个 `artifact_error`——记录失败不能改写已经发
生的事；反过来 `artifact_error` 也不会把成功的执行说成失败。

flow 上的源元数据都是可选的：`app`（bundle id）、`category`、`risk`（`read_only` · `navigation` · `side_effect`，`side_effect` 没有 `--confirm` / `confirm=true` 拒绝运行）、`locale`（标签和语言绑定）、`tags`、`verified_on`（证明过这份文件的真机记录）。文件是纯 JSON，安装源不执行任何代码；任何一个校验或哈希失败都会中止整次更新，本地目录原样不动。

格式里定死的规则：`--input KEY=VALUE` 只在本次运行解析，不写回文件；flow 在第一个失败步骤停下，不自动重试；命令行参数会留在 shell 历史里，所以参数不能装凭据、验证码或隐私内容，发送 / 发布 / 支付 / 删除类动作必须声明 `side_effect`。

app 更新不会让源悄悄失效：每条 flow 记着它在哪个 app（系统 app 则是 iOS）版本上验过，CLI 读出手机上实际装的版本（`flow apps`），每次列表都给出 `compat` 结论：`verified`、`untested-newer`、`incompatible`、`broken`、`needs-verification`、`draft`、`unknown`。`flow run` 对 broken / incompatible 的 flow 不带 `--force` 就拒跑。夜间 canary（`scripts/flow-reverify.py`）在真机上重跑已验证的只读 flow，每条给出三种结论之一：**verified**（刷新 `verified_on`）、**failed**（打 `needs-verification` 标记并提 `flow-broken` issue）、**skipped**——手机锁着、被别人占着、不可驱动，或者 daemon 自己也判不出结果。skipped 的 flow 原样不动：手机不可用的那一晚说明不了 flow 的任何事，既不该判它坏，也不该给它记一个没发生过的验证日期。

agent 不靠记性去查源，而是被推着走：`phone_elements` 直接列出屏幕上这个 app 已安装的 flow；`phone_run_steps` 成功跑完 3 步以上会提示把这段存成 flow；`phone_flow_run` 失败时保留现场，`phone_flow_report` 只需补一句说明。格式背后的调研见 [`docs/scripted-flows-research.html`](scripted-flows-research.html)。

## 运维

### 生命周期与恢复

`/agent/status` 是唯一事实来源。`recovery_owner` 在托管 loopback WDA 下是 `daemon`，首次接入尚未持久化目标时是 `unconfigured`，不托管的端点是 `external`。启动 WDA 之前，daemon 会先问手机是否锁屏：锁着的手机在连接后几秒内就显示 `setup_blocked_on:"locked"`（以前要等 Xcode 约 70 秒超时），连续两次读到解锁才启动。仍然因锁屏失败的，从 5 秒到 1 分钟安静重试；其他失败从 5 秒退避到 5 分钟；一次成功恢复清零两种退避。交互式 setup 最多等 5 分钟解锁。

在 iPhone 17 Pro Max（iOS 27）上实测：手机解锁时冷连接 13–22 秒可操作；锁着时约 4 秒提示解锁，解锁后约 14 秒可操作；按编号点击并等画面稳定（`?return=delta`）约 4 秒。`POST /agent/mode {"mode":"agent"}`（MCP 里是 `phone_reconnect`）只重启一次已配置的目标，不要循环调；先读 `hint` 和 `setup_blocked_on`（`warp|proxy|usb|trust|ddi|account|automation_mode_disabled|locked`）。`automation_mode_disabled` 表示手机已解锁但 iOS 未开启 UI 自动化：在手机上打开“设置 › 开发者 › 启用 UI 自动化”，并在解锁状态下确认密码或“允许自动化”提示。

**谁有权结束一次重连。** 一次启动归发起它的任务所有，只有这个所有者能结束它。每次开始都会生成一个代次，所以迟到的任务无法结束接替它的那一轮；`GET /agent/status` 也永远不会结束重连——读状态会刷新健康缓存，但不移动生命周期。一次等待只以一个原因结束：手机可驱动了、锁屏了、setup 报出了前置阻塞、预算用尽、或被另一轮接管，每种都有日志。整个等待受预算约束：探针由绝对截止时间掐断而不是它自己的上限，超过截止时间才返回的证据一律丢弃，被取消的等待（进程关闭、future 被 drop）会释放自己那一轮而不是把 `reconnecting` 永久留下。启动之前缓存的证据，永远不算作这次启动已完成的证明。

### 升级

```bash
iphone-use upgrade    # 或者：curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

安装器把 release tag 解析成一个确定的 commit，helper 和 skill 都钉在上面；daemon app 取对应 Release asset 并校验 SHA-256；先把 skill 安装到 `~/.agents/skills/iphone-use`、逐字节校验落盘内容和 Claude Code 发现链接，再替换 daemon。skill 失败中止升级；之后 daemon 失败则恢复旧 skill。`IPHONE_USE_SKIP_SKILL=1` 保留现有 skill，这是降级安装，安装器不保证新 daemon 和旧 skill 兼容。已有的配置（`PHONE_REMOTE_WDA_URL`、密码、token、UDID）原样保留。`PHONE_REMOTE_NO_UPDATE_CHECK=1`（或 `IPHONE_USE_NO_UPDATE_CHECK=1` / `USE_NO_UPDATE_CHECK=1`）关掉每日检查。

安装时保留有效的现有签名，签名无效就用不动 keychain 的 ad-hoc 签名补上；daemon 不持有任何 macOS 授权，换签名不会让什么失效。

### 配置

| 环境变量 | 默认 | 用途 |
|---|---|---|
| `PHONE_REMOTE_HOST` / `PHONE_REMOTE_PORT` | `127.0.0.1` / `44321` | 监听地址和端口（局域网用 `0.0.0.0`，此时必须设密码）。 |
| `PHONE_REMOTE_PASSWORD` | 无 | 浏览器登录密码；没设 agent token 时兼作 bearer。 |
| `PHONE_REMOTE_AGENT_TOKEN` | 无 | 专用 agent bearer。设了以后是**唯一**接受的 bearer。 |
| `PHONE_REMOTE_UDID` | 安装器识别并持久化 | 托管 WDA 和破坏性命令使用的 canonical iPhone。请求不能临时换机，要改就改部署并重启。setup 时传同值 `WDA_UDID`。 |
| `PHONE_REMOTE_WDA_URL` / `PHONE_REMOTE_WDA_MJPEG_URL` | `http://127.0.0.1:8100` / `:9100` | WDA 控制和 MJPEG 的 loopback。不可达时控制请求直接失败。 |
| `PHONE_REMOTE_WDA_MANAGED` | loopback 端点默认开 | daemon 是否负责 WDA supervisor / 中继的生命周期。 |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `300` | 空闲多少秒后停 runner 并停放 supervisor，下一次 agent 请求再拉起；`0` 表示常驻，代价是 iOS 每杀一次 runner 就弹一次密码。 |
| `PHONE_REMOTE_OWNER_LEASE_SECS` | `300` | `X-Phone-Owner` 租约在没有请求刷新时的存活时间。 |
| `IPU_RUNNER_SRC` | `~/.iphone-use/runner` | setup 编译的设备 runner 源码（仓库里的 `scripts/setup-wda.sh` 用仓库自己的 `runner/`）。只有不是默认值时才持久化。 |
| `WDA_RUNNER_REBUILD` | 关 | `1` 让下一次 setup 忽略记录的 runner 产物、重新编译。 |
| `PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH` | runner 默认 64 | 只对外部 WebDriverAgent 生效；设备 runner 自己限制读树（深度阶梯、5000 个节点）。 |
| `PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S` | — | 同上，只对 WebDriverAgent 生效。 |
| `PHONE_REMOTE_ELEMENTS_AFFORDANCES` | 关 | `1` 给 `/agent/elements` 的行加稀疏的 `actions`、`selected`、`min` / `max`。 |
| `PHONE_REMOTE_ELEMENTS_TRAITS` | 关 | `1` 再输出原始的辅助功能 trait 名。 |
| `PHONE_REMOTE_NO_UPDATE_CHECK` | 关 | 跳过每日 release 检查。 |

## 安全

daemon 把手机的实时控制放到了网络上，它的 URL 和密码要当凭据对待。

- 密码 / cookie / bearer 只保护 `44321`。**手机上 runner 自己的 `8100` 和 `9100` 没有鉴权**，USB 中继也不会加，手机所在 Wi-Fi 里的另一台机器能直接连过去。只在可信、隔离的网络里用；走 USB 时关掉手机 Wi-Fi 就没有这层暴露。
- 真正带鉴权的设备传输属于 Phase 2（companion app 或受控隧道）。在此之前，daemon 的登录保护不到 runner。
- 从局域网外访问时，经由带鉴权的 HTTPS 反向代理，或可信的 VPN / 隧道（比如 Tailscale）连到 `44321`，绝不要把 runner 的端口暴露出去。daemon 只提供明文 HTTP，识别 `X-Forwarded-Proto`，session cookie 是 `HttpOnly` + `SameSite=Lax`。
- owner 租约（`X-Phone-Owner`）是协作会话之间的协调机制，不是安全边界。
- 开放访问期间不要停在支付、私聊或 2FA 画面。不用时停掉 LaunchAgent。

### WARP / VPN

WARP 一类 VPN 会切断 WDA 依赖的 CoreDevice 隧道。`setup-wda.sh doctor` 能检测到，`/agent/status` 会报 `device_state:"blocked"`、`setup_blocked_on:"warp"`；两者都不会去改你的 VPN，那是操作者的决定，公司电脑需要管理员配 split tunnel。

## 开发

```bash
cargo build --release --bin iphone-use --bin iphone-use-mcp
./scripts/make-app.sh                  # → ./iPhoneUse.app
./install.sh ./iPhoneUse.app           # 签名、安装、写 LaunchAgent（使用工作树里的 skill）

# 或者不安装直接跑 daemon
PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100 \
PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100 \
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret ./target/release/iphone-use serve
```

发版：`scripts/release.sh 0.6.8` 升 crate 版本、跑发版门禁测试、推 tag、等 release 构建完成后同步插件市场（`--dry-run` 只预演）。

| 路径 | 内容 |
|---|---|
| `crates/server` | daemon：设备控制（WebDriverAgent 协议）、MJPEG 代理、浏览器 `/control`、agent API |
| `runner/` | 设备 runner：在手机上提供控制接口和 MJPEG 的 XCTest UI 测试包 |
| `crates/mcp` | `iphone-use-mcp`：MCP server、flow 运行器、源客户端、`flow publish` / `report` |
| `crates/core` | 共用的鉴权工具 |
| `web/index.html` | 浏览器客户端（MJPEG + `/control`） |
| `skills/iphone-use` | 安装器随包交付的 agent skill |
| `scripts/`、`deploy/`、`install.sh` | runner 设置、打包、LaunchAgent、桥接快捷指令生成器 |
| `docs/` | 架构、agent API 参考、设备设置常见坑、flow 调研 |

### 路线

- [x] Direct/WDA 元素树控制、Unicode 文字、标签点按、手机端截图（iPhone 17 / iOS 27 上有组件级记录，见 [`docs/wda-setup.html`](wda-setup.html)）。
- [x] MCP server；CI 出 release 二进制，一行命令安装。
- [x] 确定性 flow、官方 flow 源，以及 publish / report 两条回路。
- [ ] 记完下面的浏览器整链路真机验收矩阵。
- [ ] 让首次接机、签名续期、休眠 / 重连恢复、多设备选择在产品界面里看得懂。
- [ ] 在真机上逐条重验每个对外宣称的命令。
- [x] iOS 原生 companion App，支持扫码配对（`apps/ios`）。
- [x] 逐请求耗时统计，以及据此做的第一轮提速（按编号点击和等画面稳定都只读一次控件树）。
- [ ] Phase 2 局域网以外的带鉴权设备传输（受控隧道）。
- [ ] 一段 agent 操作手机的演示。

### 真机验收边界

下面各项都在真实 iPhone 上观察到，浏览器这条路径才算验收：

1. Mac 不给 app 任何 macOS 隐私授权，安装并跑 WDA setup，WDA 持续在线。
2. `/agent/status` 对目标 UDID 报 `backend:"direct"`、`wda:true`、`wda_actionable:true`、`drivable:true`。
3. 另一台设备打开 `/phone` 画面持续更新；停掉 9100 中继后界面报降级 / 离线，而不是装作正常。
4. `/control` 的点按、拖动、长按、滚动、ASCII 和中文各执行一次，都有确认且只落地一次。
5. `/agent/elements`、`/agent/screenshot`、`/agent/input` 走 bearer 鉴权正常，WDA 故障时也符合文档；任何命令都不动 Mac 光标。
6. 观察到 `releasing → released → reconnecting → ready`，并覆盖锁屏 / 解锁、USB 重连、Mac 重启、WDA 重签重装，多设备 Mac 上目标不串。
7. 在隔离网络里记录手机 IP 是否暴露无鉴权的 `8100/9100`。

## 反馈

碰到问题请[开 issue](https://github.com/leeguooooo/iphone-use/issues)。也欢迎 AI agent 来提：随包的 skill 让它在 API 误导自己时（经用户同意）提结构化 issue，flow 的问题则提到[官方源](https://github.com/leeguooooo/iphone-use-flows/issues)。

## 许可证

[MIT](../LICENSE)

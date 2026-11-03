# 从 agent-desktop 借什么

对象：[vercel-labs/agent-desktop](https://github.com/vercel-labs/agent-desktop)，2026-11-03 看的是 `9eb9b29`。Rust，约 36.7 万行，macOS / Windows / Linux 三个适配器共用一个 core。读了 README、CONCEPTS.md、agent skill 及其 references、docs/solutions 里的几篇、core 的 `refs` / `actionability` / `response`。

它做的事跟我们一样：读无障碍树，给元素发 ref，按 ref 动作，再读一次验证。差别在于它把“这次动作到底有没有生效、能不能重试”做成了一套贯穿全程的契约。这正是我们最常被 agent 反馈的地方。

借鉴原则照旧：借机制，不照搬它的命令名、字段名和文案（Apache-2.0 也一样）。不去它的仓库提 PR 或评论。

## 已经对齐的部分

这些我们有等价物，不用动：

| agent-desktop | iphone-use 现状 |
|---|---|
| Interaction Lease：同一时刻一个进程能注入输入 | `X-Phone-Owner` 300 s 租约 + runner 互斥锁 + `owner_epoch` 中途换主即中止（`control_scope_changed`） |
| Partial Observation：`complete` / `truncated` / `subtree_truncated` | `partial` / `partial_hint` / `coverage`，`?since=` 在读不全时不出 delta |
| `skills get desktop`：二进制自带与版本匹配的文档 | `/agent/reference` |
| 批量失败时区分“什么都没发生 / 部分发生 / 不知道” | `batch_outcome`：`nothing_applied` / `partially_applied` / `unknown`，`failed_step`、`applied_actions` |
| 读失败 ≠ 不存在 | `wait_for` 的 `absent` 不接受稀疏树作为证据（`http.rs:11845` 的注释写的就是这个） |
| jev-desktop | `phone_jev_run`（看起来是同一个 JEV 后端） |

## 差距与建议（按对手机上实际伤害排序）

### 1. 快照令牌是整屏哈希，屏上任何变化都让所有 index 失效

> 已实现（未提交，2026-11-03）：`rebind_snapshot_row`（`crates/server/src/http.rs`），runner 侧保留最近 8 棵完整树（`RunnerClient::served_tree`）。另加了两道闸：同屏判定（导航栏一致、无新 alert/sheet、至少 75% 可识别行保留），以及唯一命中时祖先行也必须一致。响应里还不会标明发生了重绑定，只打日志。待真机验证。

**它怎么做。** 每个 ref 存一份身份证据：role、name、description、native_id（开发者给的 identifier）、在树里的路径、几何、可用动作、所属窗口。动作时按顺序重新定位：先走存下的路径，再做有界搜索；候选先用 identifier 确认，再用“稳定文本”确认；只有没有任何身份的元素才允许靠唯一几何命中。零个匹配是 `STALE_REF`，多个是 `AMBIGUOUS_TARGET`，绝不猜。

其中一条规则值得单独记：**可变的值不是身份**。输入框里的内容、被提升成名字的数值，都不能拿来认元素。

**我们现在。** `snapshot` 是整棵行列表的 SHA-256（`crates/core/src/control.rs:8`）。`tap_snapshot_element`（`http.rs:7251`）在 `fetch_snapshot_row_reusing`（`http.rs:7107`）里只要哈希对不上就回 `stale_element_snapshot`。之后的重新定位其实已经做得不错：转成语义 locator 交给 runner 的 `find_element_matches`，多匹配时用 ±1pt 帧比对和同 locator 序号消歧（`pick_snapshot_element`）。

问题在闸门太粗：时间栏走一分钟、列表里一个角标变了、键盘候选词刷新，令牌都会作废。agent 只能重读（每次 1.8–3.6 s），再重新挑 index。

**建议。** 令牌对不上时，不直接判 stale，而是拿快照环里那份旧树中的这一行（`remember_element_snapshot` 已经存着）做逐行身份比对：kind + label + identifier + 祖先路径，去掉 value。新树里唯一命中就照常走现有的重新定位流程；零个命中回 stale，多个回 ambiguous 并附候选。要点：

- 身份字段排除 `value`，label 对输入框类控件也排除（输入框的 label 经常就是内容）。
- 仍然 fail-closed，只是粒度从“整屏”降到“这一行”。
- 响应里说明是“令牌已变但该行仍唯一”，方便统计这条路径命中多少。

成本：中。全在 daemon，runner 不用动。需要真机验证：拿时间栏变化、列表角标变化、键盘弹出三种场景测命中率和误命中。

### 2. 成功结果缺“送达但未验证”这一档，也不报告动作后冒出来的弹窗

**它怎么做。** 每个动作结果带 `disposition { delivery, retry }`。delivery 有五档：unknown / not_delivered / delivery_uncertain / delivered_unverified / delivered_verified。只有 not_delivered 能安全重试。有状态的动作（set-value、check、expand…）事后重读一次：读到矛盾回失败，读不到不算失败，但标成 `delivered_unverified` 并写明原因（`insufficient_evidence`、`secure_field`）。另外两个字段：

- `post_state`：目标动作后的状态。
- `surfaces`：动作结束时开着的菜单、sheet、alert。文档原话的意思是：带着 surfaces 的 unverified 结果，多半是有个确认框在等。

**我们现在。** 有 `outcome`（not_sent / unknown / partial / applied / success）和 `retry_safe`，送达这条轴是齐的。验证这条轴没有：tap 成功只说明发出去了，不说明界面变了。`observe:true` 会附一份 settle 后的观察，但没有逐动作的“验证到 / 没验证到”。

**建议。**

- 给 tap / type / set_value 的成功结果加验证档：观察里能看到目标状态变化（值变了、选中了、界面换了）就算 verified，否则 unverified + 原因。set_value 可以先做：写完读回元素值比对即可（现在是否已读回，没核实）。
- 动作结果里报系统弹窗。我们知道 alert 不在 AX 树里（见 PR #61 的原生 alert 路由），所以 agent 读元素时看不到它，只会觉得“点了没反应”。动作后顺手探一次 alert（已有探针，约 0.8 s），开着就放进结果里。这条对 agent 帮助最大。

成本：低到中。alert 探针会给每次动作加时间，可以只在 settle 后界面没变时才探。

### 3. 错误恢复是自由文本 `hint`，agent 只能读文字猜

**它怎么做。** 错误里有 `recovery { strategy, retryable, requires_fresh_snapshot, retry_after_ms }`，而且只在可以安全重试时出现。文档明确要求调用方按 `code` 和 `disposition` 分支，不按文案。

**我们现在。** 错误码齐全（`stale_element_snapshot`、`ambiguous_element_label`、`element_occluded`、`target_scope_unproven` 等，`http.rs:9740` 一带），恢复建议是 `hint` 文本；结构化的 `next_step` / `recovery` 只在 `/agent/status` 里有。

**建议。** 把 `/agent/status` 已有的结构化恢复字段推广到动作错误：每个错误码对应一个固定的策略枚举（重读元素后重试、换用 index+令牌、先关遮挡物、等待后重试……）。`hint` 保留给人看。MCP 的紧凑文本也按策略生成一行指引。

成本：低。纯 daemon + MCP。

### 4. 真人看不到 agent 点了哪里

**它怎么做。** 可选的“agent 光标”覆盖层：动作前光标移动到目标，渲染确认到位后才派发，点击时有涟漪和元素描边。多 agent 时每个 agent 一个光标。纯展示，不改变动作语义。

**我们现在。** 网页面板的涟漪（`web/index.html` 的 `spawnRipple`）只画人自己的点击。agent 的动作只在 `/agent/activity` 里出一行字，坐标点击的那一行文字里有坐标（`crates/server/src/activity.rs:147`），但只是拼进字符串，没有结构化字段。

**建议。** activity 条目加上结构化的点击坐标（元素点击再带 rect）。面板轮询 activity 时，带坐标的条目在实时画面上画一个和人点击不同颜色的涟漪；元素点击把元素 rect 也带上，画一下描边。不需要“先画后点”的握手，事后画就够。宣传视频和远程看护都直接受益。

成本：低。这是最小的可见改进。

### 5. 派发前不检查 enabled / 稳定性

**它怎么做。** 每个动作有一组前置检查：visible、stable、enabled、supported_action、policy、editable、receives_events（命中测试，看有没有被别的东西挡住）。暂时性的检查（visible、stable、enabled、receives_events）每约 100 ms 轮询，默认 5 s 预算；终态性的（不支持该动作、策略不允许、不可编辑）立即失败。稳定性是连续 3 次、至少 34 ms 内边框不变（误差 0.5pt）。

**我们现在。** `tap_target_refusal`（`http.rs:8491`）拒绝不可见和完全被遮挡；离屏目标走滚动露出再点。没有 enabled 检查，没有稳定性检查，派发前不自动等待。

**成本模型完全不同，不能照搬。** 它的原生读取是本机毫秒级；我们一次整树读要 1.8–3.6 s，经过 Wi-Fi 中转。整树轮询式的自动等待不可行。可行的是：

- enabled：重新定位时 runner 已经拿到了元素，顺带返回 enabled，禁用就回明确错误码（不是派发后“没反应”）。几乎零成本。
- 稳定性：只对单个元素读两次 frame（runner 内完成，不过网络），用于动画中的按钮。按需开启，比如 sheet 刚弹出时。

成本：enabled 低；稳定性中，要改 runner。需要真机测：禁用按钮（未填表单的“下一步”）、sheet 弹出动画中点按钮。

### 6. 没有子树下钻

**它怎么做。** `--skeleton` 只读 3 层，被截断的容器给出 `children_count` 和一个可下钻的 ref；`--root <ref>` 只读这个子树，并且只替换这个子树原来的 ref，其他区域的 ref 保持有效。下钻要求读全，读不全直接超时，不返回半棵树。

**我们现在。** 有 `scope=interactive|focused|changed`、`query`、`selector`、`limit`、`?since=` delta；没有按子树读。

**建议。** iOS 页面通常比 Electron 小，骨架模式意义不大。子树读取有价值的场景是长列表和设置页：agent 只关心一个 cell 或一个分组时，不必读整屏。需要 runner 支持从某个元素开始读子树。和第 1 条的逐行身份配合，“只替换子树内的 index”也能做。

成本：中，要改 runner。优先级低于 1–5。

### 7. 没有可回放的逐动作轨迹

**它怎么做。** session 下每个进程一个 JSONL 段，按（时间，pid，文件内位置）合并成时间线；可选保存动作前后截图和当时的 ref 表；可以导出 HTML。写进轨迹的值先脱敏：报告形状（字符数、role），不报告原始值。

**我们现在。** `agent-timing.jsonl` 只记路由、耗时、字节数和 owner，不记内容；`/agent/activity` 在内存里；有 MP4 录屏。

**建议。** 在 `phone_run_start` / `phone_run_end` 这对已有的接口上挂一个可选轨迹：每个动作一条（请求、解析到的元素、结果、验证档），可选前后截图。排查“agent 说点了但没反应”时比录屏好用。默认关，因为截图含隐私；`private` hold 期间不写。

成本：中。

### 8. 方法论（不是功能，但值得学）

- **Evidence 三态**：每个属性是 Known / Absent / Unknown，三者不合并成布尔。“没读到”不能当“没有”。我们踩过同一个坑：devicectl 读失败不等于手机已解锁。可以在 runner 返回的 enabled、selected、visible 上统一这个约定。
- **CONCEPTS.md**：一份术语表，每个词有项目内的精确含义。我们的 agent-reference.md 有 10 万字节，但概念散在各处。
- **docs/solutions/**：每个踩过的坑一篇，带 frontmatter（模块、症状、标签），写“问题 / 解法 / 怎么防”。它的 Findings Ledger 给每次测量编号（如 `A15-7`），代码注释直接引用编号。我们的类似知识现在在个人记忆里，不在仓库里。
- **双进程测试**：它有一篇专门讲“点击和等待分两次 CLI 调用时，进程内时钟是空的”。我们的 MCP 每个工具调用也是独立请求，状态都在 daemon 里，这类问题较少，但见下一条。

## 一个需要确认的潜在问题

agent-desktop 记过一个竞态：点击打开菜单后立刻 `wait --menu-closed`，菜单还没出现，第一次读就判“已关闭”。修法是：等待“消失”时，必须先见过它出现，或者距上次动作已超过 2 s。

我们的 `wait_for` 的 `absent` 有同样的形状。`agent_expectation_observation`（`http.rs:11852`）挡住了“稀疏树当作不存在”，但没有挡“还没出现”。典型用法：点“提交” → `wait_for absent: 加载中`。如果点击后第一次读树时加载提示还没冒出来，等待会立刻通过，后续步骤在加载中界面上执行。

没有在真机上复现过，先记为待确认。确认方法：找一个点击后延迟出现加载提示的 App，batch 里 tap 后紧跟 `wait_for absent`，看 `waited_ms` 是否接近 0。若确认，修法可以是给 `absent` 加可选的“先见过再等它消失”，或者在批量里紧跟动作的 `absent` 等待默认带一个最短等待。

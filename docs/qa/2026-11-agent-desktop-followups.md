# Agent Desktop 借鉴项验收

2026-11-04，基于 main `7421a90e` 的后续实现。真机使用 iPhone 17 Pro Max、iOS 27.2、Xcode 27.2；测试 runner 和隔离的开发 daemon，不以已安装旧版本代替本分支。

## 真机结果

测试使用独立的 UIKit 应用，没有操作聊天、支付或个人文档。

| 场景 | 结果 |
|---|---|
| 指定输入框的原生子树 | `coverage=complete`，返回 1 行，只读且不生成 action refs |
| 禁用按钮经 daemon 点击 | 派发前拒绝，`not_sent`，恢复策略为 `wait_until_actionable` |
| 原生单元素证据 | 实读 `enabled=false`，2 次帧采样后 `stable=true` |
| 绕过 daemon、直接请求原生禁用按钮 | HTTP 400、`invalid element state`、明确 `nothing was sent` |
| 非安全输入框 set_value | 精确读回一致；仅 `element_value` 验证通过，未声称应用保存/提交 |
| 输入后键盘弹出，再使用旧快照点击另一行 | 动作成功，响应明确报告发生了 rebound，补齐 PR #400 的该场景验收 |
| 点击后延迟 1.5 秒出现、持续 4 秒的加载提示 | `absent_after_present` 等待约 6 秒后完成；40 次读取，未在提示出现前误判完成 |
| 第一个动作成功、第二个目标不存在 | `applied_actions=1`、`partially_applied`，禁止重放整个批次 |
| run 轨迹 | 记录 8 个事件，结束后仍可读取；输入的测试字符串及字段 identifier 均未进入导出，存储明确标为 `bounded_memory` |

上表的时间和读取次数只描述这个小型测试界面，不代表普通 App 的读屏性能。绿色反馈的坐标映射、留白区域、过期事件过滤由 8 项前端测试验证；这里没有把自动化测试写成人工视觉验收。

## 自动化与构建

- `heavy-cargo +1.96.0 test -p server -p iphone-use-mcp --no-fail-fast -- --test-threads=4`。
- 与 CI 相同的 `cargo +1.96.0 clippy --workspace --locked -- -D warnings`，经机器级限流运行。
- 原生合成用例：56 项 Swift 断言、12 种 ObjC editable 属性情况、79 项输入绑定检查；既有点击送达检查继续执行。
- `node --test scripts/tests/activity-target.test.cjs`：8 项。
- runner 的 generic iOS Simulator `build-for-testing`，以及签名真机产品构建。

测试中的模拟 HTTP 服务统一读完整个请求后再应答，避免连接提前关闭把 JSON 上传误判成投递失败。冷启动就绪测试等待健康检查真正完成，不在固定睡眠后阻塞当前线程；超时送达测试仍区分派发前和派发后。

## 当前边界

子树索引只读；局部序号不是全屏元素 ref。轨迹默认关闭，仅存有界内存，不含截图，不自动重放。元素描边来自快照矩形中心，不是实际点击坐标的证明。真机验证覆盖了稳定元素；持续运动、未知属性、权限在返回前撤销等分支由合成及集成测试覆盖。

测试结束后已移除测试应用和临时 daemon，重新安装正式来源的 runner，恢复原 runner 来源/嵌入配置并交回人手。未把本分支 daemon 部署到主服务。

## 评审后回归

活动历史不保留任意点击坐标、输入框标识符、数字/单字符按键的位置和文案；绿色反馈限于非敏感的命名语义控件。服务器回归覆盖释放和换主时结束轨迹、无观察 scope 的安装结果检查当前 owner、跨应用出现记录不计入指定 App 的消失等待，以及无名称根节点仍提供缓存 viewport。

补充原生测试把生产 clickElement 与 actionNode 接在一起，验证禁用目标返回 400 且零触碰；前端增加隐藏标签页用例；子树路由增加两个匹配时拒绝且不发送子树读取的用例。模拟 HTTP 服务在读取未完成的请求时也能响应停止标记。

位置反馈已与历史分离：历史条目无任何 point/rect；live_target 仅保留最新一个非敏感目标，绑定 owner/隐私/生命周期并在 1.5 秒后清除。回归覆盖过期、换主、隐藏标签页、只返回历史几何时不回放，以及排队响应的撤权保护。

复审补充覆盖 `2 ABC`、换行和本地化数字按键，匿名请求接替过期租约，以及重定位后实时反馈使用新矩形。反馈在动作 ACK 时记录，不等待后续观察；重定位提示保留已有的精确值验证回执。

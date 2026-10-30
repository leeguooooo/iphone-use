# Native 列表验收

用 UIKit App 自己保存的模型和界面状态，核对真实 Native runner、HTTP 及 MCP 的列表结果。脚本只创建专用 Simulator，不会回落到真实 iPhone。

在 GitHub Actions 验证指定的已发布产品：

```sh
gh workflow run simulator-smoke.yml --ref main \
  -f collect_probe_only=true -f collect_release_tag=v0.27.11
```

工作流下载该版本的签名 Mac App 和 runner 源码，验证校验和，再单独构建 Native runner。App 版本、MCP 初始化版本和二进制哈希会写入证据。下载 artifact `native-list-oracle` 查看结果。

四种模式：

- `unique`：32 条相同文案、不同 identifier 的记录，逐条核对模型，并确认 UIKit 滚动及最终页 Native identifier。
- `missing`：32 条相同文案但没有 identifier，验证合并结果明确保留“不完整”，不误报业务数量。
- `reused`：32 条记录复用 identifier、文案不同，逐条核对模型。
- `counter`：8 条记录的位置不动，手势触发值从 `before` 变为 `after`。Native 最终树必须读到 `after`，采集结果仍保留首次值，不能因刷新持续翻页。

本地先构建 runner，传入明确的 `.xctestrun`，把编译时间和会话启动时间分开。使用当前选定的 Xcode；不要修改全局选择。

```sh
python3 scripts/run-collect-simulator.py \
  --daemon /path/to/iphone-use --mcp /path/to/iphone-use-mcp \
  --runner-source /path/to/released/runner --xctestrun /path/to/runner.xctestrun \
  --runtime com.apple.CoreSimulator.SimRuntime.iOS-18-2 --out /new/output/directory
```

运行前检查目标、实例记录、前台 App、窗口状态和原生 viewport。MCP 发送首个采集前，还会在同一 stdio 会话调用只读 `phone_status`，核对 Simulator UDID、可控制状态和 owner。请求不重放；HTTP 错误、MCP 错误、不确定滑动、数据缺失、前后身份检查不符和清理失败都会使验收失败。判定器负例可离线运行：`python3 -I scripts/test-collect-simulator-verdict.py`。

本机有明显内存压力或 swap 时，使用隔离 CI，不中断其他会话。脚本会在退出时清理本轮创建的 Simulator，并保留模型、Native 树、响应和无凭据日志。失败时的 `failure-ui.png` 是该专用 Simulator 的帧缓冲诊断，不是原生系统截图保存的验收。

这组用例不替代真实 iPhone 验收或同环境的竞品对照，也不保证任意 App 的精确业务记录计数。

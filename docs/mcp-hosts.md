# WorkBuddy、豆包工作的 MCP 接入

本分支新增的候选适配已做安装器和协议回归，尚未在 WorkBuddy、豆包工作的实际界面验收。当前稳定版未必包含这些改动；使用前核对安装包是否包含本适配。手机控制、签名和连接配置仍由同一个 iphone-use daemon 管理，无需为每个宿主重建 runner。

正式安装入口：

```sh
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

## WorkBuddy

包含本适配的安装器发现 `~/.workbuddy/` 时，会合并 `mcp.json` 里的 MCP 项，保存备份，保留其他连接器。已有同名但配置不同的连接器会保留并提示，不自动替换。`IPHONE_USE_NO_CLIENT_SETUP=1` 跳过桌面客户端注册；命名实例会同时写入 `IPHONE_USE_INSTANCE`。

若手动配置，命令填实际账号下的绝对路径，参数为空，环境变量加 `IPHONE_USE_MCP_HOST=workbuddy`：

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "args": [],
      "env": { "IPHONE_USE_MCP_HOST": "workbuddy" }
    }
  }
}
```

在 WorkBuddy 的连接器管理中信任该连接器，然后新开会话。技能目录是 `~/.agents/skills/iphone-use`；按宿主提供的技能导入方式添加。安装器不修改账号里的信任设置。

需要看手机时调用一次 `phone_screen`。`phone_status` 保持状态读取，不另外开面板。WorkBuddy 模式的面板只显示画面，通过对话里的正常工具操作手机。面板取帧用 MCP 的只读资源，不通过资源执行 Home、点击、解锁或接管；关闭、隐藏面板会停止轮询。

## 豆包工作

在客户端界面新建 STDIO 连接器，命令同上、参数为空，环境变量填 `IPHONE_USE_MCP_HOST=doubao`。技能按客户端的导入方式添加 `~/.agents/skills/iphone-use`。连接器的账号配置和信任设置由客户端管理，安装器不代写。

豆包模式只让 `phone_screen` 打开面板，保留面板原有的取帧和控制工具路径。两种模式都不启动额外画面服务、不开放新的本机端口，仍使用原 daemon 的认证、隐私和设备状态判断。

## 检查

新会话先读 `phone_status`。当前任务需要手机且 `drivable:true` 时，读取 `phone_elements` 验证，再打开 `phone_screen` 看画面。锁屏、交还和隐私状态按返回提示处理；不要为了面板重新连接手机或抢占其他会话。

配置中的命令必须是绝对路径；GUI 的 PATH 不保证包含 shell 里的工具目录。MCP 桥接本身是原生二进制，不需要 Node 或 Python 启动。该候选版本对回环 daemon 直接连接，不受 GUI 的 HTTP 代理变量影响；远程 daemon 保留代理设置，且需要显式配置自己的认证。

截图返回隐私拒绝时，面板会清除上一张画面；即使并行的状态读取还显示画面开放，也以截图拒绝为准。

本分支验证包含：隔离安装器、真实 stdio 请求、禁止面板工具调用的模拟宿主、MCP 进程重启、隐私画面清除和工具白名单限制。实际宿主信任、面板复用、显示帧率及完整任务完成率仍需单独验收，不能由这些回归替代。

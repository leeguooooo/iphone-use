# Glama directory and sandbox

Copy the values in [build-spec.json](build-spec.json) into the Glama Dockerfile administration form. Build and publish there after testing; this file does not configure Glama automatically. Keep the commit pin empty so future directory builds follow the repository.

The form's CMD arguments contain only `iphone-use-mcp`; Glama adds its `mcp-proxy --` wrapper. Do not copy that generated wrapper back into the form.

The image preloads the checksum-verified public flow registry with `iphone-use-mcp flow update`. This makes `phone_flow_list` and `phone_flow_info` useful in a sandbox that has no phone. Registry metadata is not evidence that a flow was run on a device.

For a browser smoke check, select `IPHONE_USE_MCP_TOOLS=readonly`, connect the Inspector, and call `phone_flow_list` with empty arguments. It should return installed registry entries. The default `all` profile still exposes the full tool catalog for inspection and grading. Remove unused optional environment rows rather than submitting empty instance names or tokens. Daemon-only configuration belongs on the operator's Mac.

A Glama container has no local iPhone daemon. Device tools need the operator's separately configured Mac and device; a failed device read in this container is not a reason to reconnect the operator's phone. Do not put credentials into a shared Inspector URL.

## 中文

按 [build-spec.json](build-spec.json) 填写 Glama 的 Dockerfile 管理表单，构建验收后在 Glama 发布。此文件不会自动修改平台配置。提交锁定保持为空，CMD 只填 `iphone-use-mcp`，不重复填写平台生成的代理包装。

镜像会预装经校验的公开流程库。浏览器验收时选择 `readonly`，连接 Inspector，空参数调用 `phone_flow_list`，应返回流程条目；这只验证目录查询，不代表执行了真机流程。默认 `all` 保留完整工具发现。删除不用的可选环境变量行，不提交空实例名或空令牌。手机操作仍需要用户自己的 Mac 和设备。

# 手机内执行网站操作

用户确认的范围：浏览器完全在手机运行，脱离电脑；复用 chrome-use site 的公开适配器。本轮独立物理验收使用 iPhone 14；iPhone 17 的后续安装与统一助手验证由协调会话负责。

App 使用当前手机浏览器账号的 Cookie 和网站数据，在自己的 WKWebView 页面执行兼容适配器。App 带有 26 个公开、附带授权声明的手机适配器；首次启动浏览器工作区即可安装。手机可直接从公开站点库更新，不需要桌面 CLI、电脑登录态导出或 Mac 服务。更新固定 Git commit，逐文件核对 Git blob 哈希，再原子替换网站库；失败保留旧版本。

2026-10-11 的 iOS 模拟器实测：直接下载两个公开网站库用时约 7 秒，得到 160 个手机兼容操作，排除 18 个不兼容操作。合成适配器在 WKWebView 内读取页面 DOM 并接收参数，已通过。工具计划校验禁止 desktop 步骤；电脑 Chrome 连接入口已从 App 移除。依赖 Node.js、OpenCLI 或电脑文件接口的命令显示为手机不支持。

写入仍使用屏幕确认。写入结果未知时保留核对状态，不能自动重复；更新网站库也不能在任务执行过程中替换适配器。通话中聊天、手机操作和网站操作由现有助手执行链路区分。

## 验证

```sh
# 普通回归不会访问公开服务。
xcodebuild -scheme IPhoneUseRemote -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:IPhoneUseRemoteTests/PhoneSiteCatalogTests \
  -only-testing:IPhoneUseRemoteTests/PhoneOnlyAssistantPolicyTests test
# 显式验证手机进程直接更新网站库：
TEST_RUNNER_PHONE_SITE_LIVE_TEST=1 xcodebuild -scheme IPhoneUseRemote \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:IPhoneUseRemoteTests/PhoneSiteCatalogTests/testActualPublicPacksDownloadOnPhoneWithoutDesktopTooling test
```

同步主分支后，在独立 iPhone 14 / iOS 26.5 模拟器上重新复核：8 项测试全部通过，包括实际公开网站库下载（约 13.7 秒，160 个兼容操作、18 个不兼容操作）、WKWebView 执行、损坏更新拒绝和禁止 desktop 计划。首次安装因磁盘空间不足、模拟器容器元数据写入失败而未进入测试；清理自有缓存后复测通过。最新日志：`/tmp/iphone-use-site-final-retry.log`。预装包与重新生成结果逐字一致。

## iPhone 14 验收条件

此前 `guouli` 的 Mac Wi-Fi runner 启动返回 `wifi_automation_refused`；不要重复 Wi-Fi 重连。共享分支的 [统一助手交接记录](2026-10-11-unified-browser-handoff.md) 已记录 build 11 在 iPhone 14 通过合成网页写入和会话解密，跨 App 控制仍待独立验证。PhoneSiteCatalog 的模拟器结果不能代替真实网站与语音验收。

浏览器与语音聊天不需要本机执行服务。跨 App 控制由手机内的设备 runner 执行；现有架构在 runner 退出或重启后仍需 Mac 重新拉起。运行时不经过 Mac，不等于冷启动恢复也已脱离 Mac。手机端启动与首次配对的实验实现位于 `feat/phone-self-launch` 的 `1c8c16b`，尚未接入主 App，真实认证、后台保活和断开 Mac 验收未通过。

真机需继续验证：自然语音识别和插话、扬声器回声、切换其他 App 时持续收音、蓝牙切换、系统来电中断；网站登录态在重启后保留、实际网站读操作、合成写入确认和未知结果恢复。

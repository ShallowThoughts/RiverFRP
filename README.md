# RiverFRP

Windows 一键内网穿透客户端。无需手写 TOML，也无需手动分配公网端口。

## 下载

从 [GitHub Releases](https://github.com/ShallowThoughts/RiverFRP/releases/latest) 下载最新的 Windows EXE。

## 使用

1. 双击 `RiverFRP-v1.0.0-win-x64.exe`。
2. 点击“继续登录”，浏览器会打开 `frp.lxixi.top`。
3. 使用独立 RiverFRP 账号登录，并允许这台设备。
4. 返回 RiverFRP，选择 TCP 或 UDP，填写本地端口。
5. 点击“一键启动并自动分配端口”，等待界面显示 `frp.lxixi.top:端口`。

客户端背景由 `https://api.lxixi.top/anime` 异步加载；接口暂时不可用时仍可正常使用隧道功能。

没有账号时，可以使用现有的邮箱注册、6 位验证码、忘记密码和密码重置流程。

邮箱密码只在 HTTPS 网站中输入，不会交给 RiverFRP 客户端。授权完成后，服务器为每台电脑签发独立设备凭证；凭证使用 Windows DPAPI 加密保存在当前 Windows 用户目录。

RiverFRP 账号与 RiverXILee Store 账号完全独立，即使使用同一个邮箱也不会互通。设备可以在 [RiverFRP 设备页面](https://frp.lxixi.top/riverfrp/devices)中单独撤销。

## 架构与限制

- 客户端内嵌从 FRP v0.64.0 源码构建的 `frpc.exe`。
- 服务器使用 OIDC client-credentials 验证，不向客户端分发 FRPS 主密钥。
- 仅允许 TCP/UDP，公网端口范围为 20000–49999。
- 每个客户端最多同时创建 5 个代理。
- 自动分配的端口不是永久固定资源；重连时服务器会尽量保留原端口。
- 发布文件未购买商业代码签名证书，Windows SmartScreen 仍可能显示“未知发布者”。

FRP 的第三方许可会在首次运行时写入：
`%LOCALAPPDATA%\RiverFRP\THIRD_PARTY_NOTICES.txt`。

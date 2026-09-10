# Codex 键盘额度工具

[![Windows CI](https://github.com/smallcat2333/codex-keyboard-quota/actions/workflows/ci.yml/badge.svg)](https://github.com/smallcat2333/codex-keyboard-quota/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Display Codex usage limits on a TICKTYPE DP-104 keyboard. Windows only.

这是个人开发的社区工具，与 OpenAI 或 TICKTYPE 无官方隶属关系。

Windows x64 单文件 GUI 工具，把当前 Codex 登录账号的 5 小时与周额度显示到 TICKTYPE DP-104 点阵屏。

当前版本：`0.1.0`

## 前置条件

- Windows 10/11 x64。
- TICKTYPE DP-104 已连接，使用前关闭 `cfg.ticktype.com` 等正在占用 HID 的网页驱动。
- 本机已安装并登录 Codex。程序通过本机 `codex app-server --stdio` 复用 Codex 登录缓存，不读取、复制或分发账号凭据。登录方式可参考 [OpenAI Codex 身份验证说明](https://learn.chatgpt.com/zh-Hans/docs/auth?surface=app)。

## 显示规则

- 固定布局：`5h | 周 | 2×5 周重置沙漏`。
- 缺失的限额窗口显示绿色 `--`；100% 在两位点阵中显示为 `99`。
- 数字颜色：`≥60%` 绿色、`20–59%` 黄色、`<20%` 红色。
- 两道竖线为白色，周重置沙漏为粉紫色。
- 周沙漏从 10 格逐步归零，按从左到右、从上到下的顺序消失。

## 使用

直接运行 `CodexKeyboardQuotaTool.exe`：

- `测试显示`：查询当前真实额度，忽略变化阈值并立即写入键盘，同时验证 HID 回显。
- `一键部署`：复制程序到 `%LOCALAPPDATA%\Smallcat\CodexKeyboardQuota\`，替换同名旧 Python 任务并注册当前用户计划任务 `Codex DP-104 Quota`，无需管理员权限。
- `一键移除`：删除计划任务、部署副本、注册表缓存和后台日志，不删除用户下载的原始便携 EXE。若从部署副本启动，窗口退出后自动删除自身。

部署后每分钟检测一次额度，计划任务禁止重叠且单次最长运行 45 秒：

- 5h 额度变化 `≥5%` 时刷新。
- 周额度变化 `≥2%` 时刷新。
- 任一当前额度低于 5% 且发生变化时立即刷新。
- 沙漏变化不能单独触发写入，只在额度触发刷新时一并更新。

后台普通跳过不写日志；只有实际刷新和错误会追加到：

```text
%LOCALAPPDATA%\Smallcat\CodexKeyboardQuota\CodexKeyboardQuota.log
```

额度缓存继续兼容 Python 版：

```text
HKCU\Software\Smallcat\CodexKeyboardQuota
```

## 构建与分享

环境：Rust stable，目标 `x86_64-pc-windows-msvc`。

```powershell
cargo test --locked
cargo build --release --locked
```

Release 产物：

```text
target\release\CodexKeyboardQuotaTool.exe
```

发布构建启用了 Windows GUI 子系统，GUI、计划任务、Codex/Node 和 PowerShell 子进程均不会显示控制台黑框。应用直接使用 Windows 原生 HID 后端，不需要 Python 或额外 HID DLL。

## 故障排查

- “找不到 Codex CLI”：先确认 Codex 已安装并登录；程序优先寻找 Codex Desktop 自带的原生 `codex.exe`，再寻找 PATH/npm 安装。
- “未找到 DP-104”：重新连接键盘，并关闭可能占用 HID 的网页驱动或其他配置软件。
- “HID 响应校验失败”：关闭网页驱动后重试“测试显示”，不要让计划任务和配置网页同时操作键盘。
- 固件可能在静态额度出现前短暂显示 `CUSTOM` 或原有吃豆人动画。这是 DP-104 固件切换自定义帧时的行为，软件层无法承诺消除。
- 点击“一键移除”后，原始便携 EXE仍保留；部署目录中的运行副本会在窗口退出后清理。

## 独立维护

本仓库根目录是当前 Rust GUI 工程。旧 Python 实现保存在 `legacy/codex_keyboard_quota.py`，仅供历史参考；当前使用和构建不依赖 Python。

可从仓库 Actions 中已通过的 Windows CI 运行下载构建产物。CI 运行单元测试并构建 EXE，不访问真实账号、不控制键盘或安装计划任务。

## 贡献与许可

参与方式见 [CONTRIBUTING.md](CONTRIBUTING.md)，源码采用 [MIT License](LICENSE)。

## 变更记录

- `2026-09-10 | 0.1.0 | 迁移为独立 GitHub 仓库，保留 Python 历史实现，补充 MIT 许可、贡献指南和 Windows CI。`

- `2026-09-04 | 0.1.0 | 首个 Rust GUI 版本：Codex app-server 查询、DP-104 原生 HID、阈值刷新与当前用户一键部署/移除。`

开发者：smallcat

项目反馈请使用 GitHub Issues。

编号：260904

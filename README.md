# Codex 键盘额度工具

[![Windows CI](https://github.com/smallcat2333/codex-keyboard-quota/actions/workflows/ci.yml/badge.svg)](https://github.com/smallcat2333/codex-keyboard-quota/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Display Codex usage limits on a TICKTYPE DP-104 keyboard. Windows only.

这是个人开发的社区工具，与 OpenAI 或 TICKTYPE 无官方隶属关系。

Windows x64 单文件 GUI 工具，把当前 Codex 官方账号的 5 小时与周额度，或 CCSwitch 当前选中中转站的余额显示到 TICKTYPE DP-104 点阵屏。

当前版本：`0.1.0`

## 前置条件

- Windows 10/11 x64。
- TICKTYPE DP-104 已连接，使用前关闭 `cfg.ticktype.com` 等正在占用 HID 的网页驱动。
- 本机已安装并登录 Codex。程序通过本机 `codex app-server --stdio` 复用 Codex 登录缓存，不读取、复制或分发账号凭据。登录方式可参考 [OpenAI Codex 身份验证说明](https://learn.chatgpt.com/zh-Hans/docs/auth?surface=app)。
- 中转站模式读取 `%USERPROFILE%\.cc-switch\settings.json` 的 `currentProviderCodex`（缺失时读取数据库当前标记），只读 `cc-switch.db`，执行该供应商已启用的 JavaScript 额度查询脚本；请求凭据只在内存中使用，不写入日志、缓存或 EXE。无需运行 CCSwitch，也无需额外安装 Node/Python。

## 显示规则

- CCSwitch 选中官方供应商时，保留布局：`5h | 周 | 2×5 周重置沙漏`；未安装 CCSwitch 时也沿用官方查询。
- 缺失的限额窗口显示绿色 `--`；100% 在两位点阵中显示为 `99`。
- 数字颜色：`≥60%` 绿色、`20–59%` 黄色、`<20%` 红色。
- 两道竖线为白色，周重置沙漏为粉紫色。
- 周沙漏从 10 格逐步归零，按从左到右、从上到下的顺序消失。
- 选中中转站时显示 `余额 | 消耗柱`。余额去掉小数、限制在 `0–999`，例如 `148.9 → 148`、`22.3 → 22`、`1481 → 999`；数字间距为 1 列，右对齐占 11 列。
- 白色竖线在第 13 列，左右各留 1 列；第 15–24 列显示 10 根五点柱，覆盖最近 5 小时，最右侧最新。
- 每根柱表示一个已完成 30 分钟周期的累计消耗（不是每分钟平均），每 30 分钟向左推移一次；从底部向上点亮：`<0.1` 为 0 点，`[0.1,1)` 为 1 点绿色，`[1,2)` 为 2 点绿色，`[2,5)` 为 3 点蓝色，`[5,10)` 为 4 点橙色，`≥10` 为 5 点整柱红色。0 档不点亮。
- 每分钟采样并累加余额下降值，充值不抵扣已记录的消耗。统计按供应商和单位隔离并写入注册表，跨进程/重启保留。首次采样满 30 分钟后生成第一根柱；查询失败或采样间隔超过 2 分钟的周期留空，不伪造消耗。余额下降发生在两次采样之间，周期边界的一次采样归入刚结束的周期。
- 余额沿用 CCSwitch 返回的数值和单位，不换算汇率；单位在 GUI 中显示，键盘只显示数字。
- 未配置查询、查询失败、结果没有余额或多余额套餐无法唯一确定时显示 `--`，GUI 说明原因，不回退到官方额度。

## 使用

直接运行 `CodexKeyboardQuotaTool.exe`：

- `测试显示`：查询当前真实额度，忽略变化阈值并立即写入键盘，同时验证 HID 回显。
- `一键部署`：复制程序到 `%LOCALAPPDATA%\Smallcat\CodexKeyboardQuota\`，替换同名旧 Python 任务并注册当前用户计划任务 `Codex DP-104 Quota`，无需管理员权限。
- `一键移除`：删除计划任务、部署副本、注册表缓存和后台日志，不删除用户下载的原始便携 EXE。若从部署副本启动，窗口退出后自动删除自身。

部署后每分钟检测一次额度，计划任务禁止重叠且单次最长运行 45 秒：

- 中转站余额相对上次成功显示值累计变化 `≥1` 时刷新（例如 USD 余额变化 `≥1 USD`），增加和减少均适用。
- 消耗柱每完成一个 30 分钟周期独立触发刷新，不受余额变化阈值限制。余额封顶显示 `999` 时仍使用真实余额计算消耗。
- 官方与中转站互切、中转站供应商切换、余额可用状态或单位改变时，在下一次每分钟检测时刷新，不受变化阈值限制。
- 官方模式继续使用以下规则：
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

- `2026-09-11 | 0.1.0 | 按 CCSwitch 当前供应商切换官方额度与中转余额；余额最多三位整数、封顶 999，每分钟采样、变化至少 1 时刷新，竖线后展示最近 5 小时的十根半小时累计消耗柱。`

- `2026-09-10 | 0.1.0 | 迁移为独立 GitHub 仓库，保留 Python 历史实现，补充 MIT 许可、贡献指南和 Windows CI。`

- `2026-09-04 | 0.1.0 | 首个 Rust GUI 版本：Codex app-server 查询、DP-104 原生 HID、阈值刷新与当前用户一键部署/移除。`

开发者：smallcat

项目反馈请使用 GitHub Issues。

编号：260904

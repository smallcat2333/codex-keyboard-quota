//! 通过本机 `codex app-server --stdio` 读取当前登录账号的限额。

use crate::quota::{
    FIVE_HOUR_WINDOW_MINUTES, QuotaStatus, SEVEN_DAY_WINDOW_MINUTES, calculate_week_reset_cells,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(15);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 一条可直接交给 `Command` 的 Codex 启动命令。
#[derive(Clone, Debug)]
pub struct ResolvedCodexCommand {
    program: PathBuf,
    prefix_args: Vec<OsString>,
    pub display: String,
}

/// app-server 标准输出读取线程发送给主线程的消息。
#[derive(Debug)]
enum ReaderMessage {
    Line(String),
    Eof,
}

/// 持有 app-server 子进程与 JSON-RPC 管道。
struct CodexAppServer {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<ReaderMessage>,
    next_request_id: u64,
}

impl CodexAppServer {
    /// 启动隐藏 app-server，并完成初始化握手。
    fn start(command: &ResolvedCodexCommand) -> Result<Self> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.prefix_args)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_hidden(&mut process);

        let mut child = process
            .spawn()
            .with_context(|| format!("无法启动 Codex：{}", command.display))?;
        let stdin = child.stdin.take().context("无法连接 Codex 标准输入")?;
        let stdout = child.stdout.take().context("无法连接 Codex 标准输出")?;
        let (sender, messages) = mpsc::channel();
        thread::Builder::new()
            .name("codex-app-server-reader".to_owned())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) if !line.trim().is_empty() => {
                            if sender.send(ReaderMessage::Line(line)).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let _ = sender.send(ReaderMessage::Eof);
            })
            .context("无法创建 Codex 响应读取线程")?;

        let mut server = Self {
            child,
            stdin,
            messages,
            next_request_id: 0,
        };
        server.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codex-keyboard-quota",
                    "title": "Codex Keyboard Quota",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )?;
        server.send_notification("initialized")?;
        Ok(server)
    }

    /// 发出一个 JSON-RPC 请求并返回 `result` 字段。
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_request_id += 1;
        let request_id = self.next_request_id;
        self.send_json(&json!({
            "id": request_id,
            "method": method,
            "params": params,
        }))?;
        let response = wait_for_response(&self.messages, request_id, APP_SERVER_TIMEOUT)?;
        response_result(response)
    }

    /// 发出一个不要求响应的 JSON-RPC 通知。
    fn send_notification(&mut self, method: &str) -> Result<()> {
        self.send_json(&json!({ "method": method }))
    }

    /// 把单行 JSON 写入 app-server 并立即刷新管道。
    fn send_json(&mut self, message: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, message).context("无法编码 Codex 请求")?;
        self.stdin.write_all(b"\n").context("无法写入 Codex 请求")?;
        self.stdin.flush().context("无法刷新 Codex 请求管道")
    }
}

impl Drop for CodexAppServer {
    /// 关闭输入管道并确保 app-server 不遗留后台进程。
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 查询当前登录账号的 Codex 额度，并返回实际使用的 CLI 路径说明。
pub fn query_current_quota() -> Result<(QuotaStatus, String)> {
    let command = resolve_codex_command()?;
    let mut server = CodexAppServer::start(&command)?;
    let result = server.request("account/rateLimits/read", Value::Null)?;
    let status = parse_rate_limits(&result, chrono::Utc::now().timestamp())?;
    Ok((status, command.display))
}

/// 从 JSON-RPC 消息队列等待指定请求编号，忽略通知和非 JSON 输出。
fn wait_for_response(
    messages: &Receiver<ReaderMessage>,
    request_id: u64,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("等待 Codex app-server 响应超时");
        }
        match messages.recv_timeout(remaining) {
            Ok(ReaderMessage::Line(line)) => {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if json_id_matches(message.get("id"), request_id) {
                    return Ok(message);
                }
            }
            Ok(ReaderMessage::Eof) | Err(RecvTimeoutError::Disconnected) => {
                bail!("Codex app-server 已退出")
            }
            Err(RecvTimeoutError::Timeout) => bail!("等待 Codex app-server 响应超时"),
        }
    }
}

/// 兼容数字或字符串形式的 JSON-RPC 请求编号。
fn json_id_matches(value: Option<&Value>, request_id: u64) -> bool {
    value.and_then(Value::as_u64) == Some(request_id)
        || value.and_then(Value::as_str) == Some(request_id.to_string().as_str())
}

/// 提取 JSON-RPC 结果，并将服务端错误转成可读错误。
fn response_result(response: Value) -> Result<Value> {
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        bail!("Codex app-server 请求失败：{error}");
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("Codex app-server 响应缺少 result"))
}

/// 解析 `rateLimitsByLimitId.codex` 或旧版 `rateLimits` 快照。
pub fn parse_rate_limits(result: &Value, now: i64) -> Result<QuotaStatus> {
    let snapshot = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .and_then(|limits| limits.get("codex"))
        .filter(|value| value.is_object())
        .or_else(|| result.get("rateLimits"))
        .and_then(Value::as_object)
        .context("Codex 响应中没有 rate limits 数据")?;

    let mut five_hour = None;
    let mut seven_day = None;
    let mut seven_day_resets_at = None;
    for position in ["primary", "secondary"] {
        let Some(window_value) = snapshot.get(position) else {
            continue;
        };
        if window_value.is_null() {
            continue;
        }
        let window = window_value
            .as_object()
            .with_context(|| format!("Codex {position} 窗口格式错误"))?;
        let duration = window
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .with_context(|| format!("Codex {position} 窗口缺少 windowDurationMins"))?;
        let remaining = remaining_percent(window, position)?;
        match duration {
            FIVE_HOUR_WINDOW_MINUTES => {
                if five_hour.replace(remaining).is_some() {
                    bail!("Codex 返回重复的 5h 限额窗口");
                }
            }
            SEVEN_DAY_WINDOW_MINUTES => {
                if seven_day.replace(remaining).is_some() {
                    bail!("Codex 返回重复的周限额窗口");
                }
                seven_day_resets_at = parse_reset_timestamp(window.get("resetsAt"))?;
            }
            _ => bail!("无法识别 Codex 限额窗口时长：{duration}"),
        }
    }

    let reset_cells = calculate_week_reset_cells(seven_day_resets_at, now);
    Ok(QuotaStatus {
        five_hour,
        seven_day,
        seven_day_resets_at,
        reset_cells,
    })
}

/// 把服务端已用百分比转换成 0 到 100 的整数剩余百分比。
fn remaining_percent(window: &serde_json::Map<String, Value>, name: &str) -> Result<u8> {
    let used = window
        .get("usedPercent")
        .and_then(Value::as_f64)
        .with_context(|| format!("Codex {name} 窗口没有 usedPercent"))?;
    if !used.is_finite() {
        bail!("Codex {name} 窗口 usedPercent 不是有限数值");
    }
    Ok((100.0 - used).round_ties_even().clamp(0.0, 100.0) as u8)
}

/// 解析可选周重置时间戳，存在但类型错误时直接报告协议异常。
fn parse_reset_timestamp(value: Option<&Value>) -> Result<Option<i64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64))
            .map(Some)
            .context("Codex 周窗口 resetsAt 格式错误"),
    }
}

/// 定位 Codex：显式配置优先，其后是原生 EXE，最后才是 npm 包装程序。
pub fn resolve_codex_command() -> Result<ResolvedCodexCommand> {
    if let Some(configured) = env::var_os("CODEX_CLI").filter(|path| !path.is_empty()) {
        return command_from_path(PathBuf::from(configured));
    }

    if let Some(native) = newest_bundled_codex() {
        return Ok(direct_command(native));
    }
    if let Some(native) = find_on_path(&["codex.exe"]) {
        return Ok(direct_command(native));
    }
    if let Some(wrapper) = find_on_path(&["codex.cmd", "codex.bat", "codex.ps1", "codex"]) {
        return command_from_path(wrapper);
    }
    bail!("找不到 Codex CLI，请先安装并登录 Codex")
}

/// 将候选路径转换成原生、Node 或隐藏脚本启动命令。
fn command_from_path(path: PathBuf) -> Result<ResolvedCodexCommand> {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "exe" || extension.is_empty() {
        return Ok(direct_command(path));
    }

    if matches!(extension.as_str(), "cmd" | "bat" | "ps1") {
        if let Some(parent) = path.parent() {
            let script = parent
                .join("node_modules")
                .join("@openai")
                .join("codex")
                .join("bin")
                .join("codex.js");
            if script.is_file()
                && let Some(node) = find_on_path(&["node.exe", "node"])
            {
                return Ok(ResolvedCodexCommand {
                    display: format!("{} ({})", node.display(), script.display()),
                    program: node,
                    prefix_args: vec![script.into_os_string()],
                });
            }
        }
    }

    match extension.as_str() {
        "cmd" | "bat" => Ok(ResolvedCodexCommand {
            display: path.display().to_string(),
            program: PathBuf::from("cmd.exe"),
            prefix_args: vec![
                OsString::from("/d"),
                OsString::from("/s"),
                OsString::from("/c"),
                path.into_os_string(),
            ],
        }),
        "ps1" => Ok(ResolvedCodexCommand {
            display: path.display().to_string(),
            program: powershell_path(),
            prefix_args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                path.into_os_string(),
            ],
        }),
        _ => bail!("不支持的 CODEX_CLI 文件类型：{}", path.display()),
    }
}

/// 创建直接执行原生 Codex 的命令描述。
fn direct_command(path: PathBuf) -> ResolvedCodexCommand {
    ResolvedCodexCommand {
        display: path.display().to_string(),
        program: path,
        prefix_args: Vec::new(),
    }
}

/// 在桌面应用的版本目录中选择最近修改的原生 Codex EXE。
fn newest_bundled_codex() -> Option<PathBuf> {
    let root = dirs::data_local_dir()?
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let mut candidates = Vec::new();
    collect_named_files(&root, "codex.exe", 3, &mut candidates);
    candidates.into_iter().max_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    })
}

/// 在有限目录深度内递归收集指定文件名，避免扫描整个用户目录。
fn collect_named_files(root: &Path, name: &str, depth: u8, output: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|file_name| file_name.eq_ignore_ascii_case(name))
        {
            output.push(path);
        } else if path.is_dir() {
            collect_named_files(&path, name, depth - 1, output);
        }
    }
}

/// 按 Windows PATH 顺序寻找第一个存在的候选文件。
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 选择 PowerShell 7，缺失时回退 Windows PowerShell。
fn powershell_path() -> PathBuf {
    let pwsh = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    if pwsh.is_file() {
        pwsh
    } else {
        PathBuf::from("powershell.exe")
    }
}

/// 为 Windows 子进程关闭控制台窗口；非 Windows 构建不修改命令。
fn configure_hidden(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造标准两窗口 app-server 返回值。
    fn normal_result() -> Value {
        json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {
                        "usedPercent": 28.0,
                        "windowDurationMins": 300,
                        "resetsAt": 1_000_100
                    },
                    "secondary": {
                        "usedPercent": 66.0,
                        "windowDurationMins": 10080,
                        "resetsAt": 1_604_800
                    }
                }
            }
        })
    }

    /// 验证按窗口时长识别，而不是依赖 primary/secondary 顺序。
    #[test]
    fn parses_normal_and_reordered_windows() {
        let status = parse_rate_limits(&normal_result(), 1_000_000).unwrap();
        assert_eq!(status.five_hour, Some(72));
        assert_eq!(status.seven_day, Some(34));
        assert_eq!(status.reset_cells, Some(10));

        let reordered = json!({
            "rateLimits": {
                "primary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1_302_400},
                "secondary": {"usedPercent": 56, "windowDurationMins": 300}
            }
        });
        let status = parse_rate_limits(&reordered, 1_000_000).unwrap();
        assert_eq!((status.five_hour, status.seven_day), (Some(44), Some(82)));
    }

    /// 验证缺少 5h 或周窗口时分别按无限额处理。
    #[test]
    fn treats_missing_windows_as_unlimited() {
        let only_week = json!({
            "rateLimits": {
                "primary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1_302_400}
            }
        });
        let status = parse_rate_limits(&only_week, 1_000_000).unwrap();
        assert_eq!(status.five_hour, None);
        assert_eq!(status.seven_day, Some(82));

        let only_five = json!({
            "rateLimits": {
                "primary": {"usedPercent": 18, "windowDurationMins": 300}
            }
        });
        let status = parse_rate_limits(&only_five, 1_000_000).unwrap();
        assert_eq!(status.five_hour, Some(82));
        assert_eq!(status.seven_day, None);
        assert_eq!(status.reset_cells, None);
    }

    /// 验证登录失败的 JSON-RPC 错误不会被误当成额度。
    #[test]
    fn reports_not_logged_in_server_error() {
        let error = response_result(json!({
            "id": 2,
            "error": {"code": -32000, "message": "not logged in"}
        }))
        .unwrap_err();
        assert!(error.to_string().contains("not logged in"));
    }

    /// 验证无响应时按超时返回，不会永久阻塞计划任务。
    #[test]
    fn reports_response_timeout() {
        let (_sender, receiver) = mpsc::channel();
        let error = wait_for_response(&receiver, 1, Duration::from_millis(2)).unwrap_err();
        assert!(error.to_string().contains("超时"));
    }

    /// 验证协议窗口时长变化时明确报错，避免显示错误额度。
    #[test]
    fn rejects_unknown_protocol_window() {
        let changed = json!({
            "rateLimits": {
                "primary": {"usedPercent": 20, "windowDurationMins": 1440}
            }
        });
        let error = parse_rate_limits(&changed, 1_000_000).unwrap_err();
        assert!(error.to_string().contains("无法识别"));
    }
}

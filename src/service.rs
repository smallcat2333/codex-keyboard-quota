//! 查询、阈值判断、HID 写入和后台日志的业务编排。

use crate::codex;
use crate::deploy;
use crate::keyboard::{self, Dp104Keyboard};
use crate::quota::{self, QuotaStatus};
use anyhow::{Context, Result};
use chrono::Local;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::process::ExitCode;

/// 一次刷新请求最终采取的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshAction {
    Refreshed,
    SkippedThreshold,
}

/// 一次查询及可选 HID 刷新的完整结果。
#[derive(Clone, Debug)]
pub struct RefreshOutcome {
    pub status: QuotaStatus,
    pub action: RefreshAction,
    pub codex_command: String,
    pub checked_at: String,
}

impl RefreshOutcome {
    /// 生成适合详情框或后台日志的一行结果。
    pub fn summary(&self) -> String {
        match self.action {
            RefreshAction::Refreshed => format!(
                "{} 已刷新键盘：{}，沙漏 {} 格",
                self.checked_at,
                self.status.display_message(),
                reset_cells_text(self.status.reset_cells)
            ),
            RefreshAction::SkippedThreshold => format!(
                "{} 已检测：{}，变化未达阈值，未写键盘",
                self.checked_at,
                self.status.display_message()
            ),
        }
    }
}

/// GUI 初始检测所需的连接与额度快照。
#[derive(Clone, Debug)]
pub struct SystemSnapshot {
    pub keyboard_connected: bool,
    pub keyboard_detail: String,
    pub codex_connected: bool,
    pub codex_detail: String,
    pub deployed: bool,
    pub quota: Option<QuotaStatus>,
    pub checked_at: String,
    pub last_result: String,
}

/// 持有跨进程独占文件锁，生命周期结束时自动释放。
struct RefreshLock {
    file: File,
}

impl Drop for RefreshLock {
    /// 释放刷新锁；操作系统仍会在进程结束时兜底释放句柄。
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// 查询真实额度；强制模式忽略阈值，否则只在规则命中时写入键盘。
pub fn refresh_once(force: bool) -> Result<RefreshOutcome> {
    let _lock = acquire_refresh_lock()?
        .ok_or_else(|| anyhow::anyhow!("另一个刷新进程正在运行，请稍后重试"))?;
    let (status, codex_command) = codex::query_current_quota()?;
    let checked_at = current_time_text();
    let recorded = quota::read_recorded_status()?;
    if !force && !quota::should_refresh(&status, recorded.as_ref()) {
        return Ok(RefreshOutcome {
            status,
            action: RefreshAction::SkippedThreshold,
            codex_command,
            checked_at,
        });
    }

    let frame = keyboard::build_static_frame(&status)?;
    let keyboard = Dp104Keyboard::open()?;
    keyboard.write_custom_frame(&frame)?;
    quota::write_recorded_status(&status)?;
    Ok(RefreshOutcome {
        status,
        action: RefreshAction::Refreshed,
        codex_command,
        checked_at,
    })
}

/// 分别检测 HID、Codex 登录和计划任务状态，单项失败不会遮蔽其他状态。
pub fn collect_system_snapshot() -> SystemSnapshot {
    let checked_at = current_time_text();
    let (keyboard_connected, keyboard_detail) = match Dp104Keyboard::is_connected() {
        Ok(true) => (true, "DP-104 厂商 HID 已连接".to_owned()),
        Ok(false) => (false, "未发现 DP-104 厂商 HID".to_owned()),
        Err(error) => (false, format!("HID 检测失败：{error:#}")),
    };
    let (codex_connected, codex_detail, quota, last_result) = match codex::query_current_quota() {
        Ok((status, command)) => {
            let result = format!("已读取当前账号额度：{}", status.display_message());
            (true, command, Some(status), result)
        }
        Err(error) => {
            let detail = format!("Codex 查询失败：{error:#}");
            (false, detail.clone(), None, detail)
        }
    };

    SystemSnapshot {
        keyboard_connected,
        keyboard_detail,
        codex_connected,
        codex_detail,
        deployed: deploy::is_deployed(),
        quota,
        checked_at,
        last_result,
    }
}

/// 由计划任务调用一次；普通跳过保持静默，只记录真实刷新和错误。
pub fn run_background_refresh() -> ExitCode {
    match refresh_once(false) {
        Ok(outcome) => {
            if outcome.action == RefreshAction::Refreshed {
                append_background_log(&outcome.summary());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            record_background_error(&format!("刷新失败：{error:#}"));
            ExitCode::FAILURE
        }
    }
}

/// 将错误追加到后台日志；日志写入本身失败时不再递归报告。
pub fn record_background_error(message: &str) {
    append_background_log(&format!("{} {message}", current_time_text()));
}

/// 构造由测试显示结果直接更新的 GUI 快照，避免再次查询额度。
pub fn snapshot_from_refresh(outcome: &RefreshOutcome) -> SystemSnapshot {
    SystemSnapshot {
        keyboard_connected: true,
        keyboard_detail: "DP-104 HID 写入及回显校验成功".to_owned(),
        codex_connected: true,
        codex_detail: outcome.codex_command.clone(),
        deployed: deploy::is_deployed(),
        quota: Some(outcome.status.clone()),
        checked_at: outcome.checked_at.clone(),
        last_result: outcome.summary(),
    }
}

/// 尝试取得跨进程文件锁；已被占用时返回 None 而不阻塞。
fn acquire_refresh_lock() -> Result<Option<RefreshLock>> {
    let lock_path = deploy::lock_path()?;
    let parent = lock_path.parent().context("锁文件路径没有父目录")?;
    fs::create_dir_all(parent).context("无法创建刷新锁目录")?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("无法打开刷新锁文件")?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(RefreshLock { file })),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error).context("无法取得刷新文件锁"),
    }
}

/// 追加一行 UTF-8 后台日志，仅供计划任务的刷新或错误路径使用。
fn append_background_log(message: &str) {
    let Ok(path) = deploy::log_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}

/// 返回统一的本地检测时间文本。
fn current_time_text() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 将可选沙漏格数转换为 UI/日志文本。
fn reset_cells_text(cells: Option<u8>) -> String {
    cells
        .map(|value| value.to_string())
        .unwrap_or_else(|| "--".to_owned())
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// 真机验收：读取当前 Codex 账号并强制写入 DP-104，同时验证 HID 回显。
    #[test]
    #[ignore = "需要本机已登录 Codex 且连接 DP-104"]
    fn queries_real_account_and_writes_dp104() {
        let outcome = refresh_once(true).expect("真机额度显示失败");
        assert_eq!(outcome.action, RefreshAction::Refreshed);
        assert_eq!(outcome.status.display_message().len(), 6);
    }
}

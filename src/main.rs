#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Codex 键盘额度工具入口。

mod app;
mod codex;
mod deploy;
mod keyboard;
mod quota;
mod service;

use std::process::ExitCode;

/// 根据启动参数进入 GUI 或计划任务单次刷新模式。
fn main() -> ExitCode {
    if std::env::args_os().any(|argument| argument == "--refresh-once") {
        return service::run_background_refresh();
    }

    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            service::record_background_error(&format!("GUI 启动失败：{error:#}"));
            ExitCode::FAILURE
        }
    }
}

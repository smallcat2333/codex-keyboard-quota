//! 当前用户级安装、计划任务注册与彻底移除。

use crate::quota;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{Duration, Local};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const TASK_NAME: &str = "Codex DP-104 Quota";
const INSTALLED_EXE_NAME: &str = "CodexKeyboardQuotaTool.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// 一键移除结果，指示 GUI 是否需要退出以便删除自身。
#[derive(Clone, Debug)]
pub struct RemovalResult {
    pub exit_after_remove: bool,
    pub details: Vec<String>,
}

/// 返回当前用户的固定部署目录。
pub fn install_root() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .context("无法定位 LOCALAPPDATA")?
        .join("Smallcat")
        .join("CodexKeyboardQuota"))
}

/// 返回计划任务使用的固定 EXE 路径。
pub fn installed_exe_path() -> Result<PathBuf> {
    Ok(install_root()?.join(INSTALLED_EXE_NAME))
}

/// 返回后台模式共用的文件锁路径。
pub fn lock_path() -> Result<PathBuf> {
    Ok(install_root()?.join("refresh.lock"))
}

/// 返回仅记录错误与实际刷新的后台日志路径。
pub fn log_path() -> Result<PathBuf> {
    Ok(install_root()?.join("CodexKeyboardQuota.log"))
}

/// 同时检查固定副本和计划任务是否存在。
pub fn is_deployed() -> bool {
    installed_exe_path().is_ok_and(|path| path.is_file()) && scheduled_task_exists()
}

/// 复制当前 EXE 并注册每分钟、禁止重叠的当前用户计划任务。
pub fn deploy_current_exe() -> Result<Vec<String>> {
    let source = std::env::current_exe().context("无法获取当前程序路径")?;
    deploy_executable(&source)
}

/// 从指定源 EXE 完成部署，独立入口便于用真实 Release 文件做集成验收。
fn deploy_executable(source: &Path) -> Result<Vec<String>> {
    let root = install_root()?;
    let destination = installed_exe_path()?;
    fs::create_dir_all(&root).context("无法创建部署目录")?;

    unregister_task_if_present().context("无法替换现有刷新任务")?;
    if !same_path(source, &destination) {
        let temporary = root.join("CodexKeyboardQuotaTool.new.exe");
        if temporary.exists() {
            fs::remove_file(&temporary).context("无法清理部署临时文件")?;
        }
        fs::copy(source, &temporary).with_context(|| {
            format!(
                "无法复制程序：{} -> {}",
                source.display(),
                temporary.display()
            )
        })?;
        if destination.exists() {
            fs::remove_file(&destination).context("无法替换旧部署副本")?;
        }
        fs::rename(&temporary, &destination).context("无法启用新部署副本")?;
    }

    register_task(&destination)?;
    Ok(vec![
        format!("已部署：{}", destination.display()),
        format!("已注册计划任务：{TASK_NAME}（每 1 分钟）"),
        "任务禁止重叠，单次运行最长 45 秒".to_owned(),
    ])
}

/// 删除计划任务、注册表缓存、部署副本与日志；部署副本自身延迟自删。
pub fn remove_deployment() -> Result<RemovalResult> {
    unregister_task_if_present().context("无法移除刷新任务")?;
    quota::remove_registry_cache()?;

    let root = install_root()?;
    let current = std::env::current_exe().context("无法获取当前程序路径")?;
    let running_from_install = path_is_within(&current, &root);
    let mut details = vec![
        format!("已移除计划任务：{TASK_NAME}"),
        "已删除注册表额度缓存".to_owned(),
    ];

    if root.exists() {
        if running_from_install {
            spawn_self_cleanup(std::process::id(), &root)?;
            details.push("部署副本与日志将在本窗口退出后删除".to_owned());
        } else {
            fs::remove_dir_all(&root).context("无法删除部署目录与日志")?;
            details.push(format!("已删除部署目录：{}", root.display()));
        }
    } else {
        details.push("部署目录不存在，无需删除".to_owned());
    }

    Ok(RemovalResult {
        exit_after_remove: running_from_install,
        details,
    })
}

/// 使用 ScheduledTasks 模块注册隐藏、当前用户、每分钟触发的任务。
fn register_task(executable: &Path) -> Result<()> {
    let executable_text = quote_xml(&executable.display().to_string());
    let working_directory = quote_xml(
        &executable
            .parent()
            .context("部署 EXE 没有父目录")?
            .display()
            .to_string(),
    );
    let task_name = quote_powershell(TASK_NAME);
    let start_boundary = (Local::now() + Duration::minutes(1))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let task_xml = format!(
        r#"<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>每分钟检测 Codex 额度，达到阈值时刷新 DP-104</Description></RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Repetition><Interval>PT1M</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition>
      <StartBoundary>{start_boundary}</StartBoundary><Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author"><UserId>__CURRENT_USER_SID__</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>true</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle><WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT45S</ExecutionTimeLimit><Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{executable_text}</Command><Arguments>--refresh-once</Arguments><WorkingDirectory>{working_directory}</WorkingDirectory></Exec>
  </Actions>
</Task>"#
    );
    let xml_base64 = STANDARD.encode(task_xml.as_bytes());
    let script = format!(
        "$ErrorActionPreference='Stop';\
         $xml=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{xml_base64}'));\
         $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value;\
         $xml=$xml.Replace('__CURRENT_USER_SID__',$sid);\
         Register-ScheduledTask -TaskName '{task_name}' -Xml $xml -Force | Out-Null"
    );
    run_powershell(&script).context("计划任务注册失败")?;
    if !scheduled_task_exists() {
        bail!("计划任务注册命令成功，但未查询到任务 {TASK_NAME}");
    }
    Ok(())
}

/// 停止并删除同名任务，因而可直接替换旧 Python 任务。
fn unregister_task_if_present() -> Result<()> {
    if !scheduled_task_exists() {
        return Ok(());
    }
    let _ = run_schtasks(&["/End", "/TN", TASK_NAME]);
    run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]).map(|_| ())
}

/// 通过 `schtasks /Query` 只读取同名计划任务是否存在。
fn scheduled_task_exists() -> bool {
    let mut command = Command::new("schtasks.exe");
    command
        .args(["/Query", "/TN", TASK_NAME])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_hidden(&mut command, false);
    command.status().is_ok_and(|status| status.success())
}

/// 隐藏运行系统任务计划 CLI，并在失败时返回其诊断文本。
fn run_schtasks(arguments: &[&str]) -> Result<String> {
    let mut command = Command::new("schtasks.exe");
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_hidden(&mut command, false);
    let output = command.output().context("无法启动 schtasks.exe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "schtasks.exe 退出码 {}：{}",
            output.status.code().unwrap_or(-1),
            if detail.is_empty() {
                "未提供错误信息"
            } else {
                &detail
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// 编码并隐藏执行 PowerShell 脚本，失败时带回标准错误。
fn run_powershell(script: &str) -> Result<String> {
    let encoded = encode_powershell(script);
    let mut command = Command::new(powershell_path());
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_hidden(&mut command, false);
    let output = command.output().context("无法启动 PowerShell")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "PowerShell 退出码 {}：{}",
            output.status.code().unwrap_or(-1),
            if stderr.is_empty() {
                "未提供错误信息"
            } else {
                &stderr
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// 启动一个与当前进程分离的隐藏清理器，等待 PID 退出后删除部署目录。
fn spawn_self_cleanup(process_id: u32, root: &Path) -> Result<()> {
    let root = quote_powershell(&root.display().to_string());
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue';\
         Wait-Process -Id {process_id} -Timeout 30;\
         for($i=0;$i -lt 20;$i++){{\
             Remove-Item -LiteralPath '{root}' -Recurse -Force;\
             if(-not (Test-Path -LiteralPath '{root}')){{break}};\
             Start-Sleep -Milliseconds 500\
         }}"
    );
    let encoded = encode_powershell(&script);
    let mut command = Command::new(powershell_path());
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_hidden(&mut command, true);
    command.spawn().context("无法启动部署副本自删除程序")?;
    Ok(())
}

/// 将 PowerShell 源码编码成 `-EncodedCommand` 要求的 UTF-16LE Base64。
fn encode_powershell(script: &str) -> String {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    STANDARD.encode(bytes)
}

/// 转义 PowerShell 单引号字符串中的单引号。
fn quote_powershell(text: &str) -> String {
    text.replace(char::from(39), "''")
}

/// 转义任务 XML 元素文本中的保留字符。
fn quote_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace(char::from(39), "&apos;")
}

/// 比较两个路径是否指向同一文件，目标不存在时退回绝对文本比较。
fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left.as_os_str().eq_ignore_ascii_case(right.as_os_str()),
    }
}

/// 判断当前 EXE 是否位于部署目录内。
fn path_is_within(path: &Path, directory: &Path) -> bool {
    match (path.canonicalize(), directory.canonicalize()) {
        (Ok(path), Ok(directory)) => path.starts_with(directory),
        _ => false,
    }
}

/// 优先使用 PowerShell 7，缺失时回退系统 Windows PowerShell。
fn powershell_path() -> PathBuf {
    let pwsh = PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe");
    if pwsh.is_file() {
        pwsh
    } else {
        PathBuf::from("powershell.exe")
    }
}

/// 为子进程设置无控制台窗口标志，自删除器额外脱离父进程。
fn configure_hidden(command: &mut Command, detached: bool) {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW | if detached { DETACHED_PROCESS } else { 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 PowerShell 路径字符串中的单引号不会截断脚本。
    #[test]
    fn escapes_powershell_single_quotes() {
        assert_eq!(quote_powershell("C:\\A'B\\tool.exe"), "C:\\A''B\\tool.exe");
    }

    /// 验证 EncodedCommand 输入确实按 UTF-16LE 编码。
    #[test]
    fn encodes_powershell_as_utf16le() {
        assert_eq!(encode_powershell("A中"), "QQAtTg==");
    }

    /// 验证路径中的 XML 保留字符会被安全编码。
    #[test]
    fn escapes_task_xml_text() {
        assert_eq!(
            quote_xml("C:\\A&B<'\"\\tool.exe"),
            "C:\\A&amp;B&lt;&apos;&quot;\\tool.exe"
        );
    }

    /// 当前机器验收：部署 Release、运行后台模式、彻底移除，再恢复 Rust 部署。
    #[test]
    #[ignore = "会替换当前用户的 Codex DP-104 Quota 计划任务"]
    fn deployment_remove_round_trip_and_restore() {
        let release_exe = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("release")
            .join(INSTALLED_EXE_NAME);
        assert!(release_exe.is_file(), "请先执行 cargo build --release");

        deploy_executable(&release_exe).expect("部署 Release 失败");
        assert!(is_deployed());
        let installed = installed_exe_path().unwrap();
        let mut refresh = Command::new(&installed);
        refresh
            .arg("--refresh-once")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_hidden(&mut refresh, false);
        assert!(refresh.status().expect("无法运行部署副本").success());

        let removal = remove_deployment().expect("彻底移除失败");
        assert!(!removal.exit_after_remove);
        assert!(!scheduled_task_exists());
        assert!(!install_root().unwrap().exists());
        assert!(quota::read_recorded_status().unwrap().is_none());

        deploy_executable(&release_exe).expect("恢复 Rust 部署失败");
        assert!(is_deployed());
        let installed = installed_exe_path().unwrap();
        let mut refresh = Command::new(&installed);
        refresh
            .arg("--refresh-once")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_hidden(&mut refresh, false);
        assert!(refresh.status().expect("恢复后刷新失败").success());
    }
}

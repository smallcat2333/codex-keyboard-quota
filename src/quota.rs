//! 额度状态、阈值判断、显示格式和注册表缓存。

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local};
use std::io::ErrorKind;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

pub const FIVE_HOUR_WINDOW_MINUTES: u64 = 5 * 60;
pub const SEVEN_DAY_WINDOW_MINUTES: u64 = 7 * 24 * 60;
pub const WEEK_RESET_CELL_COUNT: u8 = 10;
const FIVE_HOUR_REFRESH_THRESHOLD: u8 = 5;
const SEVEN_DAY_REFRESH_THRESHOLD: u8 = 2;
const LOW_QUOTA_FORCE_REFRESH_THRESHOLD: u8 = 5;
const REGISTRY_PATH: &str = r"Software\Smallcat\CodexKeyboardQuota";
const REGISTRY_FIVE_HOUR_VALUE: &str = "FiveHourRemaining";
const REGISTRY_SEVEN_DAY_VALUE: &str = "SevenDayRemaining";
const REGISTRY_WEEK_RESET_CELLS_VALUE: &str = "SevenDayResetCells";
const REGISTRY_UNLIMITED_VALUE: &str = "--";

/// 一次 Codex 限额查询的完整结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaStatus {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
    pub seven_day_resets_at: Option<i64>,
    pub reset_cells: Option<u8>,
}

/// 上次已经成功写入键盘的注册表状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedStatus {
    pub five_hour: Option<u8>,
    pub seven_day: Option<u8>,
    pub reset_cells: Option<u8>,
}

impl QuotaStatus {
    /// 生成固定为“5h|周%”的可读文本。
    pub fn display_message(&self) -> String {
        format!(
            "{}|{}%",
            format_display_percent(self.five_hour),
            format_display_percent(self.seven_day)
        )
    }

    /// 生成适合写入注册表的已显示状态。
    fn recorded(&self) -> RecordedStatus {
        RecordedStatus {
            five_hour: self.five_hour,
            seven_day: self.seven_day,
            reset_cells: self.reset_cells,
        }
    }

    /// 把周重置时间格式化为当前系统时区；无周窗口时显示无限制。
    pub fn reset_time_text(&self) -> String {
        let Some(timestamp) = self.seven_day_resets_at else {
            return "--（无周窗口）".to_owned();
        };
        DateTime::from_timestamp(timestamp, 0)
            .map(|time| {
                time.with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "时间格式错误".to_owned())
    }
}

/// 把周窗口剩余时间映射为 0 到 10 个沙漏格，采用与 Python 版一致的四舍五入。
pub fn calculate_week_reset_cells(resets_at: Option<i64>, now: i64) -> Option<u8> {
    let resets_at = resets_at?;
    let remaining_seconds = resets_at.saturating_sub(now).max(0) as f64;
    let full_window_seconds = (SEVEN_DAY_WINDOW_MINUTES * 60) as f64;
    let cells =
        (remaining_seconds / (full_window_seconds / WEEK_RESET_CELL_COUNT as f64) + 0.5) as u8;
    Some(cells.min(WEEK_RESET_CELL_COUNT))
}

/// 将额度固定格式化成两字符；无限额为 `--`，100% 以 `99` 显示。
pub fn format_display_percent(percent: Option<u8>) -> String {
    match percent {
        None => REGISTRY_UNLIMITED_VALUE.to_owned(),
        Some(value) => format!("{:02}", value.min(99)),
    }
}

/// 判断当前额度是否达到写键盘阈值；沙漏格变化不会单独触发刷新。
pub fn should_refresh(current: &QuotaStatus, recorded: Option<&RecordedStatus>) -> bool {
    let Some(recorded) = recorded else {
        return true;
    };

    if low_quota_changed(current.five_hour, recorded.five_hour)
        || low_quota_changed(current.seven_day, recorded.seven_day)
    {
        return true;
    }

    quota_changed(
        current.five_hour,
        recorded.five_hour,
        FIVE_HOUR_REFRESH_THRESHOLD,
    ) || quota_changed(
        current.seven_day,
        recorded.seven_day,
        SEVEN_DAY_REFRESH_THRESHOLD,
    )
}

/// 判断低于 5% 的当前额度是否相较已显示值发生任意变化。
fn low_quota_changed(current: Option<u8>, recorded: Option<u8>) -> bool {
    matches!(current, Some(value) if value < LOW_QUOTA_FORCE_REFRESH_THRESHOLD)
        && current != recorded
}

/// 判断单个窗口是否跨越阈值，有限与无限状态切换总是刷新。
fn quota_changed(current: Option<u8>, recorded: Option<u8>, threshold: u8) -> bool {
    match (current, recorded) {
        (Some(current), Some(recorded)) => current.abs_diff(recorded) >= threshold,
        _ => current != recorded,
    }
}

/// 读取 Python/Rust 共用的 HKCU 缓存；键或任一值缺失时视为首次运行。
pub fn read_recorded_status() -> Result<Option<RecordedStatus>> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let key = match current_user.open_subkey(REGISTRY_PATH) {
        Ok(key) => key,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法打开额度注册表缓存"),
    };

    let Some(five_hour) = read_optional_registry_number(&key, REGISTRY_FIVE_HOUR_VALUE, 100)?
    else {
        return Ok(None);
    };
    let Some(seven_day) = read_optional_registry_number(&key, REGISTRY_SEVEN_DAY_VALUE, 100)?
    else {
        return Ok(None);
    };
    let Some(reset_cells) = read_optional_registry_number(
        &key,
        REGISTRY_WEEK_RESET_CELLS_VALUE,
        WEEK_RESET_CELL_COUNT,
    )?
    else {
        return Ok(None);
    };

    Ok(Some(RecordedStatus {
        five_hour,
        seven_day,
        reset_cells,
    }))
}

/// 写入本次真正显示到键盘的状态，保持与 Python 版字符串格式兼容。
pub fn write_recorded_status(status: &QuotaStatus) -> Result<()> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = current_user
        .create_subkey(REGISTRY_PATH)
        .context("无法创建额度注册表缓存")?;
    let recorded = status.recorded();
    key.set_value(
        REGISTRY_FIVE_HOUR_VALUE,
        &registry_number_text(recorded.five_hour),
    )
    .context("无法记录 5h 额度")?;
    key.set_value(
        REGISTRY_SEVEN_DAY_VALUE,
        &registry_number_text(recorded.seven_day),
    )
    .context("无法记录周额度")?;
    key.set_value(
        REGISTRY_WEEK_RESET_CELLS_VALUE,
        &registry_number_text(recorded.reset_cells),
    )
    .context("无法记录周重置沙漏")?;
    Ok(())
}

/// 删除本工具在当前用户下的全部额度缓存。
pub fn remove_registry_cache() -> Result<()> {
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    match current_user.delete_subkey_all(REGISTRY_PATH) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("无法删除额度注册表缓存"),
    }
}

/// 将可选数值转换为注册表兼容字符串。
fn registry_number_text(value: Option<u8>) -> String {
    value
        .map(|number| number.to_string())
        .unwrap_or_else(|| REGISTRY_UNLIMITED_VALUE.to_owned())
}

/// 同时兼容旧 DWORD 和当前字符串值；外层 None 表示注册表值不存在。
fn read_optional_registry_number(
    key: &RegKey,
    name: &str,
    maximum: u8,
) -> Result<Option<Option<u8>>> {
    match key.get_value::<String, _>(name) {
        Ok(text) => return parse_registry_number(&text, name, maximum).map(Some),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => {}
    }

    match key.get_value::<u32, _>(name) {
        Ok(value) if value <= maximum as u32 => Ok(Some(Some(value as u8))),
        Ok(value) => bail!("注册表 {name} 超出允许范围：{value}"),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("无法读取注册表 {name}")),
    }
}

/// 解析注册表字符串中的数值或无限额占位符。
fn parse_registry_number(text: &str, name: &str, maximum: u8) -> Result<Option<u8>> {
    if text == REGISTRY_UNLIMITED_VALUE {
        return Ok(None);
    }
    let value = text
        .parse::<u8>()
        .map_err(|_| anyhow!("注册表 {name} 不是有效数值：{text}"))?;
    if value > maximum {
        bail!("注册表 {name} 超出允许范围：{value}");
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建用于阈值测试的额度状态。
    fn status(five_hour: Option<u8>, seven_day: Option<u8>, cells: Option<u8>) -> QuotaStatus {
        QuotaStatus {
            five_hour,
            seven_day,
            seven_day_resets_at: None,
            reset_cells: cells,
        }
    }

    /// 验证固定两位、无限额与 100% 显示规则。
    #[test]
    fn formats_percent_for_keyboard() {
        assert_eq!(format_display_percent(None), "--");
        assert_eq!(format_display_percent(Some(5)), "05");
        assert_eq!(format_display_percent(Some(82)), "82");
        assert_eq!(format_display_percent(Some(100)), "99");
    }

    /// 验证两个阈值的等号边界都会触发刷新。
    #[test]
    fn refreshes_at_equal_thresholds() {
        let recorded = RecordedStatus {
            five_hour: Some(50),
            seven_day: Some(50),
            reset_cells: Some(10),
        };
        assert!(should_refresh(
            &status(Some(45), Some(50), Some(9)),
            Some(&recorded)
        ));
        assert!(should_refresh(
            &status(Some(50), Some(48), Some(9)),
            Some(&recorded)
        ));
        assert!(!should_refresh(
            &status(Some(46), Some(49), Some(0)),
            Some(&recorded)
        ));
    }

    /// 验证低于 5% 后任意额度变化会立即刷新，等于 5% 仍遵循常规阈值。
    #[test]
    fn refreshes_every_low_quota_change() {
        let recorded = RecordedStatus {
            five_hour: Some(4),
            seven_day: Some(4),
            reset_cells: Some(1),
        };
        assert!(should_refresh(
            &status(Some(3), Some(4), Some(1)),
            Some(&recorded)
        ));
        assert!(should_refresh(
            &status(Some(4), Some(3), Some(1)),
            Some(&recorded)
        ));

        let five_percent = RecordedStatus {
            five_hour: Some(6),
            seven_day: Some(6),
            reset_cells: Some(1),
        };
        assert!(!should_refresh(
            &status(Some(5), Some(5), Some(1)),
            Some(&five_percent)
        ));
    }

    /// 验证沙漏变化本身不会触发刷新。
    #[test]
    fn ignores_hourglass_only_change() {
        let recorded = RecordedStatus {
            five_hour: Some(40),
            seven_day: Some(29),
            reset_cells: Some(10),
        };
        assert!(!should_refresh(
            &status(Some(40), Some(29), Some(1)),
            Some(&recorded)
        ));
    }

    /// 验证有限与无限窗口切换始终触发刷新。
    #[test]
    fn refreshes_when_window_appears_or_disappears() {
        let recorded = RecordedStatus {
            five_hour: None,
            seven_day: Some(82),
            reset_cells: Some(7),
        };
        assert!(should_refresh(
            &status(Some(99), Some(82), Some(7)),
            Some(&recorded)
        ));
        assert!(!should_refresh(
            &status(None, Some(82), Some(2)),
            Some(&recorded)
        ));
    }

    /// 验证完整周、半周和已到期时间映射到正确沙漏格数。
    #[test]
    fn maps_week_reset_to_ten_cells() {
        let now = 1_000_000;
        let week = (SEVEN_DAY_WINDOW_MINUTES * 60) as i64;
        assert_eq!(calculate_week_reset_cells(Some(now + week), now), Some(10));
        assert_eq!(
            calculate_week_reset_cells(Some(now + week / 2), now),
            Some(5)
        );
        assert_eq!(calculate_week_reset_cells(Some(now - 1), now), Some(0));
        assert_eq!(calculate_week_reset_cells(None, now), None);
    }
}

//! DP-104 HID 通信与 8×24 静态彩色点阵渲染。

use crate::consumption::{ConsumptionChart, consumption_level};
use crate::quota::{QuotaStatus, WEEK_RESET_CELL_COUNT, format_display_percent};
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};

pub const VENDOR_ID: u16 = 0xE560;
pub const PRODUCT_ID: u16 = 0xE104;
const VENDOR_USAGE_PAGE: u16 = 0xFF60;
const VENDOR_USAGE: u16 = 0x61;
pub const MATRIX_ROWS: usize = 8;
pub const MATRIX_COLS: usize = 24;
const MATRIX_FRAME_COUNT: u8 = 1;
const MATRIX_FPS: u8 = 1;
const MATRIX_DATA_CHUNK_LENGTH: usize = 25;
const HID_REPORT_LENGTH: usize = 33;
const COMMAND_CUSTOM_ANIMATION: u8 = 209;

pub const HSV_RED: [u8; 3] = [0, 255, 255];
pub const HSV_YELLOW: [u8; 3] = [43, 255, 255];
pub const HSV_GREEN: [u8; 3] = [85, 255, 255];
pub const HSV_WHITE: [u8; 3] = [0, 0, 255];
pub const HSV_PINK_PURPLE: [u8; 3] = [213, 200, 255];
pub const HSV_BLUE: [u8; 3] = [170, 255, 255];
pub const HSV_ORANGE: [u8; 3] = [21, 255, 255];

const GLYPH_0: [&str; 5] = ["111", "101", "101", "101", "111"];
const GLYPH_1: [&str; 5] = ["010", "110", "010", "010", "111"];
const GLYPH_2: [&str; 5] = ["110", "001", "010", "100", "111"];
const GLYPH_3: [&str; 5] = ["110", "001", "010", "001", "110"];
const GLYPH_4: [&str; 5] = ["101", "101", "111", "001", "001"];
const GLYPH_5: [&str; 5] = ["111", "100", "110", "001", "110"];
const GLYPH_6: [&str; 5] = ["110", "100", "111", "101", "111"];
const GLYPH_7: [&str; 5] = ["111", "001", "010", "010", "010"];
const GLYPH_8: [&str; 5] = ["111", "101", "111", "101", "111"];
const GLYPH_9: [&str; 5] = ["111", "101", "111", "001", "110"];
const GLYPH_DASH: [&str; 5] = ["000", "000", "111", "000", "000"];
const GLYPH_SEPARATOR: [&str; 5] = ["1", "1", "1", "1", "1"];

/// 一个带颜色及右侧间距的 3×5 或 1×5 字形。
struct ColoredGlyph {
    rows: &'static [&'static str; 5],
    color: [u8; 3],
    spacing: usize,
}

/// 已打开的 DP-104 厂商 HID 接口。
pub struct Dp104Keyboard {
    device: HidDevice,
}

impl Dp104Keyboard {
    /// 枚举并打开 usage page 0xFF60 / usage 0x61 的 DP-104 接口。
    pub fn open() -> Result<Self> {
        let api = HidApi::new().context("无法初始化 Windows HID")?;
        let info = api
            .device_list()
            .find(|device| {
                device.vendor_id() == VENDOR_ID
                    && device.product_id() == PRODUCT_ID
                    && device.usage_page() == VENDOR_USAGE_PAGE
                    && device.usage() == VENDOR_USAGE
            })
            .context("未找到 TICKTYPE DP-104，请连接键盘并关闭网页驱动")?;
        let device = info.open_device(&api).context("无法打开 DP-104 HID 接口")?;
        Ok(Self { device })
    }

    /// 快速枚举目标接口，不占用键盘句柄。
    pub fn is_connected() -> Result<bool> {
        let api = HidApi::new().context("无法初始化 Windows HID")?;
        Ok(api.device_list().any(|device| {
            device.vendor_id() == VENDOR_ID
                && device.product_id() == PRODUCT_ID
                && device.usage_page() == VENDOR_USAGE_PAGE
                && device.usage() == VENDOR_USAGE
        }))
    }

    /// 重置为单帧并按 25 字节分块写入完整 HSV 点阵。
    pub fn write_custom_frame(&self, frame: &[u8]) -> Result<()> {
        let expected_length = MATRIX_ROWS * MATRIX_COLS * 3;
        if frame.len() != expected_length {
            bail!("自定义帧长度必须为 {expected_length} 字节");
        }
        self.send_command(
            COMMAND_CUSTOM_ANIMATION,
            &[
                48,
                MATRIX_FRAME_COUNT,
                MATRIX_FPS,
                MATRIX_ROWS as u8,
                MATRIX_COLS as u8,
            ],
        )?;
        for (chunk_index, chunk) in frame.chunks(MATRIX_DATA_CHUNK_LENGTH).enumerate() {
            let offset = chunk_index * MATRIX_DATA_CHUNK_LENGTH;
            let mut arguments = Vec::with_capacity(6 + chunk.len());
            arguments.push(49);
            arguments.extend_from_slice(&(offset as u32).to_be_bytes());
            arguments.push(chunk.len() as u8);
            arguments.extend_from_slice(chunk);
            self.send_command(COMMAND_CUSTOM_ANIMATION, &arguments)?;
        }
        Ok(())
    }

    /// 发送固定 33 字节 VIA 报文并严格校验命令回显。
    fn send_command(&self, command: u8, arguments: &[u8]) -> Result<()> {
        let mut report = [0_u8; HID_REPORT_LENGTH];
        let logical_length = 2 + arguments.len();
        if logical_length > report.len() {
            bail!("HID 报文超过 DP-104 的 32 字节数据区");
        }
        report[1] = command;
        report[2..logical_length].copy_from_slice(arguments);
        let written = self.device.write(&report).context("DP-104 HID 写入失败")?;
        if written != HID_REPORT_LENGTH {
            bail!("DP-104 HID 写入长度异常：{written}");
        }

        let mut response_buffer = [0_u8; 64];
        let response_length = self
            .device
            .read_timeout(&mut response_buffer, 1_000)
            .context("DP-104 HID 回显读取失败")?;
        let mut response = &response_buffer[..response_length];
        if response.first() == Some(&0) {
            response = &response[1..];
        }
        let expected = &report[1..logical_length];
        if response.get(..expected.len()) != Some(expected) {
            bail!("DP-104 HID 响应校验失败，请关闭网页驱动后重试");
        }
        Ok(())
    }
}

/// 根据剩余额度选择数字 HSV 颜色；无限额占位符使用绿色。
pub fn quota_color(remaining_percent: Option<u8>) -> [u8; 3] {
    match remaining_percent {
        None => HSV_GREEN,
        Some(value) if value >= 60 => HSV_GREEN,
        Some(value) if value >= 20 => HSV_YELLOW,
        Some(_) => HSV_RED,
    }
}

/// 渲染固定布局：`5h | 周 | 2×5 沙漏`，输出 8×24×HSV 字节。
pub fn build_static_frame(status: &QuotaStatus) -> Result<Vec<u8>> {
    if let Some(relay) = &status.relay {
        return build_balance_frame(&relay.display_text(), &relay.chart);
    }
    if status
        .reset_cells
        .is_some_and(|cells| cells > WEEK_RESET_CELL_COUNT)
    {
        bail!("周重置倒计时格数必须在 0 到 10 之间");
    }

    let segments = [
        (
            format_display_percent(status.five_hour),
            quota_color(status.five_hour),
        ),
        ("|".to_owned(), HSV_WHITE),
        (
            format_display_percent(status.seven_day),
            quota_color(status.seven_day),
        ),
        ("|".to_owned(), HSV_WHITE),
    ];
    let mut glyphs = Vec::new();
    for (text, color) in segments {
        let character_count = text.chars().count();
        for (index, character) in text.chars().enumerate() {
            glyphs.push(ColoredGlyph {
                rows: glyph(character)?,
                color,
                spacing: if index + 1 < character_count { 2 } else { 1 },
            });
        }
    }

    let text_width = glyphs
        .iter()
        .enumerate()
        .map(|(index, glyph)| {
            glyph.rows[0].len()
                + if index + 1 < glyphs.len() {
                    glyph.spacing
                } else {
                    0
                }
        })
        .sum::<usize>();
    let layout_width = text_width + 1 + 2;
    if layout_width > MATRIX_COLS {
        bail!("额度与倒计时超过 DP-104 静态点阵宽度");
    }

    let mut frame = vec![0_u8; MATRIX_ROWS * MATRIX_COLS * 3];
    let start_x = (MATRIX_COLS - layout_width) / 2;
    let start_y = (MATRIX_ROWS - 5) / 2;
    let mut current_x = start_x;
    for glyph in glyphs {
        for (glyph_y, row) in glyph.rows.iter().enumerate() {
            for (glyph_x, pixel) in row.bytes().enumerate() {
                if pixel == b'1' {
                    set_pixel(
                        &mut frame,
                        current_x + glyph_x,
                        start_y + glyph_y,
                        glyph.color,
                    );
                }
            }
        }
        current_x += glyph.rows[0].len() + glyph.spacing;
    }

    let lit_cells = status.reset_cells.unwrap_or(0);
    let disappeared_cells = WEEK_RESET_CELL_COUNT - lit_cells;
    let reset_start_x = start_x + text_width + 1;
    for index in disappeared_cells..WEEK_RESET_CELL_COUNT {
        let x = reset_start_x + (index % 2) as usize;
        let y = start_y + (index / 2) as usize;
        set_pixel(&mut frame, x, y, HSV_PINK_PURPLE);
    }
    Ok(frame)
}

/// 三位余额右对齐占 11 列，12 列画竖线，14–23 列画十根从底部生长的五点柱。
fn build_balance_frame(text: &str, chart: &ConsumptionChart) -> Result<Vec<u8>> {
    let glyphs = text.chars().map(glyph).collect::<Result<Vec<_>>>()?;
    let width =
        glyphs.iter().map(|rows| rows[0].len()).sum::<usize>() + glyphs.len().saturating_sub(1);
    if width > 11 {
        bail!("余额超过三位数字宽度")
    }
    let mut frame = vec![0; MATRIX_ROWS * MATRIX_COLS * 3];
    let mut x = 11 - width;
    for rows in glyphs {
        for (y, row) in rows.iter().enumerate() {
            for (offset, pixel) in row.bytes().enumerate() {
                if pixel == b'1' {
                    set_pixel(&mut frame, x + offset, y + 1, HSV_GREEN);
                }
            }
        }
        x += rows[0].len() + 1;
    }
    for y in 1..=5 {
        set_pixel(&mut frame, 12, y, HSV_WHITE);
    }
    for (index, amount) in chart.bars.iter().enumerate() {
        let Some(amount) = amount else { continue };
        let level = consumption_level(*amount);
        let color = match level {
            0..=2 => HSV_GREEN,
            3 => HSV_BLUE,
            4 => HSV_ORANGE,
            _ => HSV_RED,
        };
        for point in 0..level {
            set_pixel(&mut frame, 14 + index, 5 - point, color);
        }
    }
    Ok(frame)
}

/// 返回受支持字符的 3×5 或 1×5 字模。
fn glyph(character: char) -> Result<&'static [&'static str; 5]> {
    match character {
        '0' => Ok(&GLYPH_0),
        '1' => Ok(&GLYPH_1),
        '2' => Ok(&GLYPH_2),
        '3' => Ok(&GLYPH_3),
        '4' => Ok(&GLYPH_4),
        '5' => Ok(&GLYPH_5),
        '6' => Ok(&GLYPH_6),
        '7' => Ok(&GLYPH_7),
        '8' => Ok(&GLYPH_8),
        '9' => Ok(&GLYPH_9),
        '-' => Ok(&GLYPH_DASH),
        '|' => Ok(&GLYPH_SEPARATOR),
        _ => bail!("点阵字库不支持字符：{character}"),
    }
}

/// 将一个 HSV 像素写入按行排列的帧缓冲区。
fn set_pixel(frame: &mut [u8], x: usize, y: usize, color: [u8; 3]) {
    let offset = (y * MATRIX_COLS + x) * 3;
    frame[offset..offset + 3].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证三位数字间距、竖线、十根柱的位置及各档颜色，最新柱位于最右侧。
    #[test]
    fn renders_balance_and_ten_consumption_bars() {
        let chart = ConsumptionChart {
            end_at: 1800,
            bars: [
                None,
                None,
                None,
                None,
                Some(0),
                Some(1_000_000),
                Some(2_000_000),
                Some(3_000_000),
                Some(4_000_000),
                Some(5_000_000),
            ],
        };
        let frame = build_balance_frame("999", &chart).unwrap();
        assert_eq!(frame.len(), MATRIX_ROWS * MATRIX_COLS * 3);
        for y in 1..=5 {
            for x in [3, 7, 11, 13, 18] {
                assert_eq!(pixel(&frame, x, y), [0, 0, 0]);
            }
            assert_eq!(pixel(&frame, 12, y), HSV_WHITE);
            assert_eq!(pixel(&frame, 23, y), HSV_RED);
        }
        for (x, level, color) in [
            (19, 1, HSV_GREEN),
            (20, 2, HSV_GREEN),
            (21, 3, HSV_BLUE),
            (22, 4, HSV_ORANGE),
        ] {
            for y in 1..=5 {
                assert_eq!(
                    pixel(&frame, x, y),
                    if y > 5 - level { color } else { [0, 0, 0] }
                );
            }
        }
        for text in ["0", "22", "148", "--"] {
            assert!(build_balance_frame(text, &ConsumptionChart::default()).is_ok());
        }
    }

    /// 构造点阵测试状态。
    fn status(five_hour: Option<u8>, seven_day: Option<u8>, cells: Option<u8>) -> QuotaStatus {
        QuotaStatus {
            relay: None,
            five_hour,
            seven_day,
            seven_day_resets_at: None,
            reset_cells: cells,
        }
    }

    /// 读取指定位置的 HSV 像素。
    fn pixel(frame: &[u8], x: usize, y: usize) -> [u8; 3] {
        let offset = (y * MATRIX_COLS + x) * 3;
        [frame[offset], frame[offset + 1], frame[offset + 2]]
    }

    /// 验证 60、20 两个颜色等号边界和无限额颜色。
    #[test]
    fn selects_quota_colors_at_boundaries() {
        assert_eq!(quota_color(None), HSV_GREEN);
        assert_eq!(quota_color(Some(60)), HSV_GREEN);
        assert_eq!(quota_color(Some(59)), HSV_YELLOW);
        assert_eq!(quota_color(Some(20)), HSV_YELLOW);
        assert_eq!(quota_color(Some(19)), HSV_RED);
    }

    /// 验证 24 列布局中的两道固定白色竖线。
    #[test]
    fn places_separators_in_columns_nine_and_twenty() {
        let frame = build_static_frame(&status(Some(44), Some(30), Some(0))).unwrap();
        assert_eq!(frame.len(), MATRIX_ROWS * MATRIX_COLS * 3);
        for y in 1..=5 {
            assert_eq!(pixel(&frame, 9, y), HSV_WHITE);
            assert_eq!(pixel(&frame, 20, y), HSV_WHITE);
        }
        for x in 0..MATRIX_COLS {
            assert_eq!(pixel(&frame, x, 0), [0, 0, 0]);
            assert_eq!(pixel(&frame, x, 7), [0, 0, 0]);
        }
    }

    /// 验证左右额度不会合并，并分别使用各自颜色。
    #[test]
    fn renders_two_fixed_width_quota_pairs() {
        let frame = build_static_frame(&status(None, Some(82), Some(0))).unwrap();
        assert_eq!(pixel(&frame, 0, 3), HSV_GREEN);
        assert_eq!(pixel(&frame, 5, 3), HSV_GREEN);
        assert_eq!(pixel(&frame, 11, 1), HSV_GREEN);
        assert_eq!(pixel(&frame, 16, 1), HSV_GREEN);
    }

    /// 验证沙漏从左到右、从上到下消失且只占第 22、23 列。
    #[test]
    fn maps_hourglass_to_last_two_columns() {
        let full = build_static_frame(&status(Some(50), Some(50), Some(10))).unwrap();
        for y in 1..=5 {
            assert_eq!(pixel(&full, 22, y), HSV_PINK_PURPLE);
            assert_eq!(pixel(&full, 23, y), HSV_PINK_PURPLE);
        }

        let nine = build_static_frame(&status(Some(50), Some(50), Some(9))).unwrap();
        assert_eq!(pixel(&nine, 22, 1), [0, 0, 0]);
        assert_eq!(pixel(&nine, 23, 1), HSV_PINK_PURPLE);

        let empty = build_static_frame(&status(Some(50), Some(50), Some(0))).unwrap();
        for y in 1..=5 {
            assert_eq!(pixel(&empty, 22, y), [0, 0, 0]);
            assert_eq!(pixel(&empty, 23, y), [0, 0, 0]);
        }
    }
}

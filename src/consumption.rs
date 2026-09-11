//! 每分钟余额采样，累计半小时消耗并保留最近十根柱；充值不抵扣已经记录的消耗。

use serde::{Deserialize, Serialize};

pub const BAR_COUNT: usize = 10;
pub const WINDOW_SECONDS: i64 = 30 * 60;
const MAX_SAMPLE_GAP_SECONDS: i64 = 120;

/// 已完成的十个半小时周期，从左到右由旧到新；None 表示尚无数据或采样中断。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumptionChart {
    pub end_at: i64,
    pub bars: [Option<u64>; BAR_COUNT],
}

/// 可跨计划任务进程恢复的当前周期和上一次真实余额，金额单位均为百万分之一。
#[derive(Debug, Serialize, Deserialize)]
pub struct ConsumptionHistory {
    chart: ConsumptionChart,
    last_checked_at: i64,
    last_balance: Option<i64>,
    consumed: u64,
    complete: bool,
}

impl ConsumptionHistory {
    /// 首次查询作为 30 分钟周期起点；没有有效余额时不产生有效消耗柱。
    pub fn new(now: i64, balance: Option<i64>) -> Self {
        Self {
            chart: ConsumptionChart {
                end_at: now,
                ..Default::default()
            },
            last_checked_at: now,
            last_balance: balance,
            consumed: 0,
            complete: balance.is_some(),
        }
    }

    /// 累计相邻有效采样的余额下降；周期闭合才推移柱图，长时间停机留空而不伪造消耗。
    pub fn sample(&mut self, now: i64, balance: Option<i64>) {
        if now < self.last_checked_at {
            *self = Self::new(now, balance);
            return;
        }
        let contiguous = now - self.last_checked_at <= MAX_SAMPLE_GAP_SECONDS
            && self.last_balance.is_some()
            && balance.is_some();
        let elapsed_windows = (now - self.chart.end_at) / WINDOW_SECONDS;
        if contiguous {
            // 同一分钟的跨周期余额差计入刚结束的周期，避免丢失边界采样。
            let decrease = self
                .last_balance
                .unwrap()
                .saturating_sub(balance.unwrap())
                .max(0) as u64;
            self.consumed = self.consumed.saturating_add(decrease);
        } else {
            self.complete = false;
        }
        if elapsed_windows > 0 {
            let completed = self.complete.then_some(self.consumed);
            let shift = elapsed_windows.min(BAR_COUNT as i64) as usize;
            self.chart.bars.rotate_left(shift);
            self.chart.bars[BAR_COUNT - shift..].fill(None);
            if elapsed_windows <= BAR_COUNT as i64 {
                self.chart.bars[BAR_COUNT - shift] = completed;
            }
            self.chart.end_at += elapsed_windows * WINDOW_SECONDS;
            self.consumed = 0;
            self.complete = contiguous && elapsed_windows == 1;
        }
        self.last_checked_at = now;
        self.last_balance = balance;
    }

    /// 仅暴露已完成周期，未满半小时的累计值不会改变键盘柱图。
    pub fn chart(&self) -> ConsumptionChart {
        self.chart.clone()
    }
}

/// 按半小时累计消耗映射 0–5 个点；阈值等号属于下一档。
pub fn consumption_level(amount: u64) -> usize {
    match amount {
        0..100_000 => 0,
        100_000..1_000_000 => 1,
        1_000_000..2_000_000 => 2,
        2_000_000..5_000_000 => 3,
        5_000_000..10_000_000 => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证所有档位边界，不对低于 0.1 的消耗点亮柱子。
    #[test]
    fn levels_follow_exact_thresholds() {
        for (amount, expected) in [
            (0, 0),
            (99_999, 0),
            (100_000, 1),
            (999_999, 1),
            (1_000_000, 2),
            (1_999_999, 2),
            (2_000_000, 3),
            (4_999_999, 3),
            (5_000_000, 4),
            (9_999_999, 4),
            (10_000_000, 5),
        ] {
            assert_eq!(consumption_level(amount), expected);
        }
    }

    /// 模拟每分钟采样，半小时内不推移；持久化恢复后累计满周期只产生一根最新柱。
    #[test]
    fn accumulates_and_survives_process_restarts() {
        let mut history = ConsumptionHistory::new(1000, Some(100_000_000));
        for minute in 1..30 {
            history.sample(1000 + minute * 60, Some(100_000_000 - minute * 100_000));
        }
        assert_eq!(history.chart().bars, [None; BAR_COUNT]);
        let json = serde_json::to_string(&history).unwrap();
        let mut history: ConsumptionHistory = serde_json::from_str(&json).unwrap();
        history.sample(2800, Some(97_000_000));
        assert_eq!(history.chart().bars[9], Some(3_000_000));
        assert_eq!(history.chart().end_at, 2800);
        for minute in 1..=30 {
            history.sample(2800 + minute * 60, Some(97_000_000));
        }
        assert_eq!(&history.chart().bars[8..], &[Some(3_000_000), Some(0)]);
    }

    /// 充值后只累计下降值，避免充值抹掉已发生的消耗。
    #[test]
    fn topups_do_not_cancel_consumption() {
        let mut history = ConsumptionHistory::new(0, Some(10_000_000));
        history.sample(60, Some(9_000_000));
        history.sample(120, Some(20_000_000));
        for minute in 3..=30 {
            history.sample(minute * 60, Some(19_000_000));
        }
        assert_eq!(history.chart().bars[9], Some(2_000_000));
    }

    /// 验证停机、查询失败及恢复不会把未知时段当作零消耗或一个巨大尖峰。
    #[test]
    fn leaves_gaps_for_missing_measurements() {
        let mut history = ConsumptionHistory::new(0, Some(100_000_000));
        history.sample(60, None);
        for minute in 2..=30 {
            history.sample(minute * 60, Some(90_000_000));
        }
        assert_eq!(history.chart().bars[9], None);
        for minute in 31..=60 {
            history.sample(minute * 60, Some(90_000_000));
        }
        assert_eq!(history.chart().bars[9], Some(0));
        history.sample(3600 + 12 * WINDOW_SECONDS, Some(1_000_000));
        assert_eq!(history.chart().bars, [None; BAR_COUNT]);
    }
}

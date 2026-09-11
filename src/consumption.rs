//! 每分钟余额采样，累计半小时消耗并保留最近十根柱；充值不抵扣已经记录的消耗。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const BAR_COUNT: usize = 10;
pub const WINDOW_SECONDS: i64 = 30 * 60;
const MAX_SAMPLE_GAP_SECONDS: i64 = 120;

/// 十根半小时消耗柱，从左到右由旧到新；None 表示尚无数据或采样中断。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumptionChart {
    pub end_at: i64,
    pub bars: [Option<u64>; BAR_COUNT],
}

/// 共用消耗周期及各供应商独立余额基线；金额单位为同币种的百万分之一。
#[derive(Debug, Serialize, Deserialize)]
pub struct ConsumptionHistory {
    chart: ConsumptionChart,
    last_checked_at: i64,
    active_provider: String,
    balances: BTreeMap<String, Option<i64>>,
    consumed: u64,
    complete: bool,
}

impl ConsumptionHistory {
    /// 首次查询作为 30 分钟周期起点；没有有效余额时不产生有效消耗柱。
    pub fn new(now: i64, provider: &str, balance: Option<i64>) -> Self {
        Self {
            chart: ConsumptionChart {
                end_at: now,
                ..Default::default()
            },
            last_checked_at: now,
            active_provider: provider.to_owned(),
            balances: BTreeMap::from([(provider.to_owned(), balance)]),
            consumed: 0,
            complete: balance.is_some(),
        }
    }

    /// 仅计算同一供应商连续采样的下降值；切换重建对应基线，共用周期继续累计。
    pub fn sample(&mut self, now: i64, provider: &str, balance: Option<i64>) {
        if now < self.last_checked_at {
            *self = Self::new(now, provider, balance);
            return;
        }
        let switched = provider != self.active_provider;
        let previous = self.balances.get(provider).copied().flatten();
        let contiguous = now - self.last_checked_at <= MAX_SAMPLE_GAP_SECONDS
            && (switched || previous.is_some())
            && balance.is_some();
        let elapsed_windows = (now - self.chart.end_at) / WINDOW_SECONDS;
        if contiguous && !switched {
            // 同一分钟的跨周期余额差计入刚结束的周期，避免丢失边界采样。
            let decrease = previous.unwrap().saturating_sub(balance.unwrap()).max(0) as u64;
            self.consumed = self.consumed.saturating_add(decrease);
        } else if !contiguous {
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
        self.active_provider = provider.to_owned();
        self.balances.insert(provider.to_owned(), balance);
    }

    /// 展示最近九个已完成周期及最右侧的当前累计；调用方只在余额触发时写屏。
    pub fn chart(&self) -> ConsumptionChart {
        let mut chart = self.chart.clone();
        chart.bars.rotate_left(1);
        chart.bars[BAR_COUNT - 1] = self.complete.then_some(self.consumed);
        chart
    }
}

/// 半小时累计消耗每满 1 USD 点亮一个点，最多五点；金额单位为百万分之一。
pub fn consumption_level(amount: u64) -> usize {
    (amount / 1_000_000).min(5) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最右侧立即展示当前累计，满半小时后左移为历史，新周期从零开始。
    #[test]
    fn exposes_live_consumption_before_window_completes() {
        let mut history = ConsumptionHistory::new(0, "A", Some(100_000_000));
        history.sample(60, "A", Some(97_500_000));
        assert_eq!(history.chart().bars[9], Some(2_500_000));
        assert_eq!(history.chart().bars[8], None);
        for minute in 2..=30 {
            history.sample(minute * 60, "A", Some(97_500_000));
        }
        assert_eq!(&history.chart().bars[8..], &[Some(2_500_000), Some(0)]);
        history.sample(1860, "A", Some(96_500_000));
        assert_eq!(
            &history.chart().bars[8..],
            &[Some(2_500_000), Some(1_000_000)]
        );
    }

    /// A 消耗 2、B 消耗 1 合并为 3；切回 A 重新建基线，不计入其停用期间的余额差。
    #[test]
    fn combines_providers_without_subtracting_their_balances() {
        let mut history = ConsumptionHistory::new(0, "A", Some(100_000_000));
        history.sample(60, "A", Some(98_000_000));
        history.sample(120, "B", Some(10_000_000));
        history.sample(180, "B", Some(9_000_000));
        let json = serde_json::to_string(&history).unwrap();
        let mut history: ConsumptionHistory = serde_json::from_str(&json).unwrap();
        history.sample(240, "A", Some(50_000_000));
        for minute in 5..=30 {
            history.sample(minute * 60, "A", Some(50_000_000));
        }
        assert_eq!(history.chart.bars[9], Some(3_000_000));
        assert_eq!(history.balances["A"], Some(50_000_000));
        assert_eq!(history.balances["B"], Some(9_000_000));
        assert_eq!(history.chart().end_at, 1800);
    }

    /// 正好在周期边界切换也保留已有柱及当前累计，不重置时间轴或将整根柱置空。
    #[test]
    fn provider_switch_preserves_completed_bars_and_window_boundary() {
        let mut history = ConsumptionHistory::new(0, "A", Some(100_000_000));
        for minute in 1..30 {
            history.sample(minute * 60, "A", Some(98_000_000));
        }
        history.sample(1800, "B", Some(10_000_000));
        assert_eq!(history.chart.bars[9], Some(2_000_000));
        for minute in 31..=60 {
            history.sample(minute * 60, "B", Some(9_000_000));
        }
        assert_eq!(
            &history.chart.bars[8..],
            &[Some(2_000_000), Some(1_000_000)]
        );
        assert_eq!(history.chart().end_at, 3600);
    }

    /// 验证每满 1 USD 增加一点，恰好 5 USD 及更高消耗均封顶五点。
    #[test]
    fn levels_follow_exact_thresholds() {
        for (amount, expected) in [
            (0, 0),
            (999_999, 0),
            (1_000_000, 1),
            (1_999_999, 1),
            (2_000_000, 2),
            (2_999_999, 2),
            (3_000_000, 3),
            (3_999_999, 3),
            (4_000_000, 4),
            (4_999_999, 4),
            (5_000_000, 5),
            (u64::MAX, 5),
        ] {
            assert_eq!(consumption_level(amount), expected);
        }
    }

    /// 模拟每分钟采样，半小时内不推移；持久化恢复后累计满周期只产生一根最新柱。
    #[test]
    fn accumulates_and_survives_process_restarts() {
        let mut history = ConsumptionHistory::new(1000, "A", Some(100_000_000));
        for minute in 1..30 {
            history.sample(
                1000 + minute * 60,
                "A",
                Some(100_000_000 - minute * 100_000),
            );
        }
        assert_eq!(history.chart.bars, [None; BAR_COUNT]);
        let json = serde_json::to_string(&history).unwrap();
        let mut history: ConsumptionHistory = serde_json::from_str(&json).unwrap();
        history.sample(2800, "A", Some(97_000_000));
        assert_eq!(history.chart.bars[9], Some(3_000_000));
        assert_eq!(history.chart().end_at, 2800);
        for minute in 1..=30 {
            history.sample(2800 + minute * 60, "A", Some(97_000_000));
        }
        assert_eq!(&history.chart.bars[8..], &[Some(3_000_000), Some(0)]);
    }

    /// 充值后只累计下降值，避免充值抹掉已发生的消耗。
    #[test]
    fn topups_do_not_cancel_consumption() {
        let mut history = ConsumptionHistory::new(0, "A", Some(10_000_000));
        history.sample(60, "A", Some(9_000_000));
        history.sample(120, "A", Some(20_000_000));
        for minute in 3..=30 {
            history.sample(minute * 60, "A", Some(19_000_000));
        }
        assert_eq!(history.chart.bars[9], Some(2_000_000));
    }

    /// 验证停机、查询失败及恢复不会把未知时段当作零消耗或一个巨大尖峰。
    #[test]
    fn leaves_gaps_for_missing_measurements() {
        let mut history = ConsumptionHistory::new(0, "A", Some(100_000_000));
        history.sample(60, "A", None);
        for minute in 2..=30 {
            history.sample(minute * 60, "A", Some(90_000_000));
        }
        assert_eq!(history.chart.bars[9], None);
        for minute in 31..=60 {
            history.sample(minute * 60, "A", Some(90_000_000));
        }
        assert_eq!(history.chart.bars[9], Some(0));
        history.sample(3600 + 12 * WINDOW_SECONDS, "A", Some(1_000_000));
        assert_eq!(history.chart.bars, [None; BAR_COUNT]);
    }
}

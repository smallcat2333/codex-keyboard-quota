//! eframe/egui 中文图形界面与后台任务调度。

use crate::consumption::{BAR_COUNT, WINDOW_SECONDS};
use crate::deploy;
use crate::keyboard::{MATRIX_COLS, MATRIX_ROWS};
use crate::quota::QuotaStatus;
use crate::service::{self, DisplaySnapshot, SystemSnapshot};
use anyhow::Result;
use crossbeam_channel::{Receiver, unbounded};
use eframe::egui;
use std::fs;
use std::thread;
use std::time::Duration;

const APP_TITLE: &str = "Codex 键盘额度工具";
const CJK_FONT_NAME: &str = "windows_cjk";
const CJK_FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\simhei.ttf",
    r"C:\Windows\Fonts\Deng.ttf",
    r"C:\Windows\Fonts\msyh.ttf",
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simsun.ttc",
];

/// 用户可触发的后台操作。
#[derive(Clone, Copy, Debug)]
enum BackgroundTask {
    Probe,
    TestDisplay,
    Deploy,
    Remove,
}

impl BackgroundTask {
    /// 返回用于进度和日志的中文名称。
    fn label(self) -> &'static str {
        match self {
            Self::Probe => "检测状态",
            Self::TestDisplay => "测试显示",
            Self::Deploy => "一键部署",
            Self::Remove => "一键移除",
        }
    }
}

/// 后台操作完成后一次性返回给 GUI 的数据。
#[derive(Debug)]
struct TaskCompletion {
    snapshot: Option<SystemSnapshot>,
    details: Vec<String>,
    exit_after: bool,
}

/// GUI 工作线程事件。
#[derive(Debug)]
enum WorkerEvent {
    Finished(Result<TaskCompletion, String>),
}

/// 应用窗口状态。
struct QuotaApp {
    snapshot: Option<SystemSnapshot>,
    display: Option<DisplaySnapshot>,
    display_error: Option<String>,
    display_rx: Receiver<Result<DisplaySnapshot, String>>,
    details: String,
    busy: bool,
    current_task: Option<BackgroundTask>,
    worker_rx: Option<Receiver<WorkerEvent>>,
    close_requested: bool,
    startup_repaints: u16,
}

impl QuotaApp {
    /// 初始化主题、中文字体，并立即在后台读取一次连接状态。
    fn new(context: &eframe::CreationContext<'_>) -> Self {
        configure_visuals(&context.egui_ctx);
        let font_message = configure_fonts(&context.egui_ctx);
        let mut app = Self {
            snapshot: None,
            display: None,
            display_error: None,
            display_rx: spawn_display_monitor(context.egui_ctx.clone()),
            details: format!(
                "Codex 键盘额度工具 V{} 已启动。\n{}",
                env!("CARGO_PKG_VERSION"),
                font_message
            ),
            busy: false,
            current_task: None,
            worker_rx: None,
            close_requested: false,
            startup_repaints: 180,
        };
        app.spawn_task(BackgroundTask::Probe);
        app
    }

    /// 创建一个工作线程执行耗时查询、HID 或部署操作。
    fn spawn_task(&mut self, task: BackgroundTask) {
        if self.busy {
            self.append_detail("已有操作正在执行，请等待完成。".to_owned());
            return;
        }
        self.busy = true;
        self.current_task = Some(task);
        self.append_detail(format!("开始：{}", task.label()));
        let (sender, receiver) = unbounded();
        self.worker_rx = Some(receiver);
        thread::spawn(move || {
            let result = run_task(task).map_err(|error| format!("{error:#}"));
            let _ = sender.send(WorkerEvent::Finished(result));
        });
    }

    /// 消费全部已完成事件，并更新状态卡和详情文本。
    fn poll_worker(&mut self) {
        let Some(receiver) = self.worker_rx.clone() else {
            return;
        };
        while let Ok(event) = receiver.try_recv() {
            match event {
                WorkerEvent::Finished(result) => {
                    self.busy = false;
                    self.worker_rx = None;
                    let task_label = self.current_task.take().map(BackgroundTask::label);
                    match result {
                        Ok(completion) => {
                            if let Some(snapshot) = completion.snapshot {
                                self.snapshot = Some(snapshot);
                            }
                            for detail in completion.details {
                                self.append_detail(detail);
                            }
                            if let Some(label) = task_label {
                                self.append_detail(format!("完成：{label}"));
                            }
                            self.close_requested = completion.exit_after;
                        }
                        Err(error) => {
                            self.append_detail(format!(
                                "{}失败：{error}",
                                task_label.unwrap_or("后台操作")
                            ));
                        }
                    }
                }
            }
        }
    }

    /// 在详情框末尾追加一行，并限制长期 GUI 会话的内存占用。
    fn append_detail(&mut self, message: String) {
        if !self.details.is_empty() {
            self.details.push('\n');
        }
        self.details.push_str(&message);
        if self.details.len() > 32_000 {
            let split_at = self.details.len() - 24_000;
            self.details.drain(..split_at);
        }
    }

    /// 横排三个连接状态，保留悬浮详情，减少点阵下方占用。
    fn draw_connection_status(&self, ui: &mut egui::Ui) {
        status_card(ui, "连接状态", |ui| {
            ui.horizontal(|ui| {
                let snapshot = self.snapshot.as_ref();
                connection_row(
                    ui,
                    "DP-104",
                    snapshot.map(|value| value.keyboard_connected),
                    snapshot
                        .map(|value| value.keyboard_detail.as_str())
                        .unwrap_or("检测中"),
                );
                connection_row(
                    ui,
                    "额度来源",
                    snapshot.map(|value| value.codex_connected),
                    snapshot
                        .map(|value| value.codex_detail.as_str())
                        .unwrap_or("检测中"),
                );
                connection_row(
                    ui,
                    "自动刷新",
                    snapshot.map(|value| value.deployed),
                    snapshot
                        .map(|value| {
                            if value.deployed {
                                "已部署，每分钟检测"
                            } else {
                                "未部署"
                            }
                        })
                        .unwrap_or("检测中"),
                );
            });
        });
    }

    /// 绘制额度、重置时间及最近检测状态卡。
    fn draw_quota_status(&self, ui: &mut egui::Ui) {
        status_card(ui, "额度状态", |ui| {
            let quota = self
                .snapshot
                .as_ref()
                .and_then(|value| value.quota.as_ref());
            if let Some(relay) = quota.and_then(|value| value.relay.as_ref()) {
                ui.columns(2, |columns| {
                    text_row(&mut columns[0], "中转站", &relay.name);
                    text_row(
                        &mut columns[1],
                        "剩余额度",
                        &format!("{} {}", relay.display_text(), relay.unit),
                    );
                });
                text_row(ui, "查询状态", &relay.detail);
            } else {
                ui.columns(2, |columns| {
                    quota_row(
                        &mut columns[0],
                        "5h 额度",
                        quota.and_then(|value| value.five_hour),
                    );
                    quota_row(
                        &mut columns[1],
                        "周额度",
                        quota.and_then(|value| value.seven_day),
                    );
                });
                text_row(
                    ui,
                    "周重置时间",
                    quota
                        .map(QuotaStatus::reset_time_text)
                        .unwrap_or_else(|| "--".to_owned())
                        .as_str(),
                );
                text_row(
                    ui,
                    "当前沙漏",
                    quota
                        .and_then(|value| value.reset_cells)
                        .map(|cells| format!("{cells} / 10 格"))
                        .unwrap_or_else(|| "--".to_owned())
                        .as_str(),
                );
            }
            text_row(
                ui,
                "最近检测",
                self.snapshot
                    .as_ref()
                    .map(|value| value.checked_at.as_str())
                    .unwrap_or("--"),
            );
        });
    }

    /// 操作区固定为按钮与状态一行、操作说明一行。
    fn draw_actions(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).inner_margin(10).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.heading("操作");
                ui.add_space(8.0);
                if action_button(ui, "测试显示", color_primary(), self.busy).clicked() {
                    self.spawn_task(BackgroundTask::TestDisplay);
                }
                if action_button(ui, "一键部署", color_success(), self.busy).clicked() {
                    self.spawn_task(BackgroundTask::Deploy);
                }
                if action_button(ui, "一键移除", color_danger(), self.busy).clicked() {
                    self.spawn_task(BackgroundTask::Remove);
                }
                if self.busy {
                    ui.spinner();
                    ui.label(
                        self.current_task
                            .map(BackgroundTask::label)
                            .unwrap_or("处理中"),
                    );
                }
            });
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(
                    "测试显示会忽略阈值并立即写入；部署后每分钟查询，只有额度达到变化规则才刷新键盘。",
                )
                .size(12.5)
                .color(egui::Color32::from_gray(170)),
            );
        });
    }

    /// 按写屏记录绘制原始 HSV 帧；缓存不可用时显示空态而不模拟键盘内容。
    fn draw_keyboard_display(&self, ui: &mut egui::Ui) {
        status_card(ui, "键盘点阵 · 24 × 8", |ui| {
            ui.label(
                egui::RichText::new(
                    "每秒同步最近成功写屏记录；键盘断电或其它程序改屏无法回读确认。",
                )
                .size(12.0)
                .color(egui::Color32::from_gray(170)),
            );
            if let Some(error) = &self.display_error {
                ui.colored_label(
                    color_danger(),
                    format!("同步暂不可用，保留上次画面：{error}"),
                );
            }
            let Some(frame) = self
                .display
                .as_ref()
                .and_then(|display| display.frame.as_ref())
            else {
                ui.add_space(12.0);
                ui.label("尚无成功写屏记录，点击「测试显示」后展示。");
                return;
            };
            ui.add_space(8.0);
            let pitch = (ui.available_width() / MATRIX_COLS as f32).min(23.0);
            let size = egui::vec2(pitch * MATRIX_COLS as f32, pitch * MATRIX_ROWS as f32);
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - size.x) / 2.0).max(0.0));
                let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, 8.0, egui::Color32::from_rgb(8, 12, 18));
                for (index, hsv) in frame.chunks_exact(3).enumerate() {
                    let center = rect.min
                        + egui::vec2(
                            (index % MATRIX_COLS) as f32 + 0.5,
                            (index / MATRIX_COLS) as f32 + 0.5,
                        ) * pitch;
                    ui.painter()
                        .circle_filled(center, pitch * 0.30, pixel_color(hsv));
                }
            });
        });
    }

    /// 展示写屏来源同币种的最新采样，区分当前累计、已完成周期和未知数据。
    fn draw_consumption(&self, ui: &mut egui::Ui) {
        status_card(ui, "30 min 消耗统计", |ui| {
            let Some(relay) = self
                .display
                .as_ref()
                .and_then(|display| display.relay.as_ref())
            else {
                ui.label("中转站模式显示半小时消耗；官方额度不提供金额统计。");
                return;
            };
            if relay.chart.end_at == 0 {
                ui.label("尚无消耗采样，请部署每分钟采样或使用「测试显示」。");
                return;
            }
            let sampled_at = self.display.as_ref().and_then(|display| display.sampled_at);
            let stale = sampling_is_stale(sampled_at, chrono::Utc::now().timestamp());
            ui.horizontal(|ui| {
                ui.label("当前周期累计");
                ui.label(
                    egui::RichText::new(format!(
                        "{} {}",
                        consumption_text(if stale {
                            None
                        } else {
                            relay.chart.bars[BAR_COUNT - 1]
                        }),
                        relay.unit
                    ))
                    .size(24.0)
                    .color(color_accent()),
                );
                ui.label(format!("{} 起", period_time(relay.chart.end_at)));
            });
            ui.label(
                egui::RichText::new("每格 30 分钟，按从左到右、从上到下排序；右下角为当前周期。")
                    .size(12.0)
                    .color(egui::Color32::from_gray(170)),
            );
            ui.small(format!(
                "最近采样：{}",
                sampled_at
                    .map(|timestamp| {
                        chrono::DateTime::from_timestamp(timestamp, 0)
                            .unwrap()
                            .with_timezone(&chrono::Local)
                            .format("%m-%d %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_else(|| "--".to_owned())
            ));
            if stale {
                ui.colored_label(
                    color_danger(),
                    "采样已停止更新，请用「一键部署」更新后台程序。",
                );
            }
            ui.add_space(4.0);
            egui::ScrollArea::horizontal()
                .id_salt("consumption_periods")
                .show(ui, |ui| {
                    egui::Grid::new("consumption_values")
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            for row_start in [0, 5] {
                                for index in row_start..row_start + 5 {
                                    let start = relay.chart.end_at
                                        - (BAR_COUNT - 1 - index) as i64 * WINDOW_SECONDS;
                                    ui.small(format!(
                                        "{}–{}",
                                        period_time(start),
                                        period_time(start + WINDOW_SECONDS)
                                    ))
                                    .on_hover_text(
                                        chrono::DateTime::from_timestamp(start, 0)
                                            .map(|time| {
                                                time.with_timezone(&chrono::Local)
                                                    .format("%Y-%m-%d %H:%M")
                                                    .to_string()
                                            })
                                            .unwrap_or_else(|| "无效时间".to_owned()),
                                    );
                                }
                                ui.end_row();
                                for index in row_start..row_start + 5 {
                                    let color = if index == BAR_COUNT - 1 {
                                        color_accent()
                                    } else {
                                        egui::Color32::WHITE
                                    };
                                    let amount = if stale && index == BAR_COUNT - 1 {
                                        None
                                    } else {
                                        relay.chart.bars[index]
                                    };
                                    ui.colored_label(color, consumption_text(amount));
                                }
                                ui.end_row();
                            }
                        });
                });
            ui.small(format!(
                "金额单位：{} · 同币种合计 · -- 表示无数据或采样中断",
                relay.unit
            ));
        });
    }

    /// 运行详情独占底部剩余高度，只滚动详情内容，不把操作区推出窗口。
    fn draw_details(&mut self, ui: &mut egui::Ui) {
        ui.heading("运行详情");
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("run_details")
            .stick_to_bottom(true)
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.add_sized(
                    ui.available_size(),
                    egui::TextEdit::multiline(&mut self.details)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .interactive(false),
                );
            });
    }
}

impl eframe::App for QuotaApp {
    /// 每帧接收后台结果并绘制完整窗口。
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
        while let Ok(result) = self.display_rx.try_recv() {
            match result {
                Ok(display) => {
                    self.display = Some(display);
                    self.display_error = None;
                }
                Err(error) => self.display_error = Some(error),
            }
        }
        if self.busy {
            context.request_repaint_after(Duration::from_millis(100));
        }
        if self.startup_repaints > 0 {
            self.startup_repaints -= 1;
            context.request_repaint_after(Duration::from_millis(16));
        }
        if self.close_requested {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::TopBottomPanel::top("header")
            .resizable(false)
            .exact_height(64.0)
            .show(context, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.heading(
                        egui::RichText::new(APP_TITLE)
                            .size(27.0)
                            .color(egui::Color32::WHITE),
                    );
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!("V{}", env!("CARGO_PKG_VERSION")))
                            .size(13.0)
                            .color(color_accent()),
                    );
                });
                ui.label(
                    egui::RichText::new("当前登录账号 · TICKTYPE DP-104 · 本地自动刷新")
                        .size(13.0)
                        .color(egui::Color32::from_gray(175)),
                );
                ui.add_space(8.0);
            });

        egui::TopBottomPanel::bottom("footer")
            .resizable(false)
            .exact_height(28.0)
            .show(context, |ui| {
                ui.add_space(3.0);
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("开发者：smallcat | GitHub：smallcat2333 | 260904")
                            .size(12.5)
                            .color(egui::Color32::from_gray(150)),
                    );
                });
                ui.add_space(3.0);
            });

        egui::CentralPanel::default().show(context, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("dashboard_status")
                .max_height((ui.available_height() - 240.0).max(220.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.columns(2, |columns| {
                        self.draw_keyboard_display(&mut columns[0]);
                        columns[0].add_space(6.0);
                        self.draw_connection_status(&mut columns[0]);
                        self.draw_quota_status(&mut columns[1]);
                        columns[1].add_space(6.0);
                        self.draw_consumption(&mut columns[1]);
                    });
                });
            ui.add_space(6.0);
            self.draw_actions(ui);
            ui.add_space(6.0);
            self.draw_details(ui);
        });
    }
}

/// 每秒只读本地记录，不查询网络或写 HID；窗口关闭后接收端释放，线程退出。
fn spawn_display_monitor(context: egui::Context) -> Receiver<Result<DisplaySnapshot, String>> {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        loop {
            let result = service::read_display_snapshot().map_err(|error| format!("{error:#}"));
            match sender.try_send(result) {
                Ok(()) | Err(crossbeam_channel::TrySendError::Full(_)) => context.request_repaint(),
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
    receiver
}

/// 将键盘 8 位 HSV 映射到 GUI RGB；未点亮的像素保留暗色轮廓。
fn pixel_color(hsv: &[u8]) -> egui::Color32 {
    if hsv[2] == 0 {
        egui::Color32::from_rgb(28, 36, 47)
    } else {
        egui::ecolor::Hsva::new(
            hsv[0] as f32 / 255.0,
            hsv[1] as f32 / 255.0,
            hsv[2] as f32 / 255.0,
            1.0,
        )
        .into()
    }
}

/// 金额按分四舍五入为两位小数，用整数运算避免大金额精度丢失；未知值保持 --。
fn consumption_text(amount: Option<u64>) -> String {
    match amount {
        None => "--".to_owned(),
        Some(amount) => {
            let cents = amount / 10_000 + u64::from(amount % 10_000 >= 5_000);
            format!("{}.{:02}", cents / 100, cents % 100)
        }
    }
}

/// 超过采样允许间隔或时间倒退时，不把旧累计当成当前消耗。
fn sampling_is_stale(sampled_at: Option<i64>, now: i64) -> bool {
    sampled_at.is_none_or(|timestamp| !(0..=120).contains(&now.saturating_sub(timestamp)))
}

/// 转换采样时间到本地时分；区间起点的悬浮提示提供完整日期。
fn period_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "无效时间".to_owned())
}

/// 启动本地 GUI 窗口。
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([1160.0, 760.0])
            .with_min_inner_size([1000.0, 680.0])
            .with_icon(app_icon()),
        renderer: eframe::Renderer::Wgpu,
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|context| Ok(Box::new(QuotaApp::new(context)))),
    )
}

/// 生成与 EXE 资源一致的 32×32 RGBA 应用图标。
fn app_icon() -> egui::IconData {
    const SIZE: usize = 32;
    let mut rgba = Vec::with_capacity(SIZE * SIZE * 4);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let border = !(2..=29).contains(&x) || !(2..=29).contains(&y);
            let key_line = ((6..=25).contains(&x) && matches!(y, 8 | 15 | 22))
                || ((6..=25).contains(&y) && matches!(x, 6 | 12 | 19 | 25));
            let quota_mark =
                (11..=20).contains(&x) && ((10..=12).contains(&y) || (19..=21).contains(&y));
            let color = if border {
                [139, 92, 246, 255]
            } else if quota_mark {
                [52, 211, 153, 255]
            } else if key_line {
                [71, 85, 105, 255]
            } else {
                [17, 24, 39, 255]
            };
            rgba.extend_from_slice(&color);
        }
    }
    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// 执行单项后台任务并汇总 GUI 所需结果。
fn run_task(task: BackgroundTask) -> Result<TaskCompletion> {
    match task {
        BackgroundTask::Probe => {
            let snapshot = service::collect_system_snapshot();
            let details = vec![
                snapshot.keyboard_detail.clone(),
                if snapshot.codex_connected {
                    format!("额度来源：{}", snapshot.codex_detail)
                } else {
                    snapshot.codex_detail.clone()
                },
                snapshot.last_result.clone(),
                format!(
                    "自动刷新：{}",
                    if snapshot.deployed {
                        format!("已部署（任务：{}）", deploy::TASK_NAME)
                    } else {
                        "未部署".to_owned()
                    }
                ),
            ];
            Ok(TaskCompletion {
                snapshot: Some(snapshot),
                details,
                exit_after: false,
            })
        }
        BackgroundTask::TestDisplay => {
            let outcome = service::refresh_once(true)?;
            let snapshot = service::snapshot_from_refresh(&outcome);
            Ok(TaskCompletion {
                details: vec![
                    format!("额度来源：{}", outcome.codex_command),
                    outcome.summary(),
                    "DP-104 HID 回显校验通过".to_owned(),
                ],
                snapshot: Some(snapshot),
                exit_after: false,
            })
        }
        BackgroundTask::Deploy => {
            let details = deploy::deploy_current_exe()?;
            let snapshot = service::collect_system_snapshot();
            Ok(TaskCompletion {
                snapshot: Some(snapshot),
                details,
                exit_after: false,
            })
        }
        BackgroundTask::Remove => {
            let removal = deploy::remove_deployment()?;
            let mut snapshot = service::collect_system_snapshot();
            snapshot.deployed = false;
            Ok(TaskCompletion {
                snapshot: Some(snapshot),
                details: removal.details,
                exit_after: removal.exit_after_remove,
            })
        }
    }
}

/// 应用深色主题并突出主操作颜色。
fn configure_visuals(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = egui::Color32::from_rgb(15, 20, 28);
    visuals.panel_fill = egui::Color32::from_rgb(15, 20, 28);
    visuals.extreme_bg_color = egui::Color32::from_rgb(20, 27, 37);
    visuals.faint_bg_color = egui::Color32::from_rgb(25, 34, 46);
    visuals.widgets.active.bg_fill = color_primary();
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(49, 109, 154);
    visuals.selection.bg_fill = color_primary();
    context.set_visuals(visuals);
}

/// 加载首个可用的 Windows 中文字体，并返回启动详情。
fn configure_fonts(context: &egui::Context) -> String {
    let Some((path, bytes)) = load_cjk_font() else {
        return "未找到系统中文字体，界面文字可能显示异常。".to_owned();
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        CJK_FONT_NAME.to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, CJK_FONT_NAME.to_owned());
    }
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        family.push(CJK_FONT_NAME.to_owned());
    }
    context.set_fonts(fonts);
    format!("已加载中文字体：{path}")
}

/// 从系统字体候选中读取首个可用字体。
fn load_cjk_font() -> Option<(String, Vec<u8>)> {
    CJK_FONT_CANDIDATES
        .iter()
        .find_map(|path| fs::read(path).ok().map(|bytes| ((*path).to_owned(), bytes)))
}

/// 绘制统一的分组状态卡。
fn status_card(ui: &mut egui::Ui, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(title);
            ui.add_space(4.0);
            content(ui);
        });
}

/// 绘制带绿/红/灰状态点的连接行，并在悬停时展示详情。
fn connection_row(ui: &mut egui::Ui, label: &str, state: Option<bool>, detail: &str) {
    ui.horizontal(|ui| {
        let (symbol, color) = match state {
            Some(true) => ("●", color_success()),
            Some(false) => ("●", color_danger()),
            None => ("●", egui::Color32::from_gray(110)),
        };
        ui.label(egui::RichText::new(symbol).color(color));
        ui.label(egui::RichText::new(label).size(13.5));
        ui.label(
            egui::RichText::new(match state {
                Some(true) => "正常",
                Some(false) => "异常",
                None => "检测中",
            })
            .size(12.5)
            .color(egui::Color32::from_gray(170)),
        )
        .on_hover_text(detail);
    });
}

/// 绘制带阈值颜色的额度行；缺失窗口显示绿色无限额。
fn quota_row(ui: &mut egui::Ui, label: &str, value: Option<u8>) {
    let text = value
        .map(|percent| format!("{}%", percent.min(99)))
        .unwrap_or_else(|| "--".to_owned());
    let color = match value {
        None | Some(60..=u8::MAX) => color_success(),
        Some(20..=59) => color_warning(),
        Some(_) => color_danger(),
    };
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{label}："))
                .size(13.0)
                .color(egui::Color32::from_gray(170)),
        );
        ui.label(egui::RichText::new(text).size(15.0).strong().color(color));
    });
}

/// 绘制普通键值状态行。
fn text_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{label}："))
                .size(13.0)
                .color(egui::Color32::from_gray(170)),
        );
        ui.label(
            egui::RichText::new(value)
                .size(13.0)
                .color(egui::Color32::WHITE),
        );
    });
}

/// 绘制等宽主操作按钮，并在后台忙碌时禁用。
fn action_button(
    ui: &mut egui::Ui,
    label: &str,
    color: egui::Color32,
    busy: bool,
) -> egui::Response {
    ui.add_enabled(
        !busy,
        egui::Button::new(
            egui::RichText::new(label)
                .size(14.0)
                .color(egui::Color32::WHITE),
        )
        .fill(color)
        .min_size(egui::vec2(128.0, 34.0)),
    )
}

/// 返回界面主蓝色。
fn color_primary() -> egui::Color32 {
    egui::Color32::from_rgb(41, 100, 143)
}

/// 返回成功绿色。
fn color_success() -> egui::Color32 {
    egui::Color32::from_rgb(44, 174, 118)
}

/// 返回额度中段黄色。
fn color_warning() -> egui::Color32 {
    egui::Color32::from_rgb(226, 177, 58)
}

/// 返回错误与低额度红色。
fn color_danger() -> egui::Color32 {
    egui::Color32::from_rgb(215, 73, 79)
}

/// 返回版本及沙漏使用的粉紫强调色。
fn color_accent() -> egui::Color32 {
    egui::Color32::from_rgb(201, 126, 224)
}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// 验证两位小数、四舍五入及未知数据；不通过浮点损失大金额精度。
    #[test]
    fn preserves_consumption_precision_and_missing_values() {
        assert_eq!(consumption_text(None), "--");
        assert_eq!(consumption_text(Some(0)), "0.00");
        assert_eq!(consumption_text(Some(4_999)), "0.00");
        assert_eq!(consumption_text(Some(5_000)), "0.01");
        assert_eq!(consumption_text(Some(2_995_000)), "3.00");
        assert_eq!(consumption_text(Some(2_500_000)), "2.50");
        assert_eq!(consumption_text(Some(u64::MAX)), "18446744073709.55");
    }

    /// 旧版后台不再更新共用缓存时，超过两分钟必须识别为过期。
    #[test]
    fn detects_stopped_sampling() {
        assert!(!sampling_is_stale(Some(1000), 1120));
        assert!(sampling_is_stale(Some(1000), 1121));
        assert!(sampling_is_stale(None, 1000));
        assert!(sampling_is_stale(Some(1001), 1000));
    }

    /// 验证无缓存、官方及中转点阵可在窄窗口完成真实 egui 布局与绘制。
    #[test]
    fn renders_monitor_states_at_minimum_width() {
        let context = egui::Context::default();
        let (_, receiver) = crossbeam_channel::bounded(1);
        let mut app = QuotaApp {
            snapshot: None,
            display: None,
            display_error: None,
            display_rx: receiver,
            details: String::new(),
            busy: false,
            current_task: None,
            worker_rx: None,
            close_requested: false,
            startup_repaints: 0,
        };
        let mut status = QuotaStatus {
            relay: None,
            five_hour: Some(60),
            seven_day: Some(19),
            seven_day_resets_at: None,
            reset_cells: Some(8),
        };
        for state in 0..3 {
            if state == 2 {
                status.relay = Some(crate::ccswitch::RelayBalance {
                    chart: crate::consumption::ConsumptionChart {
                        end_at: chrono::Utc::now().timestamp(),
                        bars: [
                            None,
                            Some(0),
                            Some(1),
                            Some(1_000_000),
                            Some(2_000_000),
                            Some(3_000_000),
                            Some(4_000_000),
                            Some(5_000_000),
                            None,
                            Some(2_500_000),
                        ],
                    },
                    provider_id: "fixture".to_owned(),
                    name: "fixture".to_owned(),
                    remaining: Some(148_000_000),
                    unit: "USD".to_owned(),
                    detail: String::new(),
                });
            }
            if state > 0 {
                app.display = Some(DisplaySnapshot {
                    frame: Some(crate::keyboard::build_static_frame(&status).unwrap()),
                    relay: status.relay.clone(),
                    sampled_at: Some(chrono::Utc::now().timestamp()),
                });
            }
            let output = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 680.0),
                    )),
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        ui.columns(2, |columns| {
                            app.draw_keyboard_display(&mut columns[0]);
                            app.draw_consumption(&mut columns[1]);
                        });
                    });
                },
            );
            assert!(!output.shapes.is_empty());
            if state > 0 {
                let circles = output
                    .shapes
                    .iter()
                    .filter(|shape| matches!(shape.shape, egui::epaint::Shape::Circle(_)))
                    .count();
                assert_eq!(circles, MATRIX_COLS * MATRIX_ROWS);
            }
        }
    }
}

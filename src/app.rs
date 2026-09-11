//! eframe/egui 中文图形界面与后台任务调度。

use crate::deploy;
use crate::quota::QuotaStatus;
use crate::service::{self, SystemSnapshot};
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
            details: format!(
                "Codex 键盘额度工具 v{} 已启动。\n{}",
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

    /// 绘制连接状态卡。
    fn draw_connection_status(&self, ui: &mut egui::Ui) {
        status_card(ui, "连接状态", |ui| {
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
    }

    /// 绘制额度、重置时间及最近检测状态卡。
    fn draw_quota_status(&self, ui: &mut egui::Ui) {
        status_card(ui, "额度状态", |ui| {
            let quota = self
                .snapshot
                .as_ref()
                .and_then(|value| value.quota.as_ref());
            if let Some(relay) = quota.and_then(|value| value.relay.as_ref()) {
                text_row(ui, "中转站", &relay.name);
                text_row(
                    ui,
                    "剩余额度",
                    &format!("{} {}", relay.display_text(), relay.unit),
                );
                text_row(ui, "查询状态", &relay.detail);
                text_row(ui, "消耗柱", "10 柱 × 30 分钟累计，最右侧最新");
            } else {
                quota_row(ui, "5h 额度", quota.and_then(|value| value.five_hour));
                quota_row(ui, "周额度", quota.and_then(|value| value.seven_day));
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
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    self.snapshot
                        .as_ref()
                        .map(|value| value.last_result.as_str())
                        .unwrap_or("正在读取当前账号额度……"),
                )
                .size(12.5)
                .color(egui::Color32::from_gray(175)),
            );
        });
    }

    /// 绘制三项主操作按钮及其执行状态。
    fn draw_actions(&mut self, ui: &mut egui::Ui) {
        status_card(ui, "操作", |ui| {
            ui.horizontal(|ui| {
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
            ui.add_space(7.0);
            ui.label(
                egui::RichText::new(
                    "测试显示会忽略阈值并立即写入；部署后每分钟查询，只有额度达到变化规则才刷新键盘。",
                )
                .size(12.5)
                .color(egui::Color32::from_gray(170)),
            );
        });
    }

    /// 绘制底部只读、可滚动详情框。
    fn draw_details(&mut self, ui: &mut egui::Ui) {
        ui.heading("运行详情");
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.details)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .interactive(false),
                );
            });
    }
}

impl eframe::App for QuotaApp {
    /// 每帧接收后台结果并绘制完整窗口。
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker();
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
            .exact_height(72.0)
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
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
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
            .exact_height(36.0)
            .show(context, |ui| {
                ui.add_space(7.0);
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new("开发者：smallcat | GitHub：smallcat2333 | 260904")
                            .size(12.5)
                            .color(egui::Color32::from_gray(150)),
                    );
                });
                ui.add_space(7.0);
            });

        egui::CentralPanel::default().show(context, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.columns(2, |columns| {
                        self.draw_connection_status(&mut columns[0]);
                        self.draw_quota_status(&mut columns[1]);
                    });
                    ui.add_space(10.0);
                    self.draw_actions(ui);
                    ui.add_space(10.0);
                    self.draw_details(ui);
                });
        });
    }
}

/// 启动本地 GUI 窗口。
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([900.0, 690.0])
            .with_min_inner_size([760.0, 610.0])
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
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.heading(title);
            ui.add_space(8.0);
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

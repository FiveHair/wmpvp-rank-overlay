//! 完美世界竞技平台(CS2)对局段位监控 — OBS 覆盖层 + 配置窗口 + 托盘图标。
//!
//! 配置优先读 exe 同级 config.json(token / steamId / 端口), 也可用命令行参数覆盖。
//! 启动后托盘图标常驻: 左键图标显示配置窗口, 右键图标打开菜单(退出); 关闭窗口直接退出。
//! OBS 添加"浏览器源", URL 填 http://127.0.0.1:<端口>。

// 发布版隐藏控制台窗口(日志在设置窗口底部面板查看)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod datalog;
mod form;
mod mock;
mod monitor;
mod plugin;
mod rank;
mod server;
mod state;
mod wmpvp;

use std::sync::Arc;

use eframe::egui;
use monitor::MonitorConfig;
use tokio::sync::watch;

/// 内存日志环形缓冲(设置窗口底部实时展示)
type LogBuf = Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

/// tracing 输出 tee: 完整行同时写 stderr 与内存缓冲。
struct TeeWriter {
    buf: LogBuf,
    line: String,
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.push_str(&String::from_utf8_lossy(buf));
        while let Some(pos) = self.line.find('\n') {
            let raw: String = self.line.drain(..=pos).collect();
            let line = raw.trim_end().to_string();
            // stderr 写失败不能 panic(无控制台子系统下 eprintln 会崩), 忽略错误
            let _ = writeln!(std::io::stderr(), "{}", line);
            let mut q = self.buf.lock().unwrap();
            if q.len() >= 400 {
                q.pop_front();
            }
            q.push_back(line);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 日志行简化显示: "2026-..T06:19:31.017Z  INFO xx" -> "06:19:31 INFO xx"
fn short_log_line(l: &str) -> String {
    let b = l.as_bytes();
    if b.len() > 24 && b.get(10) == Some(&b'T') {
        let time = &l[11..19];
        let rest = match l.find("  ") {
            Some(i) => l[i..].trim_start(),
            None => "",
        };
        format!("{} {}", time, rest)
    } else {
        l.to_string()
    }
}

/// 日志时间戳: 本地时区 "2026-08-28T15:04:05.123"(short_log_line 按 T 截取时分秒)
struct LocalTimer;
impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"))
    }
}

#[derive(Default)]
struct CliArgs {
    steam_id: Option<String>,
    token: Option<String>,
    port: Option<u16>,
    poll_ms: Option<u64>,
    help: bool,
}

fn print_usage() {
    // 无控制台子系统下 eprintln 会 panic, 统一忽略写入错误
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "完美段位监控: 配置窗口 + 托盘图标 + OBS 覆盖层\n\
         \n\
         启动后先显示配置窗口, 填写 token / Steam ID 保存即生效;\n\
         配置保存在 exe 同目录 config.json。\n\
         \n\
         可选命令行参数(覆盖配置文件):\n\
         \x20 --steam-id <id>      被监控账号的 Steam 64 位 ID\n\
         \x20 --token <token>      完美电竞登录 token(可选; 提供后显示赛季统计)\n\
         \x20 --port, -p <端口>    本地面板端口(默认 8910)\n\
         \x20 --poll-ms <毫秒>     对局数据刷新间隔(默认 3000, 最小 1000)\n\
         \x20 --help, -h           显示帮助\n\
         \n\
         OBS 用法: 添加浏览器源, URL 填 http://127.0.0.1:<端口>\n"
    );
}

fn parse_cli(args: &[String]) -> CliArgs {
    let mut cli = CliArgs::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--steam-id" => {
                i += 1;
                if i < args.len() {
                    cli.steam_id = Some(args[i].trim().to_string());
                }
            }
            "--token" => {
                i += 1;
                if i < args.len() {
                    cli.token = Some(args[i].trim().to_string());
                }
            }
            "--port" | "-p" => {
                i += 1;
                if i < args.len() {
                    cli.port = args[i].parse().ok();
                }
            }
            "--poll-ms" => {
                i += 1;
                if i < args.len() {
                    cli.poll_ms = args[i].parse().ok();
                }
            }
            "--help" | "-h" => cli.help = true,
            _ => {}
        }
        i += 1;
    }
    cli
}

fn main() -> anyhow::Result<()> {
    // 日志: 同时输出到 stderr 与内存环形缓冲(设置窗口底部实时展示)
    let log_buf: LogBuf = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    {
        let w = log_buf.clone();
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_target(false)
            .with_ansi(false)
            .with_timer(LocalTimer)
            .with_writer(move || TeeWriter {
                buf: w.clone(),
                line: String::new(),
            })
            .init();
    }

    // 单实例锁: exe 同级 instance.lock, 已有实例持锁时本次启动直接退出
    {
        let mut lock_path = config::config_path();
        lock_path.set_file_name("instance.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        if lock_file.try_lock().is_err() {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "已有一个完美段位监控实例在运行, 本次启动退出。"
            );
            return Ok(());
        }
        // 持锁到进程退出
        std::mem::forget(lock_file);
    }

    let args: Vec<String> = std::env::args().collect();
    let cli = parse_cli(&args);
    if cli.help {
        print_usage();
        return Ok(());
    }

    // 配置文件优先, 命令行参数覆盖
    let mut file = config::load();
    if let Some(v) = cli.steam_id {
        file.steam_id = v;
    }
    if let Some(v) = cli.token {
        file.token = v;
    }
    if let Some(v) = cli.port {
        file.port = v;
    }
    if let Some(v) = cli.poll_ms {
        file.poll_ms = v;
    }
    if file.port == 0 {
        file.port = 8910;
    }
    if file.poll_ms < 1000 {
        file.poll_ms = 3000;
    }

    let initial = MonitorConfig {
        steam_id: file.steam_id.trim().to_string(),
        token: file.token.trim().to_string(),
        poll_ms: file.poll_ms,
        port: file.port,
        anim_exit: file.anim_exit,
        mock: file.mock,
    };

    let (cfg_tx, state) = start_service(initial)?;

    // 固定尺寸窗口(最小=最大限制拉伸); 窗口位置由 eframe 自带持久化记住(正常关闭后恢复)
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 640.0])
            .with_min_inner_size([520.0, 640.0])
            .with_title("完美段位监控"),
        ..Default::default()
    };
    let app = OverlayApp::new(cfg_tx, state, log_buf, file);
    // glow 后端的错误类型非 Send, 不能用 ? 直接透传给 anyhow
    match eframe::run_native(
        "wmpvp-rank-overlay",
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            configure_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    ) {
        Ok(()) => {
            // 直接结束进程: 后台 tokio runtime/WS 等待收尾会拖慢退出, 状态已在上面保存
            std::process::exit(0);
        }
        Err(e) => {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "eframe 运行错误: {e}");
            std::process::exit(1);
        }
    }
}

// ================= 深色主题 =================

/// 界面配色(CS 风格深蓝 + 金色主色, 与托盘图标一致)。
mod theme {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(17, 22, 32);
    pub const CARD: Color32 = Color32::from_rgb(25, 32, 46);
    pub const STROKE: Color32 = Color32::from_rgb(48, 58, 80);
    pub const INPUT_BG: Color32 = Color32::from_rgb(13, 17, 24);
    pub const WIDGET_BG: Color32 = Color32::from_rgb(36, 45, 64);
    pub const WIDGET_HOVER: Color32 = Color32::from_rgb(47, 59, 84);
    pub const TEXT: Color32 = Color32::from_rgb(228, 235, 245);
    pub const GOLD: Color32 = Color32::from_rgb(255, 213, 107);
    pub const GREEN: Color32 = Color32::from_rgb(118, 217, 138);
    pub const RED: Color32 = Color32::from_rgb(255, 128, 110);
    pub const BLUE: Color32 = Color32::from_rgb(90, 200, 250);
}

/// 全局控件样式: 深色底、圆角、统一间距。
fn configure_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = theme::BG;
        v.window_fill = theme::BG;
        v.extreme_bg_color = theme::INPUT_BG;
        v.faint_bg_color = theme::CARD;
        v.widgets.noninteractive.bg_fill = theme::CARD;
        v.widgets.noninteractive.weak_bg_fill = theme::CARD;
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, theme::STROKE);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, theme::TEXT);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered] {
            w.bg_fill = theme::WIDGET_BG;
            w.weak_bg_fill = theme::WIDGET_BG;
            w.bg_stroke = egui::Stroke::new(1.0, theme::STROKE);
            w.fg_stroke = egui::Stroke::new(1.0, theme::TEXT);
            w.corner_radius = egui::CornerRadius::same(6);
        }
        v.widgets.hovered.bg_fill = theme::WIDGET_HOVER;
        v.widgets.active.bg_fill = theme::WIDGET_HOVER;
        v.widgets.active.weak_bg_fill = theme::WIDGET_HOVER;
        v.widgets.active.bg_stroke = egui::Stroke::new(1.0, theme::GOLD.gamma_multiply(0.7));
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, theme::GOLD);
        v.widgets.active.corner_radius = egui::CornerRadius::same(6);
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    });
}

/// 加载系统中文字体(egui 默认字体不含 CJK, 会导致中文显示为方框)。
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        ("msyh", "C:\\Windows\\Fonts\\msyh.ttc"), // 微软雅黑
        ("simhei", "C:\\Windows\\Fonts\\simhei.ttf"), // 黑体
        ("simsun", "C:\\Windows\\Fonts\\simsun.ttc"), // 宋体
        ("deng", "C:\\Windows\\Fonts\\Deng.ttf"), // 等线
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_string(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push(name.to_string());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(name.to_string());
            tracing::info!(font = name, "已加载中文字体");
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// 后台服务: axum 面板 + 数据源(真实监控/占位数据) + 插件, 跑在独立 tokio runtime 线程。
/// mock 开关经 watch 通道热切换: 保存并应用后立即生效, 不需重启。
fn start_service(
    initial: MonitorConfig,
) -> anyhow::Result<(watch::Sender<Arc<MonitorConfig>>, Arc<state::AppState>)> {
    let rt = tokio::runtime::Runtime::new()?;
    let client = wmpvp::build_client()?;
    let state = state::AppState::new();
    // RankService/FormService::new 内部使用 tokio::spawn, 需要先进入 runtime context
    let _guard = rt.enter();
    let ranks = Arc::new(rank::RankService::new(client.clone()));
    let forms = form::FormService::new(client.clone(), initial.token.clone());
    let (cfg_tx, cfg_rx) = watch::channel(Arc::new(initial));
    // 数据源监督: mock 变化时中止当前源、清空数据、启动另一源
    rt.spawn({
        let state = Arc::clone(&state);
        let ranks = Arc::clone(&ranks);
        let forms = forms.clone();
        let client = client.clone();
        let mut cfg_rx = cfg_rx.clone();
        async move {
            let mut mock = cfg_rx.borrow().mock;
            let mut src = tokio::spawn(run_source(
                mock,
                client.clone(),
                Arc::clone(&state),
                ranks.clone(),
                forms.clone(),
                cfg_rx.clone(),
            ));
            while cfg_rx.changed().await.is_ok() {
                let m = cfg_rx.borrow().mock;
                if m == mock {
                    continue;
                }
                mock = m;
                src.abort();
                state.reset_data().await;
                state.bump_cfg_epoch();
                tracing::info!(mock = mock, "数据源已切换");
                src = tokio::spawn(run_source(
                    mock,
                    client.clone(),
                    Arc::clone(&state),
                    ranks.clone(),
                    forms.clone(),
                    cfg_rx.clone(),
                ));
            }
        }
    });
    // 数据插件(plugins/*.json, 见 docs/plugins.md)
    plugin::spawn_all(Arc::clone(&state), cfg_rx.clone());
    rt.spawn(server::serve(
        Arc::clone(&state),
        Arc::clone(&ranks),
        Arc::clone(&forms),
        cfg_rx,
    ));
    // 保持 runtime 在后台线程持续运行
    std::thread::spawn(move || {
        let _ = rt.block_on(std::future::pending::<()>());
    });
    Ok((cfg_tx, state))
}

/// 按 mock 标志启动对应数据源(占位数据 / 真实 WS 监控), 两者都不返回
async fn run_source(
    mock: bool,
    client: reqwest::Client,
    state: Arc<state::AppState>,
    ranks: Arc<rank::RankService>,
    forms: Arc<form::FormService>,
    cfg_rx: watch::Receiver<Arc<MonitorConfig>>,
) {
    if mock {
        let sid = cfg_rx.borrow().steam_id.clone();
        mock::run(state, ranks, forms, sid).await;
    } else {
        monitor::run(client, state, ranks, forms, cfg_rx).await;
    }
}

// ================= 配置窗口 =================

struct OverlayApp {
    cfg_tx: watch::Sender<Arc<MonitorConfig>>,
    state: Arc<state::AppState>,
    file: config::ConfigFile,
    /// 日志环形缓冲(底部面板展示)
    log: LogBuf,
    token_input: String,
    steam_input: String,
    port_input: String,
    poll_input: String,
    msg: Option<(bool, String)>,
    quit: bool,
    show_window: bool,
    _tray: Option<tray_icon::TrayIcon>,
    tray_ids: (tray_icon::menu::MenuId, tray_icon::menu::MenuId),
    /// 托盘图标当前表示的 WS 状态(变化时换图标边框色)
    tray_status: String,
}

impl OverlayApp {
    fn new(
        cfg_tx: watch::Sender<Arc<MonitorConfig>>,
        state: Arc<state::AppState>,
        log: LogBuf,
        file: config::ConfigFile,
    ) -> Self {
        let (tray, tray_ids) = build_tray();
        Self {
            token_input: file.token.clone(),
            steam_input: file.steam_id.clone(),
            port_input: file.port.to_string(),
            poll_input: file.poll_ms.to_string(),
            cfg_tx,
            state,
            log,
            file,
            msg: None,
            quit: false,
            show_window: true,
            _tray: tray,
            tray_ids,
            tray_status: String::new(),
        }
    }

    /// 从托盘打开/还原窗口
    fn restore_window(&mut self, ctx: &egui::Context) {
        self.show_window = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn save(&mut self) {
        let token = self.token_input.trim().to_string();
        let steam_id = self.steam_input.trim().to_string();
        let port: u16 = self.port_input.trim().parse().unwrap_or(8910);
        let poll_ms: u64 = self.poll_input.trim().parse().unwrap_or(3000).max(1000);
        if steam_id.is_empty() {
            self.msg = Some((true, "Steam ID 不能为空".to_string()));
            return;
        }
        if port == 0 {
            self.msg = Some((true, "端口无效".to_string()));
            return;
        }
        self.file.token = token.clone();
        self.file.steam_id = steam_id.clone();
        self.file.port = port;
        self.file.poll_ms = poll_ms;
        match config::save(&self.file) {
            Ok(_) => {
                let _ = self.cfg_tx.send(Arc::new(MonitorConfig {
                    steam_id,
                    token,
                    poll_ms,
                    port,
                    anim_exit: self.file.anim_exit,
                    mock: self.file.mock,
                }));
                // 通知前端: 配置已变化, 丢弃本局已展示状态并按新数据重新展示
                self.state.bump_cfg_epoch();
                self.msg = Some((
                    false,
                    format!("已保存并应用, 面板地址: http://127.0.0.1:{}", port),
                ));
            }
            Err(e) => self.msg = Some((true, format!("保存失败: {}", e))),
        }
    }

    fn ui_impl(&mut self, ui: &mut egui::Ui) {
        let ws_status = self
            .state
            .my
            .try_lock()
            .map(|m| m.ws_status.clone())
            .unwrap_or_default();

        // ---------- 头部 ----------
        ui.horizontal(|ui| {
            egui::Frame::NONE
                .fill(theme::GOLD.gamma_multiply(0.20))
                .stroke(egui::Stroke::new(1.0, theme::GOLD.gamma_multiply(0.55)))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(9, 7))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("★").size(16.0).color(theme::GOLD));
                });
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("完美段位监控").size(18.0).strong());
                ui.label(
                    egui::RichText::new("完美世界竞技平台 · OBS 覆盖层")
                        .size(11.0)
                        .weak(),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_pill(ui, &ws_status);
            });
        });
        ui.add_space(4.0);

        // ---------- 浏览器源地址 ----------
        let port = self.file.port;
        let mut copied = None;
        egui::Grid::new("urls")
            .num_columns(3)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for (label, path) in [("账号卡", "/"), ("对局板", "/match")] {
                    ui.label(egui::RichText::new(label).size(11.0).weak());
                    ui.label(
                        egui::RichText::new(format!("http://127.0.0.1:{}{}", port, path))
                            .monospace()
                            .weak()
                            .size(11.0),
                    );
                    if ui
                        .add(egui::Button::new(egui::RichText::new("复制").size(11.0)))
                        .on_hover_text("复制 OBS 浏览器源地址到剪贴板")
                        .clicked()
                    {
                        copied = Some(format!("http://127.0.0.1:{}{}", port, path));
                    }
                    ui.end_row();
                }
            });
        if let Some(url) = copied {
            ui.ctx().copy_text(url);
            self.msg = Some((false, "面板地址已复制到剪贴板".to_string()));
        }

        // ---------- 配置表单 ----------
        section_title(ui, "配置（保存后自动生效）");
        egui::Grid::new("cfg")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Token");
                ui.add(
                    egui::TextEdit::singleline(&mut self.token_input)
                        .password(true)
                        .hint_text("完美电竞登录 token")
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.label("Steam ID");
                ui.add(
                    egui::TextEdit::singleline(&mut self.steam_input)
                        .hint_text("64 位 Steam ID")
                        .desired_width(320.0),
                );
                ui.end_row();
                ui.label("端口");
                ui.add(
                    egui::TextEdit::singleline(&mut self.port_input)
                        .hint_text("8910")
                        .desired_width(110.0),
                );
                ui.end_row();
                ui.label("轮询间隔 ms");
                ui.add(
                    egui::TextEdit::singleline(&mut self.poll_input)
                        .hint_text("3000")
                        .desired_width(110.0),
                );
                ui.end_row();
                ui.label("模拟数据");
                ui.checkbox(&mut self.file.mock, "调试模式(占位数据)")
                    .on_hover_text("开启后本程序与插件全部使用占位/模拟数据, 保存并应用后立即切换; 不访问任何完美接口");
                ui.end_row();
                ui.label("对局板退出");
                ui.checkbox(&mut self.file.anim_exit, "入场播完 5 秒后自动退出动画")
                    .on_hover_text("勾选: 对局板入场动画播完展示 5 秒后倒序播放退出动画; 不勾选: 一直展示直到下一局");
                ui.end_row();
            });

        // ---------- 操作按钮 + 消息 ----------
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let save_btn = egui::Button::new(
                egui::RichText::new("保存并应用")
                    .strong()
                    .color(egui::Color32::from_rgb(35, 30, 14)),
            )
            .fill(theme::GOLD)
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(130.0, 30.0));
            if ui.add(save_btn).clicked() {
                self.save();
            }
            if let Some((err, text)) = &self.msg {
                let color = if *err { theme::RED } else { theme::GREEN };
                ui.label(
                    egui::RichText::new(format!("● {}", text))
                        .color(color)
                        .size(12.0),
                );
            }
        });

        ui.add_space(4.0);

        // ---------- 日志面板(实时输出, 自动滚到底, 填满窗口剩余高度) ----------
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("日志输出").size(12.0).strong());
            if ui.button("清空").clicked() {
                self.log.lock().unwrap().clear();
            }
        });
        let log_h = (ui.available_height() - 6.0).max(120.0);
        egui::ScrollArea::vertical()
            .max_height(log_h)
            .min_scrolled_height(log_h)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                let lines: Vec<String> =
                    self.log.lock().unwrap().iter().map(|l| l.clone()).collect();
                if lines.is_empty() {
                    ui.label(egui::RichText::new("(暂无日志)").weak().monospace());
                    return;
                }
                // 只渲染最近 100 条, 防止日志过多拖慢界面
                let start = lines.len().saturating_sub(100);
                for line in &lines[start..] {
                    let short = short_log_line(&line);
                    let color = if short.contains("ERROR") {
                        theme::RED
                    } else if short.contains("WARN") {
                        egui::Color32::from_rgb(235, 185, 90)
                    } else {
                        theme::TEXT.gamma_multiply(0.72)
                    };
                    ui.label(
                        egui::RichText::new(short)
                            .monospace()
                            .size(10.5)
                            .color(color),
                    );
                }
            });
    }
}

/// 区块小标题。
fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text).size(12.0).strong().color(theme::TEXT));
}

/// 带彩色圆点的状态胶囊。
fn status_pill(ui: &mut egui::Ui, ws_status: &str) {
    let c = status_color(ws_status);
    egui::Frame::NONE
        .fill(c.gamma_multiply(0.25))
        .corner_radius(egui::CornerRadius::same(9))
        .inner_margin(egui::Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(egui::RichText::new("●").size(10.0).color(c));
            ui.label(egui::RichText::new(status_text(ws_status)).size(12.0).color(theme::TEXT));
        });
}

impl eframe::App for OverlayApp {
    /// 每帧前调用(窗口隐藏时也调用): 处理托盘事件与关闭请求。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        use tray_icon::menu::MenuEvent;
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.tray_ids.0 {
                self.restore_window(ctx);
            } else if ev.id == self.tray_ids.1 {
                self.quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // 托盘图标左键单击/双击 -> 打开/还原窗口(菜单仅右键)
        while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
            match ev {
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                }
                | tray_icon::TrayIconEvent::DoubleClick {
                    button: tray_icon::MouseButton::Left,
                    ..
                } => self.restore_window(ctx),
                _ => {}
            }
        }
        // 托盘图标整块底色 + 标题栏同步显示 WS 状态
        let ws_status = self
            .state
            .my
            .try_lock()
            .map(|m| m.ws_status.clone())
            .unwrap_or_default();
        if ws_status != self.tray_status {
            self.tray_status = ws_status.clone();
            if let Some(t) = &self._tray {
                if let Err(e) = t.set_icon(Some(make_tray_icon(Some(&ws_status)))) {
                    tracing::warn!(error = %e, "托盘图标更新失败");
                }
                let tip = format!("{} - 完美段位监控", status_text(&ws_status));
                if let Err(e) = t.set_tooltip(Some(tip)) {
                    tracing::warn!(error = %e, "托盘提示更新失败");
                }
            }
            // 状态同步到设置窗口标题栏(状态在前)
            let title = format!("{} - 完美段位监控", status_text(&ws_status));
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
        // 关闭窗口 -> 直接退出程序(单实例锁随进程释放)
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit {
            self.quit = true;
        }
        // 无论窗口是否有焦点都保持定时重绘(后台日志/数据仍持续刷新)
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.show_window {
            return;
        }
        // 四周留白 + 内容(日志面板填满窗口剩余高度, 底边即窗口底)
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| self.ui_impl(ui));
    }
}

fn status_text(s: &str) -> &'static str {
    match s {
        "in-match" => "对局中",
        "idle" => "空闲",
        "connecting" => "连接中",
        "resolving" => "查询账号",
        "token-invalid" => "Token 已失效",
        "error" => "错误",
        _ => "未启动",
    }
}

fn status_color(s: &str) -> egui::Color32 {
    match s {
        "in-match" => theme::BLUE,
        "idle" => theme::GREEN,
        "connecting" | "resolving" => theme::GOLD,
        "token-invalid" | "error" => theme::RED,
        _ => egui::Color32::from_rgb(130, 140, 160),
    }
}

// ================= 托盘图标 =================

fn build_tray() -> (
    Option<tray_icon::TrayIcon>,
    (tray_icon::menu::MenuId, tray_icon::menu::MenuId),
) {
    use tray_icon::menu::{Menu, MenuItem};
    let menu = Menu::new();
    let show = MenuItem::new("显示配置窗口", true, None);
    let quit = MenuItem::new("退出", true, None);
    let show_id = show.id().clone();
    let quit_id = quit.id().clone();
    if let Err(e) = menu.append_items(&[&show, &quit]) {
        tracing::warn!(error = %e, "创建托盘菜单失败");
        return (None, (show_id, quit_id));
    }
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        // 菜单仅右键弹出; 左键单击/双击只开窗口(见 logic 里的 TrayIconEvent)
        .with_menu_on_left_click(false)
        .with_tooltip("完美段位监控 - OBS 覆盖层")
        .with_icon(make_tray_icon(None))
        .build();
    match tray {
        Ok(t) => (Some(t), (show_id, quit_id)),
        Err(e) => {
            tracing::warn!(error = %e, "创建托盘图标失败");
            (None, (show_id, quit_id))
        }
    }
}

/// 代码生成 32x32 托盘图标: 整块状态色底 + 白边 + 深色 S。
/// 任务栏以 16px 渲染, 细边框看不出变化, 所以用整块底色表达 WS 状态:
/// 对局蓝 / 空闲绿 / 连接金 / 错误红; 未启动 = 深蓝底金边。
fn make_tray_icon(status: Option<&str>) -> tray_icon::Icon {
    const W: usize = 32;
    const H: usize = 32;
    const R: f32 = 7.0;
    let mut px = vec![0u8; W * H * 4];
    let (bg, border, letter) = match status {
        Some(s) => {
            let c = status_color(s);
            ([c.r(), c.g(), c.b(), 255], [240, 244, 248, 255], [17, 22, 32, 255])
        }
        None => ([22, 32, 46, 255], [255, 213, 107, 255], [240, 244, 248, 255]),
    };

    for y in 0..H {
        for x in 0..W {
            let fx = x as f32 + 0.5 - (W as f32 / 2.0);
            let fy = y as f32 + 0.5 - (H as f32 / 2.0);
            let outer = rounded_sdf(fx, fy, W as f32, H as f32, R);
            if outer > 0.5 {
                continue; // 圆角外, 透明
            }
            let inner = rounded_sdf(fx, fy, W as f32, H as f32, R - 2.5);
            let col = if inner > 0.0 { border } else { bg };
            let i = (y * W + x) * 4;
            px[i..i + 4].copy_from_slice(&col);
        }
    }
    // 白色 S: 5x7 点阵, 放大 2 倍
    const S_PATTERN: [&str; 7] = ["01110", "10001", "10000", "01110", "00001", "10001", "01110"];
    let scale = 2usize;
    let ox = (W - 5 * scale) / 2;
    let oy = (H - 7 * scale) / 2;
    for (row, line) in S_PATTERN.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            if ch == '0' {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = ox + col * scale + dx;
                    let y = oy + row * scale + dy;
                    if x < W && y < H {
                        let i = (y * W + x) * 4;
                        px[i..i + 4].copy_from_slice(&letter);
                    }
                }
            }
        }
    }
    tray_icon::Icon::from_rgba(px, W as u32, H as u32).unwrap()
}

/// 圆角矩形 SDF(相对中心点): 返回 <=0 表示在矩形内。
fn rounded_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let bx = w / 2.0 - r;
    let by = h / 2.0 - r;
    let qx = px.abs() - bx;
    let qy = py.abs() - by;
    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r
}

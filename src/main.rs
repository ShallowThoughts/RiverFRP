#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::VecDeque,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use eframe::egui::{
    self, Align, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, RichText, Shape,
    Stroke, StrokeKind, Vec2,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const APP_NAME: &str = "RiverFRP";
const FRPC_VERSION: &str = "0.64.0";
const FRPC_SHA256: &str = "44ef367d7a10cff9e543fcfa0a71735658a329dd98fddbcb1a827f928d08f0a3";
const DEFAULT_SERVER: &str = "39.107.55.139";
const DEFAULT_SERVER_PORT: u16 = 7100;
const PUBLIC_SERVER_HOST: &str = "frp.lxixi.top";
const WALLPAPER_API: &str = "https://api.lxixi.top/anime";
const OIDC_TOKEN_ENDPOINT: &str = "https://frp.lxixi.top/oauth/token";
const ACCOUNT_API_BASE: &str = "https://frp.lxixi.top";
const FRPC_BYTES: &[u8] = include_bytes!("../vendor/frpc.exe");
const FRP_LICENSE: &[u8] = include_bytes!("../vendor/LICENSE-frp");

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    server_addr: String,
    server_port: u16,
    encrypted_access_key: String,
    account_email: String,
    device_id: String,
    tunnel_name: String,
    local_port: String,
    protocol: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_addr: DEFAULT_SERVER.to_owned(),
            server_port: DEFAULT_SERVER_PORT,
            encrypted_access_key: String::new(),
            account_email: String::new(),
            device_id: Uuid::new_v4().simple().to_string()[..12].to_owned(),
            tunnel_name: "我的隧道".to_owned(),
            local_port: "25565".to_owned(),
            protocol: "tcp".to_owned(),
        }
    }
}

#[derive(PartialEq, Eq)]
enum RunState {
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(PartialEq, Eq)]
enum AccountState {
    SignedOut,
    Starting,
    Waiting,
    SignedIn,
    Error,
}

enum AccountEvent {
    OpenBrowser(String),
    SignedIn { access_key: String, email: String },
    Error(String),
}

#[derive(Serialize)]
struct DeviceStartRequest {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(rename = "deviceName")]
    device_name: String,
}

#[derive(Deserialize)]
struct DeviceStartResponse {
    #[serde(rename = "requestToken")]
    request_token: String,
    #[serde(rename = "verificationUrl")]
    verification_url: String,
}

#[derive(Serialize)]
struct DevicePollRequest<'a> {
    #[serde(rename = "requestToken")]
    request_token: &'a str,
}

#[derive(Deserialize)]
struct DevicePollResponse {
    status: String,
    #[serde(rename = "accessKey")]
    access_key: Option<String>,
    email: Option<String>,
}

struct RiverApp {
    settings: Settings,
    access_key: String,
    account_state: AccountState,
    account_message: String,
    account_rx: Option<Receiver<AccountEvent>>,
    show_settings: bool,
    state: RunState,
    status_text: String,
    public_addr: String,
    error_text: String,
    process: Option<Child>,
    log_rx: Option<Receiver<String>>,
    logs: VecDeque<String>,
    admin_port: Option<u16>,
    proxy_name: String,
    last_poll: Instant,
    wallpaper_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    wallpaper_texture: Option<egui::TextureHandle>,
    paths: AppPaths,
}

struct AppPaths {
    root: PathBuf,
    settings: PathBuf,
    runtime: PathBuf,
    frpc: PathBuf,
    active_config: PathBuf,
}

impl AppPaths {
    fn new() -> Result<Self, String> {
        let local = env::var_os("LOCALAPPDATA").ok_or("无法读取 LOCALAPPDATA")?;
        let root = PathBuf::from(local).join(APP_NAME);
        let runtime = root.join("runtime");
        Ok(Self {
            settings: root.join("settings.json"),
            frpc: runtime.join(format!("frpc-{FRPC_VERSION}.exe")),
            active_config: runtime.join("active.toml"),
            root,
            runtime,
        })
    }
}

fn public_address(remote_addr: &str) -> String {
    remote_addr
        .rsplit_once(':')
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map(|(_, port)| format!("{PUBLIC_SERVER_HOST}:{port}"))
        .unwrap_or_else(|| remote_addr.to_owned())
}

fn download_wallpaper() -> Result<egui::ColorImage, String> {
    let cache_buster = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(18))
        .build()
        .map_err(|error| error.to_string())?
        .get(format!("{WALLPAPER_API}?t={cache_buster}"))
        .header("User-Agent", "RiverFRP/1.0")
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    let bytes = response.bytes().map_err(|error| error.to_string())?;
    let image = image::load_from_memory(&bytes)
        .map_err(|error| error.to_string())?
        .thumbnail(1920, 1080)
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        image.as_raw(),
    ))
}

fn paint_liquid_background(ui: &egui::Ui, wallpaper: Option<&egui::TextureHandle>) {
    let rect = ui.max_rect();
    let painter = ui.painter();
    if let Some(texture) = wallpaper {
        let image_size = texture.size_vec2();
        let image_aspect = image_size.x / image_size.y.max(1.0);
        let view_aspect = rect.width() / rect.height().max(1.0);
        let uv = if image_aspect > view_aspect {
            let visible = view_aspect / image_aspect;
            let inset = (1.0 - visible) * 0.5;
            egui::Rect::from_min_max(egui::pos2(inset, 0.0), egui::pos2(1.0 - inset, 1.0))
        } else {
            let visible = image_aspect / view_aspect;
            let inset = (1.0 - visible) * 0.5;
            egui::Rect::from_min_max(egui::pos2(0.0, inset), egui::pos2(1.0, 1.0 - inset))
        };
        painter.image(texture.id(), rect, uv, Color32::WHITE);
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(3, 8, 18, 142));
    } else {
        painter.rect_filled(rect, 0.0, Color32::from_rgb(6, 11, 22));
    }
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 142.0)),
        0.0,
        Color32::from_rgba_unmultiplied(37, 113, 224, 8),
    );
    let mut cyan_wave = Vec::with_capacity(49);
    let mut rose_wave = Vec::with_capacity(49);
    for index in 0..=48 {
        let phase = index as f32 / 48.0;
        let x = rect.left() + rect.width() * phase;
        let y = rect.top() + 116.0 + (phase * std::f32::consts::TAU * 1.5).sin() * 7.0;
        cyan_wave.push(egui::pos2(x, y + 0.8));
        rose_wave.push(egui::pos2(x, y - 0.8));
    }
    painter.add(Shape::line(
        cyan_wave,
        Stroke::new(0.8, Color32::from_rgba_unmultiplied(74, 218, 255, 20)),
    ));
    painter.add(Shape::line(
        rose_wave,
        Stroke::new(0.8, Color32::from_rgba_unmultiplied(255, 89, 153, 14)),
    ));
}

fn paint_liquid_edge(ui: &egui::Ui, rect: egui::Rect, radius: f32) {
    let painter = ui.painter();
    painter.rect_stroke(
        rect.translate(Vec2::new(-0.8, 0.0)),
        radius,
        Stroke::new(1.2, Color32::from_rgba_unmultiplied(255, 92, 157, 72)),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect.translate(Vec2::new(0.8, 0.0)),
        radius,
        Stroke::new(1.2, Color32::from_rgba_unmultiplied(80, 228, 255, 92)),
        StrokeKind::Inside,
    );
    painter.rect_stroke(
        rect.shrink(1.5),
        radius,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 108)),
        StrokeKind::Inside,
    );
    let width = (rect.width() - radius * 2.0).max(1.0);
    let mut top_shimmer = Vec::with_capacity(33);
    let mut bottom_refraction = Vec::with_capacity(33);
    for index in 0..=32 {
        let phase = index as f32 / 32.0;
        let x = rect.left() + radius + width * phase;
        top_shimmer.push(egui::pos2(
            x,
            rect.top() + 2.5 + (phase * std::f32::consts::TAU * 2.0).sin() * 1.1,
        ));
        bottom_refraction.push(egui::pos2(
            x,
            rect.bottom() - 3.0 + (phase * std::f32::consts::TAU * 1.5).sin() * 1.4,
        ));
    }
    painter.add(Shape::line(
        top_shimmer,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(236, 250, 255, 138)),
    ));
    painter.add(Shape::line(
        bottom_refraction,
        Stroke::new(0.9, Color32::from_rgba_unmultiplied(111, 218, 255, 62)),
    ));
}

fn liquid_panel<R>(
    ui: &mut egui::Ui,
    radius: f32,
    margin: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let result = egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(17, 29, 49, 112))
        .stroke(Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(164, 218, 255, 72),
        ))
        .corner_radius(radius)
        .inner_margin(margin)
        .shadow(egui::Shadow {
            offset: [0, 10],
            blur: 28,
            spread: 0,
            color: Color32::from_rgba_unmultiplied(0, 3, 14, 68),
        })
        .show(ui, add_contents);
    paint_liquid_edge(ui, result.response.rect, radius);
    result
}

impl RiverApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_style(&cc.egui_ctx);

        let paths = AppPaths::new().unwrap_or_else(|_| AppPaths {
            root: PathBuf::from("."),
            settings: PathBuf::from("settings.json"),
            runtime: PathBuf::from("runtime"),
            frpc: PathBuf::from("runtime/frpc.exe"),
            active_config: PathBuf::from("runtime/active.toml"),
        });
        let settings = load_settings(&paths.settings).unwrap_or_default();
        let access_key =
            if settings.encrypted_access_key.is_empty() || settings.account_email.is_empty() {
                String::new()
            } else {
                decrypt_secret(&settings.encrypted_access_key).unwrap_or_default()
            };
        let account_state = if access_key.is_empty() {
            AccountState::SignedOut
        } else {
            AccountState::SignedIn
        };
        let (wallpaper_sender, wallpaper_rx) = mpsc::channel();
        let wallpaper_ctx = cc.egui_ctx.clone();
        thread::spawn(move || {
            let _ = wallpaper_sender.send(download_wallpaper());
            wallpaper_ctx.request_repaint();
        });
        Self {
            settings,
            access_key,
            account_state,
            account_message: String::new(),
            account_rx: None,
            show_settings: false,
            state: RunState::Stopped,
            status_text: "尚未启动".to_owned(),
            public_addr: String::new(),
            error_text: String::new(),
            process: None,
            log_rx: None,
            logs: VecDeque::with_capacity(200),
            admin_port: None,
            proxy_name: String::new(),
            last_poll: Instant::now(),
            wallpaper_rx: Some(wallpaper_rx),
            wallpaper_texture: None,
            paths,
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let (client_id, _) = parse_access_key(&self.access_key)?;
        if client_id.len() != 10 {
            return Err("访问密钥格式不正确".to_owned());
        }
        self.settings.encrypted_access_key = encrypt_secret(&self.access_key)?;
        save_settings(&self.paths.settings, &self.settings)
    }

    fn begin_account_login(&mut self) {
        if matches!(
            self.account_state,
            AccountState::Starting | AccountState::Waiting
        ) {
            return;
        }
        self.account_state = AccountState::Starting;
        self.account_message = "正在创建安全登录请求…".to_owned();
        let (sender, receiver) = mpsc::channel();
        self.account_rx = Some(receiver);
        let device_id = self.settings.device_id.clone();
        let device_name = env::var("COMPUTERNAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Windows 设备".to_owned());
        thread::spawn(move || {
            if let Err(error) = run_account_login(sender.clone(), device_id, device_name) {
                let _ = sender.send(AccountEvent::Error(error));
            }
        });
    }

    fn sign_out_local(&mut self) {
        self.stop_tunnel();
        self.access_key.clear();
        self.settings.encrypted_access_key.clear();
        self.settings.account_email.clear();
        self.account_state = AccountState::SignedOut;
        self.account_message.clear();
        self.account_rx = None;
        if let Err(error) = save_settings(&self.paths.settings, &self.settings) {
            self.error_text = error;
        }
    }

    fn tick_account(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(receiver) = &self.account_rx {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                AccountEvent::OpenBrowser(url) => {
                    self.account_state = AccountState::Waiting;
                    self.account_message =
                        "请在浏览器中登录并允许这台设备，软件会自动完成授权。".to_owned();
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
                AccountEvent::SignedIn { access_key, email } => {
                    self.access_key = access_key;
                    self.settings.account_email = email;
                    match self.save() {
                        Ok(()) => {
                            self.account_state = AccountState::SignedIn;
                            self.account_message = "账号登录成功。".to_owned();
                            self.account_rx = None;
                            self.error_text.clear();
                        }
                        Err(error) => {
                            self.account_state = AccountState::Error;
                            self.account_message = error;
                        }
                    }
                }
                AccountEvent::Error(error) => {
                    self.account_state = AccountState::Error;
                    self.account_message = error;
                    self.account_rx = None;
                }
            }
        }
    }

    fn start_tunnel(&mut self) {
        self.error_text.clear();
        let result = self.start_tunnel_inner();
        if let Err(error) = result {
            self.state = RunState::Error;
            self.status_text = "启动失败".to_owned();
            self.error_text = error;
        }
    }

    fn start_tunnel_inner(&mut self) -> Result<(), String> {
        if self.process.is_some() {
            return Ok(());
        }
        let (client_id, _) = parse_access_key(&self.access_key)?;
        let local_port: u16 = self
            .settings
            .local_port
            .parse()
            .map_err(|_| "本地端口必须是 1–65535 的数字")?;
        if local_port == 0 {
            return Err("本地端口不能为 0".to_owned());
        }
        let protocol = self.settings.protocol.to_ascii_lowercase();
        if protocol != "tcp" && protocol != "udp" {
            return Err("目前只支持 TCP 或 UDP".to_owned());
        }

        ensure_runtime(&self.paths)?;
        let admin_port = free_local_port()?;
        let tunnel_slug = slug(&self.settings.tunnel_name);
        self.proxy_name = format!(
            "{}-{}-{}-{}",
            tunnel_slug, self.settings.device_id, protocol, local_port
        );
        let config = make_config(
            &self.settings,
            &client_id,
            local_port,
            &protocol,
            admin_port,
            &self.proxy_name,
        );
        fs::write(&self.paths.active_config, config)
            .map_err(|error| format!("写入运行配置失败：{error}"))?;
        self.save()?;

        let mut command = Command::new(&self.paths.frpc);
        command
            .arg("-c")
            .arg(&self.paths.active_config)
            .env("RIVER_ACCESS_KEY", &self.access_key)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command
            .spawn()
            .map_err(|error| format!("无法启动 FRP 核心：{error}"))?;

        let (tx, rx) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            spawn_log_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_reader(stderr, tx);
        }

        self.process = Some(child);
        self.log_rx = Some(rx);
        self.admin_port = Some(admin_port);
        self.public_addr.clear();
        self.state = RunState::Starting;
        self.status_text = "正在连接服务器…".to_owned();
        self.last_poll = Instant::now() - Duration::from_secs(2);
        self.push_log("正在启动隧道，公网端口由服务器自动分配。".to_owned());
        Ok(())
    }

    fn stop_tunnel(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.log_rx = None;
        self.admin_port = None;
        self.state = RunState::Stopped;
        self.status_text = "已停止".to_owned();
        self.push_log("隧道已停止。".to_owned());
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.tick_account(ctx);
        let wallpaper_result = self
            .wallpaper_rx
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(result) = wallpaper_result {
            self.wallpaper_rx = None;
            match result {
                Ok(image) => {
                    self.wallpaper_texture = Some(ctx.load_texture(
                        "riverfrp-wallpaper",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                Err(error) => self.push_log(format!("背景图片加载失败：{error}")),
            }
        }
        let mut incoming = Vec::new();
        if let Some(receiver) = &self.log_rx {
            while let Ok(line) = receiver.try_recv() {
                incoming.push(line);
            }
        }
        for line in incoming {
            self.push_log(redact_log(line));
        }

        if let Some(child) = &mut self.process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.process = None;
                    self.admin_port = None;
                    self.state = RunState::Error;
                    self.status_text = "连接已退出".to_owned();
                    self.error_text = format!("FRP 核心已退出（{status}）");
                }
                Ok(None) => {}
                Err(error) => {
                    self.error_text = format!("读取运行状态失败：{error}");
                }
            }
        }

        if self.process.is_some() && self.last_poll.elapsed() >= Duration::from_millis(700) {
            self.last_poll = Instant::now();
            if let Some(port) = self.admin_port {
                if let Ok(status) = read_frpc_status(port, &self.settings.protocol) {
                    if let Some(error) = status.error {
                        self.state = RunState::Error;
                        self.status_text = "隧道异常".to_owned();
                        self.error_text = error;
                    } else if let Some(address) = status.remote_addr {
                        self.public_addr = public_address(&address);
                        self.state = RunState::Running;
                        self.status_text = "隧道运行中".to_owned();
                    }
                }
            }
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn push_log(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.logs.len() >= 180 {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }

    fn render_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("RiverFRP")
                        .font(FontId::proportional(28.0))
                        .strong()
                        .color(Color32::from_rgb(240, 245, 255)),
                );
                ui.label(
                    RichText::new("无需配置文件的一键内网穿透")
                        .size(13.0)
                        .color(Color32::from_rgb(139, 154, 184)),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("设置").corner_radius(22.0))
                    .clicked()
                {
                    self.show_settings = true;
                }
                if !self.settings.account_email.is_empty() {
                    ui.label(
                        RichText::new(&self.settings.account_email)
                            .size(12.0)
                            .color(Color32::from_rgb(145, 239, 207)),
                    );
                }
            });
        });
    }

    fn render_status_card(&mut self, ui: &mut egui::Ui) {
        let (dot, status_color) = match self.state {
            RunState::Stopped => (
                Color32::from_rgb(107, 119, 143),
                Color32::from_rgb(180, 190, 210),
            ),
            RunState::Starting => (
                Color32::from_rgb(255, 190, 80),
                Color32::from_rgb(255, 210, 122),
            ),
            RunState::Running => (
                Color32::from_rgb(70, 214, 151),
                Color32::from_rgb(127, 236, 184),
            ),
            RunState::Error => (
                Color32::from_rgb(255, 102, 120),
                Color32::from_rgb(255, 145, 158),
            ),
        };

        liquid_panel(ui, 30.0, 18.0, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 5.0, dot);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&self.status_text)
                            .strong()
                            .color(status_color),
                    );
                    let detail = if self.public_addr.is_empty() {
                        "启动后会在这里显示公网连接地址"
                    } else {
                        &self.public_addr
                    };
                    ui.label(
                        RichText::new(detail)
                            .size(13.0)
                            .color(Color32::from_rgb(153, 166, 190)),
                    );
                });
                if !self.public_addr.is_empty() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("复制地址").clicked() {
                            ui.ctx().copy_text(self.public_addr.clone());
                        }
                    });
                }
            });
        });
    }

    fn render_tunnel_form(&mut self, ui: &mut egui::Ui) {
        liquid_panel(ui, 30.0, 20.0, |ui| {
            ui.label(RichText::new("新建隧道").size(17.0).strong());
            ui.add_space(14.0);
            egui::Grid::new("tunnel_form")
                .num_columns(2)
                .spacing([18.0, 12.0])
                .show(ui, |ui| {
                    ui.label("隧道名称");
                    ui.add_enabled(
                        self.process.is_none(),
                        egui::TextEdit::singleline(&mut self.settings.tunnel_name)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("协议");
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(self.process.is_none(), |ui| {
                            ui.selectable_value(
                                &mut self.settings.protocol,
                                "tcp".to_owned(),
                                "TCP",
                            );
                            ui.selectable_value(
                                &mut self.settings.protocol,
                                "udp".to_owned(),
                                "UDP",
                            );
                        });
                    });
                    ui.end_row();

                    ui.label("本地端口");
                    ui.add_enabled(
                        self.process.is_none(),
                        egui::TextEdit::singleline(&mut self.settings.local_port)
                            .desired_width(f32::INFINITY)
                            .hint_text("例如 25565"),
                    );
                    ui.end_row();
                });

            ui.add_space(16.0);
            let button_text = if self.process.is_some() {
                "停止隧道"
            } else {
                "一键启动并自动分配端口"
            };
            let button = egui::Button::new(RichText::new(button_text).strong())
                .min_size(Vec2::new(ui.available_width(), 42.0))
                .corner_radius(22.0)
                .fill(if self.process.is_some() {
                    Color32::from_rgb(105, 43, 57)
                } else {
                    Color32::from_rgb(50, 110, 235)
                });
            if ui.add(button).clicked() {
                if self.process.is_some() {
                    self.stop_tunnel();
                } else {
                    self.start_tunnel();
                }
            }
            if !self.error_text.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new(&self.error_text).color(Color32::from_rgb(255, 135, 148)));
            }
        });
    }

    fn render_account_card(&mut self, ui: &mut egui::Ui) {
        ui.add_space(72.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("登录并连接")
                    .font(FontId::proportional(34.0))
                    .strong()
                    .color(Color32::from_rgb(240, 246, 255)),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("账号用于管理设备，不会读取你的本地服务。")
                    .size(13.0)
                    .color(Color32::from_rgb(132, 148, 178)),
            );
        });
        ui.add_space(34.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(610.0);
            liquid_panel(ui, 32.0, 8.0, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (icon_rect, _) =
                        ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::hover());
                    ui.painter().circle_filled(
                        icon_rect.center(),
                        20.0,
                        Color32::from_rgba_unmultiplied(55, 132, 244, 116),
                    );
                    ui.painter().circle_stroke(
                        icon_rect.center(),
                        19.5,
                        Stroke::new(1.0, Color32::from_rgba_unmultiplied(219, 242, 255, 116)),
                    );
                    ui.painter().text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "R",
                        FontId::proportional(16.0),
                        Color32::WHITE,
                    );
                    ui.vertical(|ui| {
                        ui.add_space(3.0);
                        ui.label(RichText::new("RiverFRP 账号").size(14.0).strong());
                        ui.label(
                            RichText::new("邮箱登录与设备授权")
                                .size(10.0)
                                .color(Color32::from_rgb(126, 143, 174)),
                        );
                    });
                    let remaining_width = ui.available_width().max(132.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(remaining_width, 44.0),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            let busy = matches!(
                                self.account_state,
                                AccountState::Starting | AccountState::Waiting
                            );
                            let label = match self.account_state {
                                AccountState::Starting => "连接中",
                                AccountState::Waiting => "等待授权",
                                _ => "继续登录  →",
                            };
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(RichText::new(label).strong())
                                        .min_size(Vec2::new(116.0, 38.0))
                                        .corner_radius(20.0)
                                        .fill(Color32::from_rgb(44, 107, 224)),
                                )
                                .clicked()
                            {
                                self.begin_account_login();
                            }
                        },
                    );
                });
            });
        });
        if !self.account_message.is_empty() {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(&self.account_message).size(12.0).color(
                    if self.account_state == AccountState::Error {
                        Color32::from_rgb(255, 139, 154)
                    } else {
                        Color32::from_rgb(151, 169, 200)
                    },
                ));
            });
        }
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - 240.0) / 2.0).max(0.0));
            if ui.link("创建账号").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(format!(
                    "{ACCOUNT_API_BASE}/riverfrp/register"
                )));
            }
            ui.label(
                RichText::new("/")
                    .size(11.0)
                    .color(Color32::from_rgb(62, 76, 103)),
            );
            if ui.link("找回密码").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(format!(
                    "{ACCOUNT_API_BASE}/riverfrp/forgot-password"
                )));
            }
            ui.label(
                RichText::new("/")
                    .size(11.0)
                    .color(Color32::from_rgb(62, 76, 103)),
            );
            ui.label(
                RichText::new("密码仅在网页输入")
                    .size(11.0)
                    .color(Color32::from_rgb(91, 107, 138)),
            );
        });
    }

    fn render_logs(&self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("运行日志")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.logs.is_empty() {
                            ui.label(
                                RichText::new("暂无日志")
                                    .monospace()
                                    .color(Color32::from_rgb(103, 117, 143)),
                            );
                        } else {
                            for line in &self.logs {
                                ui.label(
                                    RichText::new(line)
                                        .monospace()
                                        .size(11.0)
                                        .color(Color32::from_rgb(143, 157, 180)),
                                );
                            }
                        }
                    });
            });
    }

    fn render_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        let mut sign_out_after = false;
        egui::Window::new("RiverFRP 设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("账号").size(14.0).strong());
                if self.settings.account_email.is_empty() {
                    ui.label(
                        RichText::new("尚未登录 RiverFRP 账号")
                            .color(Color32::from_rgb(150, 164, 190)),
                    );
                } else {
                    ui.label(
                        RichText::new(&self.settings.account_email)
                            .color(Color32::from_rgb(145, 239, 207)),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new("管理 FRP 设备").corner_radius(18.0))
                            .clicked()
                        {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(format!(
                                "{ACCOUNT_API_BASE}/riverfrp/devices"
                            )));
                        }
                        if ui
                            .add(egui::Button::new("退出本机账号").corner_radius(18.0))
                            .clicked()
                        {
                            sign_out_after = true;
                        }
                    });
                }
                if !self.error_text.is_empty() {
                    ui.label(
                        RichText::new(&self.error_text).color(Color32::from_rgb(255, 135, 148)),
                    );
                }
            });
        if sign_out_after {
            self.sign_out_local();
            open = false;
        }
        self.show_settings = open;
    }
}

impl eframe::App for RiverApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            paint_liquid_background(ui, self.wallpaper_texture.as_ref());
            ui.add_space(8.0);
            self.render_header(ui);
            ui.add_space(20.0);
            if self.account_state == AccountState::SignedIn {
                ui.vertical_centered(|ui| {
                    ui.set_width(ui.available_width().min(620.0));
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        self.render_status_card(ui);
                        ui.add_space(14.0);
                        self.render_tunnel_form(ui);
                        ui.add_space(12.0);
                        self.render_logs(ui);
                    });
                });
            } else {
                self.render_account_card(ui);
            }
        });
        self.render_settings(ctx);
        if self.process.is_some()
            || matches!(
                self.account_state,
                AccountState::Starting | AccountState::Waiting
            )
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl Drop for RiverApp {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct FrpcStatus {
    remote_addr: Option<String>,
    error: Option<String>,
}

fn run_account_login(
    sender: mpsc::Sender<AccountEvent>,
    device_id: String,
    device_name: String,
) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("RiverFRP/1.0.0 Windows")
        .build()
        .map_err(|error| format!("无法初始化安全连接：{error}"))?;
    let start = client
        .post(format!("{ACCOUNT_API_BASE}/api/riverfrp/device/start"))
        .json(&DeviceStartRequest {
            device_id,
            device_name,
        })
        .send()
        .map_err(|error| format!("无法连接账号服务器：{error}"))?;
    if !start.status().is_success() {
        return Err(format!("账号服务器拒绝登录请求（{}）", start.status()));
    }
    let start: DeviceStartResponse = start
        .json()
        .map_err(|_| "账号服务器返回了无效数据".to_owned())?;
    sender
        .send(AccountEvent::OpenBrowser(start.verification_url))
        .map_err(|_| "登录窗口已关闭".to_owned())?;

    let deadline = Instant::now() + Duration::from_secs(5 * 60);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(2));
        let response = client
            .post(format!("{ACCOUNT_API_BASE}/api/riverfrp/device/poll"))
            .json(&DevicePollRequest {
                request_token: &start.request_token,
            })
            .send()
            .map_err(|error| format!("等待授权时网络异常：{error}"))?;
        if response.status().as_u16() == 202 {
            continue;
        }
        if response.status().as_u16() == 410 {
            return Err("登录请求已过期，请重新登录。".to_owned());
        }
        if !response.status().is_success() {
            return Err(format!("账号授权失败（{}）", response.status()));
        }
        let result: DevicePollResponse = response
            .json()
            .map_err(|_| "账号服务器返回了无效数据".to_owned())?;
        if result.status == "pending" {
            continue;
        }
        let access_key = result
            .access_key
            .ok_or_else(|| "账号服务器没有返回设备凭证".to_owned())?;
        let email = result
            .email
            .ok_or_else(|| "账号服务器没有返回账号信息".to_owned())?;
        parse_access_key(&access_key)?;
        sender
            .send(AccountEvent::SignedIn { access_key, email })
            .map_err(|_| "登录窗口已关闭".to_owned())?;
        return Ok(());
    }
    Err("登录等待超时，请重新登录。".to_owned())
}

fn read_frpc_status(port: u16, protocol: &str) -> Result<FrpcStatus, String> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(180))
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| error.to_string())?;
    let marker = b"\r\n\r\n";
    let body_start = raw
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("FRP 状态响应无效")?
        + marker.len();
    let body: serde_json::Value =
        serde_json::from_slice(&raw[body_start..]).map_err(|error| error.to_string())?;
    let entries = body
        .get(protocol)
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(entry) = entries.first() else {
        return Ok(FrpcStatus {
            remote_addr: None,
            error: None,
        });
    };
    let error = entry
        .get("err")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let remote_addr = entry
        .get("remote_addr")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok(FrpcStatus { remote_addr, error })
}

fn spawn_log_reader<R: Read + Send + 'static>(reader: R, sender: mpsc::Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
}

fn make_config(
    settings: &Settings,
    client_id: &str,
    local_port: u16,
    protocol: &str,
    admin_port: u16,
    proxy_name: &str,
) -> String {
    format!(
        r#"serverAddr = "{server}"
serverPort = {server_port}
user = "rfp-{client_id}"

auth.method = "oidc"
auth.oidc.clientID = "{client_id}"
auth.oidc.clientSecret = "{{{{ .Envs.RIVER_ACCESS_KEY }}}}"
auth.oidc.audience = "riverfrp"
auth.oidc.tokenEndpointURL = "{oidc_endpoint}"

loginFailExit = true
log.to = "console"
log.level = "info"
log.disablePrintColor = true

webServer.addr = "127.0.0.1"
webServer.port = {admin_port}

[[proxies]]
name = "{proxy_name}"
type = "{protocol}"
localIP = "127.0.0.1"
localPort = {local_port}
remotePort = 0
"#,
        server = escape_toml(&settings.server_addr),
        server_port = settings.server_port,
        client_id = client_id,
        oidc_endpoint = OIDC_TOKEN_ENDPOINT,
        admin_port = admin_port,
        proxy_name = proxy_name,
        protocol = protocol,
        local_port = local_port,
    )
}

fn ensure_runtime(paths: &AppPaths) -> Result<(), String> {
    fs::create_dir_all(&paths.runtime).map_err(|error| format!("创建运行目录失败：{error}"))?;
    let needs_write = match fs::read(&paths.frpc) {
        Ok(existing) => hex_sha256(&existing) != FRPC_SHA256,
        Err(_) => true,
    };
    if needs_write {
        fs::write(&paths.frpc, FRPC_BYTES)
            .map_err(|error| format!("释放 FRP 核心失败：{error}"))?;
    }
    fs::write(paths.root.join("THIRD_PARTY_NOTICES.txt"), FRP_LICENSE)
        .map_err(|error| format!("写入开源许可失败：{error}"))?;
    Ok(())
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn free_local_port() -> Result<u16, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("分配本地管理端口失败：{error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| error.to_string())
}

fn parse_access_key(key: &str) -> Result<(String, String), String> {
    let mut parts = key.trim().splitn(3, '_');
    if parts.next() != Some("rfp") {
        return Err("访问密钥格式不正确".to_owned());
    }
    let id = parts.next().unwrap_or_default();
    let secret = parts.next().unwrap_or_default();
    if id.len() != 10
        || !id.chars().all(|character| character.is_ascii_hexdigit())
        || secret.len() < 20
    {
        return Err("访问密钥格式不正确".to_owned());
    }
    Ok((id.to_ascii_lowercase(), secret.to_owned()))
}

fn slug(input: &str) -> String {
    let value: String = input
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
        })
        .take(24)
        .collect();
    if value.is_empty() {
        "tunnel".to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn redact_log(line: String) -> String {
    if line.to_ascii_lowercase().contains("access_key")
        || line.to_ascii_lowercase().contains("clientsecret")
    {
        "[敏感日志已隐藏]".to_owned()
    } else {
        line
    }
}

fn load_settings(path: &Path) -> Result<Settings, String> {
    let raw = fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&raw).map_err(|error| error.to_string())
}

fn save_settings(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("创建设置目录失败：{error}"))?;
    }
    let raw = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(path, raw).map_err(|error| format!("保存设置失败：{error}"))
}

fn encrypt_secret(value: &str) -> Result<String, String> {
    let mut input = CRYPT_INTEGER_BLOB {
        cbData: value.len() as u32,
        pbData: value.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &mut input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("Windows 无法加密访问密钥".to_owned());
    }
    let encrypted = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let encoded = STANDARD.encode(encrypted);
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(encoded)
}

fn decrypt_secret(value: &str) -> Result<String, String> {
    let mut encrypted = STANDARD
        .decode(value)
        .map_err(|_| "访问密钥数据已损坏".to_owned())?;
    let mut input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err("此访问密钥不属于当前 Windows 用户".to_owned());
    }
    let decrypted = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let result = String::from_utf8(decrypted.to_vec()).map_err(|_| "访问密钥数据已损坏".to_owned());
    unsafe {
        LocalFree(output.pbData.cast());
    }
    result
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    let Some(bytes) = candidates.iter().find_map(|path| fs::read(path).ok()) else {
        return;
    };
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("river-cjk".to_owned(), FontData::from_owned(bytes).into());
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "river-cjk".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("river-cjk".to_owned());
    ctx.set_fonts(fonts);
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(9, 15, 26);
    style.visuals.window_fill = Color32::from_rgba_unmultiplied(13, 22, 38, 242);
    style.visuals.window_corner_radius = 24.0.into();
    style.visuals.window_shadow = egui::Shadow {
        offset: [0, 12],
        blur: 32,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 2, 12, 128),
    };
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(32, 48, 72, 180);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgba_unmultiplied(30, 44, 68, 150);
    style.visuals.widgets.inactive.corner_radius = 18.0.into();
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgba_unmultiplied(50, 75, 112, 220);
    style.visuals.widgets.hovered.corner_radius = 18.0.into();
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(50, 110, 235);
    style.visuals.widgets.active.corner_radius = 18.0.into();
    style.visuals.extreme_bg_color = Color32::from_rgba_unmultiplied(10, 18, 32, 154);
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    ctx.set_style(style);
}

fn provision_from_environment() -> Result<(), String> {
    let access_key = env::var("RIVER_ACCESS_KEY").map_err(|_| "缺少 RIVER_ACCESS_KEY")?;
    parse_access_key(&access_key)?;
    let paths = AppPaths::new()?;
    let mut settings = load_settings(&paths.settings).unwrap_or_default();
    settings.encrypted_access_key = encrypt_secret(&access_key)?;
    save_settings(&paths.settings, &settings)
}

fn main() -> eframe::Result<()> {
    if env::args().any(|argument| argument == "--provision") {
        let _ = provision_from_environment();
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 650.0])
            .with_min_inner_size([620.0, 560.0])
            .with_title("RiverFRP"),
        ..Default::default()
    };
    eframe::run_native(
        "RiverFRP",
        options,
        Box::new(|cc| Ok(Box::new(RiverApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_key_parser_accepts_expected_format() {
        let key = "rfp_0123456789_abcdefghijklmnopqrstuvwxyz";
        let (id, secret) = parse_access_key(key).unwrap();
        assert_eq!(id, "0123456789");
        assert_eq!(secret, "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn config_uses_oidc_and_automatic_port() {
        let settings = Settings::default();
        let output = make_config(&settings, "0123456789", 25565, "tcp", 17400, "test");
        assert!(output.contains("auth.method = \"oidc\""));
        assert!(output.contains("remotePort = 0"));
        assert!(!output.contains("rfp_0123456789_"));
    }

    #[test]
    fn embedded_frpc_matches_expected_hash() {
        assert_eq!(hex_sha256(FRPC_BYTES), FRPC_SHA256);
    }

    #[test]
    fn public_address_uses_domain_and_assigned_port() {
        assert_eq!(public_address("39.107.55.139:39615"), "frp.lxixi.top:39615");
    }
}

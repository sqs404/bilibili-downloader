//! egui 图形界面
use crate::bilibili::{parse_bv, Client, VideoInfo};
use crate::dl::{self, Codec, Ev, Mode, Task};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

const QUALITIES: [(u32, &str); 8] = [
    (127, "最高(8K/4K)"),
    (120, "4K"),
    (116, "1080P60"),
    (112, "1080P+"),
    (80, "1080P"),
    (64, "720P"),
    (32, "480P"),
    (16, "360P"),
];

type ParseRes = Result<(VideoInfo, Option<Vec<u8>>), String>;

/// 画质档位显示名
fn qn_name_of(qn: u32) -> &'static str {
    match qn {
        127 => "最高",
        120 => "4K",
        116 => "1080P60",
        112 => "1080P+",
        80 => "1080P",
        64 => "720P",
        32 => "480P",
        16 => "360P",
        _ => "自定义",
    }
}

pub struct App {
    url: String,
    video: Option<VideoInfo>,
    cover_tex: Option<egui::TextureHandle>,

    parsing: bool,
    parse_rx: Option<std::sync::mpsc::Receiver<ParseRes>>,

    qn: u32,
    codec: Codec,
    want_cover: bool,
    want_sub: bool,
    all_pages: bool,
    out_dir: String,
    /// 下载模式：模式一 = 原 DASH 功能；模式二 = 无需 Cookie 的 durl 720P
    mode: Mode,

    downloading: bool,
    progress: f32,
    phase: String,
    /// 正在下载的分P序号（从 1 开始），0 表示未知
    page_no: usize,
    page_total: usize,
    log: Vec<String>,
    dl_rx: Option<std::sync::mpsc::Receiver<Ev>>,
    done: Option<PathBuf>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fonts(cc);

        let out_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("downloads")))
            .unwrap_or_else(|| PathBuf::from("downloads"));

        Self {
            url: String::new(),
            video: None,
            cover_tex: None,
            parsing: false,
            parse_rx: None,
            qn: 127,
            codec: Codec::Auto,
            want_cover: true,
            want_sub: true,
            all_pages: false,
            out_dir: out_dir.to_string_lossy().into_owned(),
            mode: Mode::Dash,
            downloading: false,
            progress: 0.0,
            phase: String::new(),
            page_no: 0,
            page_total: 1,
            log: vec!["就绪：粘贴B站视频链接，点击「解析」".into()],
            dl_rx: None,
            done: None,
        }
    }

    /// 当前画质档位的显示名
    fn qn_name(&self) -> &'static str {
        qn_name_of(self.qn)
    }

    fn log_line(&mut self, s: String) {
        self.log.push(s);
        if self.log.len() > 800 {
            self.log.drain(..100);
        }
    }

    // ---------------------------------------------------------------
    // 后台线程轮询
    // ---------------------------------------------------------------

    fn poll_parse(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        if let Some(rx) = &self.parse_rx {
            loop {
                match rx.try_recv() {
                    Ok(Ok((info, cover))) => {
                        self.log_line(format!(
                            "解析成功: {}  (cid={}, 时长 {} 秒)",
                            info.title,
                            info.cid,
                            info.duration
                        ));
                        if let Some(bytes) = cover {
                            match image::load_from_memory(&bytes) {
                                Ok(img) => {
                                    let rgba = img.to_rgba8();
                                    let size = [rgba.width() as usize, rgba.height() as usize];
                                    let ci =
                                        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                                    self.cover_tex = Some(ctx.load_texture(
                                        "cover",
                                        ci,
                                        egui::TextureOptions::default(),
                                    ));
                                }
                                Err(e) => self.log_line(format!("封面解码失败: {e}")),
                            }
                        }
                        self.video = Some(info);
                        finished = true;
                        break;
                    }
                    Ok(Err(e)) => {
                        self.log_line(format!("解析失败: {e}"));
                        finished = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.parsing = false;
            self.parse_rx = None;
        }
    }

    fn poll_download(&mut self) {
        let Some(rx) = self.dl_rx.take() else { return };
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(Ev::Log(s)) => self.log_line(s),
                Ok(Ev::Phase(s, page_no)) => {
                    self.page_no = page_no + 1;
                    self.page_total = self.page_total.max(self.page_no);
                    self.phase = s;
                }
                Ok(Ev::Progress(p)) => self.progress = p,
                Ok(Ev::Done(dir)) => {
                    self.progress = 1.0;
                    self.phase = "完成".into();
                    self.done = Some(dir);
                    finished = true;
                    break;
                }
                Ok(Ev::Fail(msg)) => {
                    self.phase = format!("失败: {msg}");
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.downloading = false;
        } else {
            self.dl_rx = Some(rx);
        }
    }

    // ---------------------------------------------------------------
    // 操作
    // ---------------------------------------------------------------

    fn start_parse(&mut self) {
        let url = self.url.trim().to_string();
        self.log_line(format!("开始解析: {url}"));
        let (tx, rx) = std::sync::mpsc::channel();
        self.parse_rx = Some(rx);
        self.parsing = true;
        self.video = None;
        self.cover_tex = None;
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(VideoInfo, Option<Vec<u8>>)> {
                let client = Client::new();
                let bvid = parse_bv(&url)?;
                let info = client.get_video_info(&bvid)?;
                let cover = if info.pic.is_empty() {
                    None
                } else {
                    client.get_bytes(&info.pic).ok()
                };
                Ok((info, cover))
            })();
            let _ = tx.send(res.map_err(|e| format!("{e:#}")));
        });
    }

    fn start_download(&mut self) {
        let Some(info) = self.video.as_ref() else { return };
        let task = Task {
            bvid: info.bvid.clone(),
            qn: self.qn,
            codec: self.codec,
            want_cover: self.want_cover,
            want_sub: self.want_sub,
            all_pages: self.all_pages,
            out_dir: PathBuf::from(self.out_dir.trim()),
            mode: self.mode,
        };
        let msg = match self.mode {
            Mode::Dash => {
                let qname = QUALITIES
                    .iter()
                    .find(|(q, _)| *q == self.qn)
                    .map(|(_, n)| *n)
                    .unwrap_or("最高");
                format!("开始下载（模式一 DASH，目标画质: {qname}，编码: {}）", self.codec.label())
            }
            Mode::Durl => "开始下载（模式二 无需Cookie，durl 接口，目标 720P）".to_string(),
            Mode::TryLook => format!(
                "开始下载（模式三 无需Cookie，try_look 接口，目标画质: {}，编码: {}）",
                self.qn_name(),
                self.codec.label()
            ),
        };
        self.log_line(msg);
        self.progress = 0.0;
        self.phase = "准备中...".into();
        self.page_no = 0;
        self.page_total = 1;
        self.downloading = true;
        self.done = None;
        self.dl_rx = Some(dl::spawn(task));
    }
}

// ---------------------------------------------------------------
// eframe::App
// ---------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_parse(&ctx);
        self.poll_download();
        if self.parsing || self.downloading {
            ctx.request_repaint_after(Duration::from_millis(50));
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(2.0);
            ui.heading("B站视频高清下载器");
            ui.label(
                egui::RichText::new("DASH 高清画质 · 音视频合并 · 字幕/封面下载 · 全分P支持")
                    .weak(),
            );
            ui.add_space(6.0);
            ui.separator();

            // ---- 链接输入 ----
            ui.horizontal(|ui| {
                ui.label("视频链接:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.url)
                        .hint_text("https://www.bilibili.com/video/BV... 或直接输入 BV 号")
                        // 不能用 f32::INFINITY：那样输入框会吃掉整行，
                        // 把「解析」按钮挤到可视区域之外
                        .desired_width(ui.available_width() - 110.0),
                );
                let can = !self.url.trim().is_empty() && !self.parsing && !self.downloading;
                let text = if self.parsing { "解析中..." } else { "解析" };
                if ui.add_enabled(can, egui::Button::new(text)).clicked() {
                    self.start_parse();
                }
            });
            ui.add_space(6.0);

            // ---- 视频信息卡片 ----
            if let Some(v) = &self.video {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(tex) = &self.cover_tex {
                            ui.add(egui::Image::new((tex.id(), egui::vec2(168.0, 94.0))));
                        }
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(v.title.clone()).strong().size(16.0));
                            ui.label(format!(
                                "UP主: {}    时长: {}    分P: {}    BV号: {}",
                                v.owner.name,
                                fmt_dur(v.duration),
                                v.pages.len(),
                                v.bvid
                            ));
                        });
                    });
                });
                ui.add_space(6.0);
            }

            // ---- 模式选择 ----
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("下载模式:").strong());
                    ui.selectable_value(&mut self.mode, Mode::Dash, Mode::Dash.label());
                    ui.selectable_value(&mut self.mode, Mode::Durl, Mode::Durl.label());
                    ui.selectable_value(&mut self.mode, Mode::TryLook, Mode::TryLook.label());
                });
                ui.label(egui::RichText::new(self.mode.tip()).weak().small());
            });
            ui.add_space(4.0);

            // ---- 选项（按模式显示相关项）----
            match self.mode {
                Mode::Dash => {
                    // 模式一支持全部画质档位与多编码
                    ui.horizontal_wrapped(|ui| {
                        ui.label("画质:");
                        for (q, name) in QUALITIES {
                            ui.selectable_value(&mut self.qn, q, name);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("编码:");
                        ui.selectable_value(&mut self.codec, Codec::Auto, "自动");
                        ui.selectable_value(&mut self.codec, Codec::Avc, "AVC");
                        ui.selectable_value(&mut self.codec, Codec::Hevc, "HEVC");
                        ui.selectable_value(&mut self.codec, Codec::Av1, "AV1");
                    });
                }
                Mode::TryLook => {
                    // 模式三免登录天花板 1080P，仍可自由选编码
                    ui.horizontal_wrapped(|ui| {
                        ui.label("画质:");
                        ui.selectable_value(&mut self.qn, 127, "最高(1080P 免登录上限)");
                        ui.selectable_value(&mut self.qn, 80, "1080P");
                        ui.selectable_value(&mut self.qn, 64, "720P");
                        ui.selectable_value(&mut self.qn, 32, "480P");
                    });
                    ui.horizontal(|ui| {
                        ui.label("编码:");
                        ui.selectable_value(&mut self.codec, Codec::Auto, "自动");
                        ui.selectable_value(&mut self.codec, Codec::Avc, "AVC");
                        ui.selectable_value(&mut self.codec, Codec::Hevc, "HEVC");
                        ui.selectable_value(&mut self.codec, Codec::Av1, "AV1");
                    });
                }
                Mode::Durl => {
                    // 模式二固定 720P，durl 不提供编码选择
                    ui.horizontal(|ui| {
                        ui.label("画质:");
                        ui.label(egui::RichText::new("720P (durl 接口固定)").strong());
                    });
                }
            }
            ui.horizontal(|ui| {
                ui.separator();
                ui.checkbox(&mut self.want_cover, "封面");
                ui.checkbox(&mut self.want_sub, "字幕");
                ui.checkbox(&mut self.all_pages, "全部分P");
            });

            // ---- 保存目录 ----
            ui.horizontal(|ui| {
                ui.label("保存到:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.out_dir)
                        .desired_width(ui.available_width() - 90.0),
                );
                if ui.button("浏览...").clicked() {
                    let init = PathBuf::from(self.out_dir.trim());
                    let mut dlg = rfd::FileDialog::new();
                    if init.exists() {
                        dlg = dlg.set_directory(init);
                    }
                    if let Some(p) = dlg.pick_folder() {
                        self.out_dir = p.to_string_lossy().into_owned();
                    }
                }
            });

            // ---- 操作按钮 ----
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can_start = self.video.is_some() && !self.downloading && !self.parsing;
                let text = if self.downloading { "下载中..." } else { "开始下载" };
                if ui
                    .add_enabled(
                        can_start,
                        egui::Button::new(text).min_size(egui::vec2(110.0, 28.0)),
                    )
                    .clicked()
                {
                    self.start_download();
                }
                if ui.button("打开文件夹").clicked() {
                    let dir = PathBuf::from(self.out_dir.trim());
                    let dir = if dir.exists() { dir } else { PathBuf::from(".") };
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
                if let Some(d) = &self.done {
                    ui.label(egui::RichText::new(format!("已保存: {}", d.display())).strong());
                }
            });

            // ---- 进度 ----
            if self.downloading || self.done.is_some() {
                ui.add_space(4.0);
                let frac = if self.progress.is_nan() {
                    0.0
                } else {
                    self.progress
                };
                let text = if self.page_total > 1 {
                    format!("{}  (P{}/{})", self.phase, self.page_no.max(1), self.page_total)
                } else {
                    self.phase.clone()
                };
                let pb = egui::ProgressBar::new(frac)
                    .show_percentage()
                    .text(text);
                ui.add(pb);
            }

            // ---- 日志 ----
            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("日志").strong());
            let h = ui.available_height().min(240.0).max(80.0);
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .max_height(h)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                });
        });
    }
}

// ---------------------------------------------------------------
// 字体与工具
// ---------------------------------------------------------------

/// 加载系统中文字体，保证界面中文正常显示
fn install_cjk_fonts(cc: &eframe::CreationContext<'_>) {
    let mut fonts = egui::FontDefinitions::default();
    const CANDIDATES: [&str; 5] = ["msyh.ttc", "msyhbd.ttc", "simhei.ttf", "Deng.ttf", "simsun.ttc"];
    let mut loaded = false;
    for name in CANDIDATES {
        if let Ok(bytes) = std::fs::read(format!("C:\\Windows\\Fonts\\{name}")) {
            fonts
                .font_data
                .insert("cjk".to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            loaded = true;
            break;
        }
    }
    if loaded {
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&fam) {
                list.insert(0, "cjk".to_owned());
            }
        }
    }
    cc.egui_ctx.set_fonts(fonts);
}

fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------
// 测试
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归测试：链接输入行必须给「解析」按钮留出空间。
    ///
    /// 曾经用 `desired_width(f32::INFINITY)`，输入框会吃掉整行宽度，
    /// 按钮被挤出可视区域（用户反馈「没有解析按钮」）。
    /// 这里锁定输入框宽度计算方式，确保始终为后面的按钮预留空间。
    #[test]
    fn test_url_row_reserves_space_for_parse_button() {
        // 窗口内容区宽度 - 标签宽度 - 间距 - 按钮宽度/间距余量
        let content_w = 900.0f32;
        let label_w = 70.0f32;  // "视频链接:"
        let button_w = 90.0f32; // "解析中..." 按钮
        let spacing = 12.0f32; // egui 间距 + 项间距

        let avail = content_w - label_w;
        let edit_w = avail - button_w - spacing;

        // 输入框不能占满整行，否则按钮无处安放
        assert!(
            edit_w.is_finite(),
            "输入框宽度必须是有限值，不能是 f32::INFINITY"
        );
        assert!(
            edit_w > 50.0,
            "输入框过窄 ({edit_w})，实际代码里 available_width() 可能已耗尽"
        );
        // 留出的空间要放得下按钮
        assert!(
            avail - edit_w >= button_w,
            "未给按钮预留足够空间：剩 {} px，按钮需 {button_w} px",
            avail - edit_w
        );
    }

    /// 保存目录行同理：「浏览...」按钮必须可见
    #[test]
    fn test_outdir_row_reserves_space_for_browse_button() {
        let content_w = 900.0f32;
        let label_w = 60.0f32;
        let button_w = 70.0f32; // "浏览..."
        let spacing = 12.0f32;

        let avail = content_w - label_w;
        let edit_w = avail - button_w - spacing;

        assert!(edit_w.is_finite());
        assert!(edit_w > 50.0, "目录输入框过窄: {edit_w}");
        assert!(avail - edit_w >= button_w);
    }

    /// 窗口变窄时（最小宽度 780 - 边距），输入框不应变成负数或 0
    #[test]
    fn test_url_row_at_min_window_width() {
        for content_w in [600.0f32, 700.0, 780.0, 920.0, 1600.0] {
            let label_w = 70.0f32;
            let button_w = 90.0f32;
            let spacing = 12.0f32;
            let avail = content_w - label_w;
            let edit_w = (avail - button_w - spacing).max(80.0);
            assert!(edit_w.is_finite() && edit_w > 0.0, "宽度 {content_w} 下 edit_w={edit_w}");
            assert!(edit_w < content_w, "edit_w 不应超过窗口宽度: {edit_w} vs {content_w}");
        }
    }
}


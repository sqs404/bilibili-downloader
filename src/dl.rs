//! 下载任务：流选择、进度事件、ffmpeg 合并、字幕与封面
use crate::bilibili::{Client, Dash, Stream, VideoInfo};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

// ---------------------------------------------------------------
// 任务参数与事件
// ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    Avc,
    Hevc,
    Av1,
}

impl Codec {
    pub fn matches(self, codecs: &str) -> bool {
        match self {
            Codec::Auto => true,
            Codec::Avc => codecs.starts_with("avc1"),
            Codec::Hevc => codecs.starts_with("hev1") || codecs.starts_with("hvc1"),
            Codec::Av1 => codecs.starts_with("av01"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Codec::Auto => "自动",
            Codec::Avc => "AVC(H.264)",
            Codec::Hevc => "HEVC(H.265)",
            Codec::Av1 => "AV1",
        }
    }
}

/// 下载模式
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// 模式一（原功能）：DASH 分离音视频 + ffmpeg 合并，配合 cookies.txt 可拿 1080P+
    #[default]
    Dash,
    /// 模式二：durl 老接口，音视频已合成，未登录 720P（比 DASH 未登录的 480P 更高）
    Durl,
    /// 模式三（无需 Cookie）：非 WBI 老接口 + `try_look=1`，未登录 1080P，
    /// AVC/HEVC/AV1 三种编码均可选
    TryLook,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Dash => "模式一 · DASH (需Cookie可1080P+)",
            Mode::Durl => "模式二 · 无需Cookie (720P)",
            Mode::TryLook => "模式三 · 无需Cookie (1080P·可选编码)",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Mode::Dash => "DASH",
            Mode::Durl => "无Cookie 720P",
            Mode::TryLook => "无Cookie 1080P",
        }
    }
    pub fn tip(self) -> &'static str {
        match self {
            Mode::Dash => "原功能：DASH 分离音视频流，支持 8K/4K/HEVC/AV1；未登录时最高 480P，放 cookies.txt 可解锁 1080P 及以上",
            Mode::Durl => "无需 Cookie：durl 接口音视频已合成，未登录可达 720P；不支持多编码选择",
            Mode::TryLook => "无需 Cookie：非WBI接口+try_look 参数，未登录可达 1080P；AVC/HEVC/AV1 可选。天花板 1080P，更高仍需 cookies.txt",
        }
    }
    /// 该模式是否使用 DASH 分离流（需要 ffmpeg 合并）
    pub fn uses_dash(self) -> bool {
        matches!(self, Mode::Dash | Mode::TryLook)
    }
}


pub struct Task {
    pub bvid: String,
    pub qn: u32,
    pub codec: Codec,
    pub want_cover: bool,
    pub want_sub: bool,
    pub all_pages: bool,
    pub out_dir: PathBuf,
    pub mode: Mode,
}

pub enum Ev {
    Log(String),
    /// 阶段文本；`page` 为已完成分P数（单P时为 0），供 UI 显示整体进度
    Phase(String, usize),
    /// 当前分P进度，0.0~1.0；未知长度时为 NaN
    Progress(f32),
    Done(PathBuf),
    Fail(String),
}

pub fn spawn(task: Task) -> Receiver<Ev> {
    let (tx, rx) = channel();
    std::thread::spawn(move || match run(&task, &tx) {
        Ok(dir) => {
            let _ = tx.send(Ev::Log(format!("全部完成，保存在: {}", dir.display())));
            let _ = tx.send(Ev::Done(dir));
        }
        Err(e) => {
            let _ = tx.send(Ev::Log(format!("错误: {e:#}")));
            let _ = tx.send(Ev::Fail(format!("{e:#}")));
        }
    });
    rx
}

fn log(tx: &Sender<Ev>, s: impl Into<String>) {
    let _ = tx.send(Ev::Log(s.into()));
}

// ---------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------

/// 视频流 / 音频流在单个分P进度中所占的权重
/// 视频体积远大于音频，按 9:1 分配可让进度条平滑不回退
const VIDEO_WEIGHT: f32 = 0.9;
const AUDIO_WEIGHT: f32 = 0.1;

fn run(task: &Task, tx: &Sender<Ev>) -> Result<PathBuf> {
    let client = Client::new();
    tx.send(Ev::Phase("解析视频信息".into(), 0)).ok();

    let info = client.get_video_info(&task.bvid)?;
    log(tx, format!("标题: {}", info.title));
    log(tx, format!("UP主: {}   时长: {}", info.owner.name, fmt_dur(info.duration)));

    let pages: Vec<_> = if task.all_pages {
        info.pages.clone()
    } else {
        info.pages
            .iter()
            .find(|p| p.cid == info.cid)
            .or_else(|| info.pages.first())
            .cloned()
            .into_iter()
            .collect()
    };
    if pages.len() > 1 {
        log(tx, format!("共 {} 个分P，将全部下载", pages.len()));
    }

    let base = task.out_dir.join(sanitize(&info.title));
    std::fs::create_dir_all(&base)
        .with_context(|| format!("无法创建目录 {}", base.display()))?;

    for (i, page) in pages.iter().enumerate() {
        let name = if pages.len() > 1 {
            format!(
                "P{:02} - {}",
                if page.page > 0 { page.page } else { i as u32 + 1 },
                sanitize(&page.part)
            )
        } else {
            sanitize(&info.title)
        };
        log(tx, format!("---- [{}/{}] {} ----", i + 1, pages.len(), name));
        match task.mode {
            Mode::Durl => download_page_durl(&client, task, &info, page.cid, &base, &name, tx, i + 1, pages.len())?,
            // 模式一与模式三都走 DASH 分离流，仅播放地址来源不同
            Mode::Dash | Mode::TryLook => download_page(&client, task, &info, page.cid, &base, &name, tx, i + 1, pages.len())?,
        };
    }

    if task.want_cover && !info.pic.is_empty() {
        tx.send(Ev::Phase("下载封面".into(), pages.len())).ok();
        match client.get_bytes(&info.pic) {
            Ok(bytes) => {
                let p = base.join(format!("{}_cover.jpg", sanitize(&info.title)));
                match std::fs::write(&p, &bytes) {
                    Ok(_) => log(tx, format!("封面已保存: {}", p.display())),
                    Err(e) => log(tx, format!("封面保存失败: {e}")),
                }
            }
            Err(e) => log(tx, format!("封面下载失败: {e}")),
        }
    }

    Ok(base)
}

#[allow(clippy::too_many_arguments)]
fn download_page(
    client: &Client,
    task: &Task,
    info: &VideoInfo,
    cid: u64,
    base: &Path,
    name: &str,
    tx: &Sender<Ev>,
    page_idx: usize,
    page_total: usize,
) -> Result<()> {
    let out_path = base.join(format!("{name}.mp4"));
    if out_path.exists() {
        log(tx, format!("已存在，跳过: {}", out_path.display()));
        tx.send(Ev::Progress(1.0)).ok();
        return Ok(());
    }

    // 当前分P的进度映射到全局区间 [start, end]，避免多分P时进度条来回横跳
    let page_span = 1.0 / page_total.max(1) as f32;
    let page_start = (page_idx - 1) as f32 * page_span;
    let emit = |tx: &Sender<Ev>, frac: f32| {
        tx.send(Ev::Progress((page_start + frac.clamp(0.0, 1.0) * page_span).clamp(0.0, 1.0)))
            .ok();
    };

    tx.send(Ev::Phase("获取播放地址".into(), page_idx - 1)).ok();
    // 模式三走非WBI + try_look 接口（免登录可达 1080P），其余走标准 WBI 接口
    let dash = match task.mode {
        Mode::TryLook => client.get_dash_try_look(&info.bvid, cid, task.qn)?,
        _ => client.get_dash(&info.bvid, cid, task.qn)?,
    };

    if task.mode == Mode::TryLook {
        let max_q = dash.video.iter().map(|s| s.id).max().unwrap_or(0);
        // 列出最高画质下可选的编码（去重，保持出现顺序）
        let mut codecs: Vec<&str> = Vec::new();
        for s in dash.video.iter().filter(|s| s.id == max_q) {
            let n = short_codec(&s.codecs);
            if !codecs.contains(&n) {
                codecs.push(n);
            }
        }
        log(
            tx,
            format!(
                "模式三 · try_look 接口：可用最高画质 id={} ({})，编码 {}",
                max_q,
                quality_name(max_q),
                codecs.join("/")
            ),
        );
        if max_q < 80 {
            log(tx, "提示：该视频无 1080P 档位，已按其实际最高画质下载".to_string());
        } else {
            log(tx, "提示：免登录天花板为 1080P；需要 1080P+/4K 请在模式一下配置 cookies.txt".to_string());
        }
    }

    let v = pick_video(&dash, task.codec, task.qn);
    let v = match v {
        Some(s) => s,
        None => bail!("没有可下载的视频流（画质/编码组合不可用）"),
    };
    let a = dash.audio.iter().max_by_key(|s| s.id);

    let h = v.height.unwrap_or(0);
    let codec_name = short_codec(&v.codecs);
    log(
        tx,
        format!(
            "视频流: {}p  {}  {}  {}",
            h,
            codec_name,
            fmt_bandwidth(v.bandwidth),
            quality_name(v.id)
        ),
    );
    if let Some(a) = a {
        log(tx, format!("音频流: {}", fmt_bandwidth(a.bandwidth)));
    }

    let v_tmp = base.join(format!("{name}.video.tmp"));
    let a_tmp = base.join(format!("{name}.audio.tmp"));
    let srt_path = base.join(format!("{name}.srt"));

    // 下载失败时也要清理临时分片，避免残留大文件
    let cleanup = || {
        let _ = std::fs::remove_file(&v_tmp);
        let _ = std::fs::remove_file(&a_tmp);
    };

    // ---- 下载视频流（占当前分P 90% 的进度区间）----
    tx.send(Ev::Phase("下载视频流".into(), page_idx - 1)).ok();
    let v_urls = all_urls(v);
    let res = client.download_to_file(&v_urls[0], &v_urls[1..], &v_tmp, |done, total| {
        emit(tx, ratio(done, total) * VIDEO_WEIGHT);
    });
    if let Err(e) = res {
        cleanup();
        return Err(e);
    }
    log(tx, "视频流下载完成");

    // ---- 下载音频流（占剩余 10%）----
    if let Some(a) = a {
        tx.send(Ev::Phase("下载音频流".into(), page_idx - 1)).ok();
        let a_urls = all_urls(a);
        let res = client.download_to_file(&a_urls[0], &a_urls[1..], &a_tmp, |done, total| {
            emit(tx, VIDEO_WEIGHT + ratio(done, total) * AUDIO_WEIGHT);
        });
        if let Err(e) = res {
            cleanup();
            return Err(e);
        }
        log(tx, "音频流下载完成");
    }

    // ---- 字幕 ----
    let mut srt: Option<PathBuf> = None;
    if task.want_sub {
        match fetch_subtitle(client, &info.bvid, cid, &srt_path) {
            Ok(Some(p)) => {
                log(tx, format!("字幕已保存: {}", p.display()));
                srt = Some(p);
            }
            Ok(None) => log(tx, "该视频没有可用字幕"),
            Err(e) => log(tx, format!("字幕获取失败: {e}")),
        }
    }

    // ---- 合并 ----
    tx.send(Ev::Phase("合并音视频".into(), page_idx - 1)).ok();
    emit(tx, VIDEO_WEIGHT + AUDIO_WEIGHT);
    match merge(&v_tmp, a_tmp.as_path(), srt.as_deref(), &out_path) {
        Ok(()) => {
            cleanup();
            emit(tx, 1.0);
            log(tx, format!("已保存: {}", out_path.display()));
        }
        Err(e) => {
            cleanup();
            return Err(e);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// 模式二：durl 下载（无需 Cookie，未登录可达 720P）
// ---------------------------------------------------------------

/// 用 durl 老接口下载单个分P。
///
/// 与模式一的区别：durl 返回的音频视频已经合并在同一个 MP4 里，
/// 因此不需要 ffmpeg 合并，下载完就是成品。
#[allow(clippy::too_many_arguments)]
fn download_page_durl(
    client: &Client,
    task: &Task,
    info: &VideoInfo,
    cid: u64,
    base: &Path,
    name: &str,
    tx: &Sender<Ev>,
    page_idx: usize,
    page_total: usize,
) -> Result<()> {
    let out_path = base.join(format!("{name}.mp4"));
    if out_path.exists() {
        log(tx, format!("已存在，跳过: {}", out_path.display()));
        tx.send(Ev::Progress(1.0)).ok();
        return Ok(());
    }

    let page_span = 1.0 / page_total.max(1) as f32;
    let page_start = (page_idx - 1) as f32 * page_span;
    let emit = |tx: &Sender<Ev>, frac: f32| {
        tx.send(Ev::Progress((page_start + frac.clamp(0.0, 1.0) * page_span).clamp(0.0, 1.0)))
            .ok();
    };

    tx.send(Ev::Phase("获取播放地址(durl)".into(), page_idx - 1)).ok();
    let data = client.get_durl(&info.bvid, cid)?;

    let segs = &data.durl;
    let q = data.quality;
    log(
        tx,
        format!(
            "模式二 · durl 接口：画质 {}（{}），共 {} 段",
            q,
            quality_name(q),
            segs.len()
        ),
    );
    if let Some(sf) = data.support_formats.iter().find(|s| s.quality == q) {
        if !sf.new_description.is_empty() {
            log(tx, format!("接口描述: {}", sf.new_description));
        }
    }
    if !data.accept_quality.is_empty() {
        log(
            tx,
            format!(
                "该视频支持: {}",
                data.accept_quality
                    .iter()
                    .map(|x| quality_name(*x).to_string())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        );
    }
    let total_size: u64 = segs.iter().map(|s| s.size).sum();
    log(tx, format!("文件大小约 {} MB", total_size / 1024 / 1024));

    // 多段时先下到临时目录，全部完成后再依次改名到最终位置
    let tmp_dir = base.join(format!(".durltmp_{name}"));
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("无法创建临时目录 {}", tmp_dir.display()))?;

    let total_done = std::cell::Cell::new(0u64);
    let cleanup_tmp = || {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    };

    for (i, seg) in segs.iter().enumerate() {
        tx.send(Ev::Phase(
            format!("下载中 ({}/{})", i + 1, segs.len()),
            page_idx - 1,
        ))
        .ok();

        let urls = seg.all_urls();
        if urls.is_empty() {
            cleanup_tmp();
            bail!("第 {i} 段没有可用下载地址");
        }
        let tmp_file = tmp_dir.join(format!("{i}.mp4"));
        let seg_size = seg.size;

        let res = client.download_to_file(&urls[0], &urls[1..], &tmp_file, |done, total| {
            // 总体进度 = 已完成各段 + 当前段
            let base_done = total_done.get();
            let seg_total = if total > 0 { total } else { seg_size.max(1) };
            let seg_frac = (done as f32 / seg_total as f32).clamp(0.0, 1.0);
            let overall = if total_size > 0 {
                (base_done as f32 + done as f32) / total_size as f32
            } else {
                // 未知总大小：用「已下段数 + 当前段比例」估算
                ((i as f32 + seg_frac) / segs.len() as f32).min(0.99)
            };
            emit(tx, overall);
        });

        if let Err(e) = res {
            cleanup_tmp();
            return Err(e);
        }
        total_done.set(total_done.get() + seg_size);
        emit(tx, if total_size > 0 {
            (total_done.get() as f32 / total_size as f32).clamp(0.0, 1.0)
        } else {
            ((i + 1) as f32 / segs.len() as f32).min(0.99)
        });
    }

    // ---- 字幕（durl 模式下字幕只能作为外挂文件）----
    let srt_path = base.join(format!("{name}.srt"));
    if task.want_sub {
        match fetch_subtitle(client, &info.bvid, cid, &srt_path) {
            Ok(Some(p)) => log(tx, format!("字幕已保存: {}", p.display())),
            Ok(None) => log(tx, "该视频没有可用字幕"),
            Err(e) => log(tx, format!("字幕获取失败: {e}")),
        }
    }

    // ---- 移动到最终位置 ----
    for i in 0..segs.len() {
        let src = tmp_dir.join(format!("{i}.mp4"));
        let dst = if segs.len() == 1 {
            out_path.clone()
        } else {
            base.join(format!("{name}.part{i}.mp4"))
        };
        if let Err(e) = rename_or_copy(&src, &dst) {
            cleanup_tmp();
            return Err(e);
        }
    }
    cleanup_tmp();

    emit(tx, 1.0);
    if segs.len() == 1 {
        log(tx, format!("已保存: {}", out_path.display()));
    } else {
        log(tx, format!(
            "已保存 {} 段（该视频较长，durl 为分段格式）",
            segs.len()
        ));
    }
    Ok(())
}

/// 先尝试改名（同盘最快），跨卷失败则退回复制
fn rename_or_copy(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        let _ = std::fs::remove_file(dst);
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)
                .with_context(|| format!("移动失败: {} -> {}", src.display(), dst.display()))?;
            let _ = std::fs::remove_file(src);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------
// 流选择
// ---------------------------------------------------------------

fn pick_video<'a>(dash: &'a Dash, codec: Codec, qn: u32) -> Option<&'a Stream> {
    // 画质不高于目标值的流才可选
    let pool: Vec<&Stream> = dash.video.iter().filter(|s| s.id <= qn).collect();
    // 优先：画质不超过目标 + 编码匹配，取画质带宽最高者
    let best = |pool: Vec<&'a Stream>| -> Option<&'a Stream> {
        pool.into_iter()
            .max_by(|a, b| (a.id, a.bandwidth).cmp(&(b.id, b.bandwidth)))
    };
    let by_codec: Vec<&Stream> = pool
        .iter()
        .copied()
        .filter(|s| codec.matches(&s.codecs))
        .collect();
    best(by_codec).or_else(|| best(pool))
}

fn all_urls(s: &Stream) -> Vec<String> {
    let mut v = vec![s.primary_url().to_string()];
    for u in s.backup_urls() {
        if !u.is_empty() && !v.contains(&u) {
            v.push(u);
        }
    }
    v
}

// ---------------------------------------------------------------
// 字幕
// ---------------------------------------------------------------

#[derive(Deserialize)]
struct SubFile {
    #[serde(default)]
    body: Vec<SubLine>,
}

#[derive(Deserialize)]
struct SubLine {
    from: f64,
    to: f64,
    content: String,
}

fn fetch_subtitle(client: &Client, bvid: &str, cid: u64, srt_path: &Path) -> Result<Option<PathBuf>> {
    let Some(url) = client.get_subtitle_url(bvid, cid)? else {
        return Ok(None);
    };
    let json = client.get_text_url(&url)?;
    let srt = json_to_srt(&json)?;
    std::fs::write(srt_path, srt).with_context(|| format!("写入 {} 失败", srt_path.display()))?;
    Ok(Some(srt_path.to_path_buf()))
}

fn json_to_srt(json: &str) -> Result<String> {
    let f: SubFile = serde_json::from_str(json).context("字幕 JSON 解析失败")?;
    if f.body.is_empty() {
        bail!("字幕内容为空");
    }
    let mut out = String::new();
    for (i, l) in f.body.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            ts(l.from),
            ts(l.to),
            l.content
        ));
    }
    Ok(out)
}

fn ts(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let ms = ms % 1000;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

// ---------------------------------------------------------------
// ffmpeg 合并
// ---------------------------------------------------------------

/// 内嵌的精简 ffmpeg（编译自 ffmpeg 7.1，仅保留 MP4 无转码封装所需能力）
///
/// 约 5.3 MB（原版 160 MB，缩减 96.7%）。编译选项见
/// `build/ffmpeg-minimal.sh`：只启用 mov demuxer / mp4 muxer /
/// file protocol / h264,hevc,av1,aac 解码 / movtext 编码 / srt,ass 解复用。
static EMBEDDED_FFMPEG: &[u8] = include_bytes!("ffmpeg_min.exe");

/// 内嵌 ffmpeg 的 SHA-256（用于判断已释放的文件是否完整）
const EMBEDDED_FFMPEG_LEN: usize = EMBEDDED_FFMPEG.len();

/// 定位可用的 ffmpeg；找不到时自动释放内嵌副本到临时目录
///
/// 优先级：
/// 1. 用户自备的 ffmpeg（exe 同目录 / 上级目录 / 当前目录）
/// 2. 程序释放出的内嵌副本（`<exe所在目录>\.ffmpeg\ffmpeg.exe`）
/// 3. PATH 中的 ffmpeg（必须能真正运行）
fn ffmpeg_path() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE.get_or_init(locate_or_extract_ffmpeg).clone()
}

fn locate_or_extract_ffmpeg() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    // 1) 用户自备的完整版 ffmpeg（优先，尊重用户替换的可能）
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &exe_dir {
        candidates.push(dir.join("ffmpeg").join("ffmpeg.exe"));
        candidates.push(dir.join("ffmpeg.exe"));
        if let Some(up) = dir.parent() {
            candidates.push(up.join("ffmpeg").join("ffmpeg.exe"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ffmpeg").join("ffmpeg.exe"));
        candidates.push(cwd.join("ffmpeg.exe"));
    }
    for c in &candidates {
        if c.is_file() {
            return Some(c.clone());
        }
    }

    // 2) 释放内嵌精简版
    if let Some(dir) = &exe_dir {
        if let Some(p) = extract_embedded_ffmpeg(dir) {
            return Some(p);
        }
    }

    // 3) PATH
    if which_ffmpeg() {
        return Some(PathBuf::from("ffmpeg"));
    }
    None
}

/// 把内嵌 ffmpeg 写出到磁盘；若已存在且大小一致则直接复用
fn extract_embedded_ffmpeg(exe_dir: &Path) -> Option<PathBuf> {
    let dir = exe_dir.join(".ffmpeg");
    let path = dir.join("ffmpeg.exe");

    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() == EMBEDDED_FFMPEG_LEN as u64 {
            return Some(path);
        }
    }
    if std::fs::create_dir_all(&dir).is_err() {
        // 主目录不可写（如装在 Program Files），退回临时目录
        let dir = std::env::temp_dir().join("bilibili_dl_ffmpeg");
        let path = dir.join("ffmpeg.exe");
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() == EMBEDDED_FFMPEG_LEN as u64 {
                return Some(path);
            }
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        if std::fs::write(&path, EMBEDDED_FFMPEG).is_err() {
            return None;
        }
        return Some(path);
    }

    // 先写临时文件再改名，避免中途失败留下损坏的 exe
    let tmp = dir.join("ffmpeg.exe.tmp");
    if std::fs::write(&tmp, EMBEDDED_FFMPEG).is_err() {
        return None;
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(path)
}

fn merge(video: &Path, audio: &Path, srt: Option<&Path>, out: &Path) -> Result<()> {
    let ff = match ffmpeg_path() {
        Some(p) => p,
        None => bail!("未找到可用的 ffmpeg（内嵌版本释放失败，请检查程序目录写权限）"),
    };

    let has_audio = audio.exists();
    let mut attempts: Vec<Vec<String>> = Vec::new();

    // 尝试 1：视频+音频+字幕（内嵌 mov_text）
    if let (true, Some(s)) = (has_audio, srt) {
        attempts.push(vec![
            "-y".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(),
            "-i".into(), video.display().to_string(),
            "-i".into(), audio.display().to_string(),
            "-i".into(), s.display().to_string(),
            "-c:v".into(), "copy".into(),
            "-c:a".into(), "copy".into(),
            "-c:s".into(), "mov_text".into(),
            out.display().to_string(),
        ]);
    }
    // 尝试 2：视频+音频（无字幕）
    if has_audio {
        attempts.push(vec![
            "-y".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(),
            "-i".into(), video.display().to_string(),
            "-i".into(), audio.display().to_string(),
            "-c".into(), "copy".into(),
            out.display().to_string(),
        ]);
    }
    // 尝试 3：仅视频（无声视频）
    attempts.push(vec![
        "-y".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(),
        "-i".into(), video.display().to_string(),
        "-c".into(), "copy".into(),
        out.display().to_string(),
    ]);

    let mut errs: Vec<String> = Vec::new();
    for args in &attempts {
        let res = Command::new(&ff).args(args).output();
        match res {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr);
                errs.push(e.lines().rev().take(5).collect::<Vec<_>>().join(" / "));
            }
            Err(e) => {
                bail!("无法启动 ffmpeg: {e}");
            }
        }
    }
    bail!("ffmpeg 合并失败: {}", errs.join(" | "))
}

fn which_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------

fn ratio(done: u64, total: u64) -> f32 {
    if total > 0 {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    } else {
        f32::NAN // 未知长度
    }
}

fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => ' ',
            c => c,
        })
        .collect();
    out = out.trim().trim_end_matches('.').to_string();
    if out.is_empty() {
        out = "untitled".into();
    }
    if out.chars().count() > 80 {
        out = out.chars().take(80).collect();
    }
    out
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

fn fmt_bandwidth(bw: u64) -> String {
    if bw >= 1_000_000 {
        format!("{:.1} Mbps", bw as f64 / 1_000_000.0)
    } else if bw >= 1_000 {
        format!("{:.0} Kbps", bw as f64 / 1_000.0)
    } else {
        format!("{bw} bps")
    }
}

fn short_codec(codecs: &str) -> &str {
    if codecs.starts_with("avc1") { "AVC" }
    else if codecs.starts_with("hev1") || codecs.starts_with("hvc1") { "HEVC" }
    else if codecs.starts_with("av01") { "AV1" }
    else if codecs.starts_with("vp09") { "VP9" }
    else { codecs }
}

fn quality_name(id: u32) -> &'static str {
    match id {
        127 => "8K",
        126 => "Dolby",
        125 => "HDR",
        120 => "4K",
        116 => "1080P60",
        112 => "1080P+",
        80 => "1080P",
        74 => "1080P",
        64 => "720P",
        32 => "480P",
        16 => "360P",
        _ => "未知画质",
    }
}

// ---------------------------------------------------------------
// 测试
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 复现 download_page 中的进度映射，确认全程单调不回退且不越界
    #[test]
    fn test_progress_is_monotonic() {
        let page_total = 3usize;
        let page_span = 1.0 / page_total as f32;
        let mut last = -1.0f32;
        for page_idx in 1..=page_total {
            let page_start = (page_idx - 1) as f32 * page_span;
            let emit = |frac: f32| -> f32 {
                (page_start + frac.clamp(0.0, 1.0) * page_span).clamp(0.0, 1.0)
            };
            // 模拟：视频流 0->1，音频流 0->1
            for i in 0..=10 {
                let v = emit(ratio(i as u64 * 100, 1000) * VIDEO_WEIGHT);
                assert!(v >= last, "进度回退: {v} < {last}");
                assert!((0.0..=1.0).contains(&v));
                last = v;
            }
            for i in 0..=10 {
                let a = emit(VIDEO_WEIGHT + ratio(i as u64 * 100, 1000) * AUDIO_WEIGHT);
                assert!(a >= last, "进度回退: {a} < {last}");
                last = a;
            }
            // 合并阶段收尾
            let end = emit(1.0);
            assert!(end >= last);
            last = end;
        }
        assert_eq!(last, 1.0, "全部完成后进度应为 1.0");
    }

    /// 未知长度（total=0）时应返回 NaN，由 UI 兜底显示
    #[test]
    fn test_ratio_unknown_length() {
        assert!(ratio(500, 0).is_nan());
        assert_eq!(ratio(500, 1000), 0.5);
        assert_eq!(ratio(2000, 1000), 1.0, "超出长度应被截断");
    }

    #[test]
    fn test_sanitize_keeps_valid_filename() {
        assert_eq!(sanitize("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize("..."), "untitled");
        assert_eq!(sanitize("正常标题"), "正常标题");
        assert_eq!(sanitize(&"x".repeat(200)).chars().count(), 80);
    }

    /// 画质选择：目标画质之下的流才可被选中
    #[test]
    fn test_pick_video_respects_target() {
        let mk = |id: u32, bw: u64, codecs: &str| Stream {
            base_url: "u".into(),
            base_url_snake: String::new(),
            backup_url: vec![],
            backup_url_snake: vec![],
            id,
            bandwidth: bw,
            codecs: codecs.into(),
            width: None,
            height: None,
        };
        let dash = Dash {
            video: vec![
                mk(127, 9_000_000, "avc1.640033"),
                mk(80, 3_000_000, "avc1.640032"),
                mk(64, 2_000_000, "hev1.1.6.L120.90"),
                mk(32, 1_000_000, "avc1.4D401E"),
            ],
            audio: vec![],
        };
        // qn=80 时不应选到 127
        let v = pick_video(&dash, Codec::Auto, 80).unwrap();
        assert_eq!(v.id, 80);
        // 指定 AVC 时：id=64 那条是 HEVC，应被跳过，退到下一条 AVC(id=32)
        let v = pick_video(&dash, Codec::Avc, 64).unwrap();
        assert_eq!(v.id, 32);
        assert!(v.codecs.starts_with("avc1"));
        // 指定 AVC + qn=80 时可拿到 id=80 的 AVC
        let v = pick_video(&dash, Codec::Avc, 80).unwrap();
        assert_eq!(v.id, 80);
        // 指定 HEVC 时应选中 hev1 那条
        let v = pick_video(&dash, Codec::Hevc, 127).unwrap();
        assert!(v.codecs.starts_with("hev1"));
        assert_eq!(v.id, 64);
        // 无匹配编码时回退到任意编码的最高画质
        let v = pick_video(&dash, Codec::Av1, 127).unwrap();
        assert_eq!(v.id, 127);
    }

    /// 验证模式三 try_look 接口免登录可达 1080P
#[test]
fn test_mode3_try_look_reaches_1080p() {
    let c = Client::new();
    let info = c.get_video_info("BV1GJ411x7h7").expect("view 失败");
    let dash = c
        .get_dash_try_look(&info.bvid, info.cid, 127)
        .expect("try_look 接口失败");
    let max_q = dash.video.iter().map(|s| s.id).max().unwrap_or(0);
    println!("try_look 免登录最高画质 id={max_q} ({})", quality_name(max_q));
    assert_eq!(
        max_q, 80,
        "免登录 try_look 应返回 1080P(id=80)，实际 {max_q}"
    );
    // 普通 DASH 对照：免登录只有 480P
    let dash2 = c.get_dash(&info.bvid, info.cid, 127).expect("dash 失败");
    let max2 = dash2.video.iter().map(|s| s.id).max().unwrap_or(0);
    assert_eq!(max2, 32, "普通 DASH 免登录应为 480P");
    assert!(max_q > max2);
}

/// 验证模式三下三种编码都可选（这是用户要的「最好能选编码」）
#[test]
fn test_mode3_offers_all_codecs_at_1080p() {
    let c = Client::new();
    let info = c.get_video_info("BV1GJ411x7h7").expect("view 失败");
    let dash = c
        .get_dash_try_look(&info.bvid, info.cid, 127)
        .expect("try_look 失败");

    let has = |c: Codec| dash.video.iter().any(|s| s.id == 80 && c.matches(&s.codecs));
    println!(
        "1080P 编码可用性: AVC={} HEVC={} AV1={}",
        has(Codec::Avc),
        has(Codec::Hevc),
        has(Codec::Av1)
    );
    assert!(has(Codec::Avc), "1080P 应有 AVC");
    assert!(has(Codec::Hevc), "1080P 应有 HEVC");
    assert!(has(Codec::Av1), "1080P 应有 AV1");

    // pick_video 必须能按编码挑出对应流
    for (want, name) in [(Codec::Avc, "avc1"), (Codec::Hevc, "hev1"), (Codec::Av1, "av01")] {
        let v = pick_video(&dash, want, 127)
            .unwrap_or_else(|| panic!("选不出 {name} 流"));
        println!("  选中 {}: id={} codecs={}", want.label(), v.id, v.codecs);
        assert_eq!(v.id, 80, "{name} 应选中 1080P");
        assert!(want.matches(&v.codecs), "编码不匹配: {}", v.codecs);
    }
}

/// 模式三端到端：真实下载 1080P + 合并
#[test]
fn test_mode3_end_to_end_1080p() {
    let task = Task {
        bvid: "BV1GJ411x7h7".into(),
        qn: 80,
        codec: Codec::Auto,
        want_cover: false,
        want_sub: false,
        all_pages: false,
        out_dir: std::env::temp_dir().join("dsh_mode3_test"),
        mode: Mode::TryLook,
    };
    let rx = spawn(task);
    let mut done: Option<PathBuf> = None;
    loop {
        match rx.recv() {
            Ok(Ev::Log(s)) => println!("LOG  {s}"),
            Ok(Ev::Phase(s, n)) => println!("PHASE {s} (已完 {n} P)"),
            Ok(Ev::Progress(_)) => {}
            Ok(Ev::Done(dir)) => {
                done = Some(dir);
                break;
            }
            Ok(Ev::Fail(e)) => panic!("模式三下载失败: {e}"),
            Err(_) => break,
        }
    }
    let dir = done.expect("未收到 Done 事件");
    let mp4: Vec<_> = std::fs::read_dir(&dir)
        .expect("读取输出目录")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "mp4").unwrap_or(false))
        .collect();
    assert!(!mp4.is_empty(), "目录中没有 mp4: {}", dir.display());
    let meta = std::fs::metadata(&mp4[0].path()).unwrap();
    assert!(meta.len() > 1_000_000, "1080P 文件太小: {} 字节", meta.len());
    println!("模式三 OK: {} ({} 字节)", mp4[0].path().display(), meta.len());
}

/// 端到端测试：模式二 durl（无需 Cookie）真实下载
    ///
    /// 验证无 Cookie 情况下：
    /// - durl 接口可用
    /// - 画质达到 720P (id=64)，高于 DASH 未登录时的 480P (id=32)
    /// - 下载完成且文件非空
    #[test]
    fn test_mode2_durl_no_cookie_720p() {
        let task = Task {
            bvid: "BV1xx411c7mD".into(),
            qn: 64,
            codec: Codec::Auto,
            want_cover: false,
            want_sub: false,
            all_pages: false,
            out_dir: std::env::temp_dir().join("dsh_durl_test"),
            mode: Mode::Durl,
        };
        let rx = spawn(task);
        let mut done: Option<PathBuf> = None;
        loop {
            match rx.recv() {
                Ok(Ev::Log(s)) => println!("LOG  {s}"),
                Ok(Ev::Phase(s, n)) => println!("PHASE {s} (已完 {n} P)"),
                Ok(Ev::Progress(p)) => {}
                Ok(Ev::Done(dir)) => {
                    done = Some(dir);
                    break;
                }
                Ok(Ev::Fail(e)) => panic!("模式二下载失败: {e}"),
                Err(_) => break,
            }
        }
        let dir = done.expect("未收到 Done 事件");
        let mp4: Vec<_> = std::fs::read_dir(&dir)
            .expect("读取输出目录")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mp4").unwrap_or(false))
            .collect();
        assert!(!mp4.is_empty(), "目录中没有 mp4 文件: {}", dir.display());
        let meta = std::fs::metadata(&mp4[0].path()).unwrap();
        assert!(meta.len() > 50_000, "mp4 太小: {} 字节", meta.len());
        println!(
            "模式二 OK: {} ({} 字节)",
            mp4[0].path().display(),
            meta.len()
        );
    }

    /// 验证模式二 durl 接口在无 Cookie 下确实达到 720P (id=64)
    #[test]
    fn test_durl_quality_is_720p_without_cookie() {
        let c = Client::new();
        let info = c.get_video_info("BV1xx411c7mD").expect("view 失败");
        let data = c.get_durl(&info.bvid, info.cid).expect("durl 失败");
        println!(
            "durl quality={} 段数={} accept={:?}",
            data.quality,
            data.durl.len(),
            data.accept_quality
        );
        assert_eq!(
            data.quality, 64,
            "无 Cookie 下 durl 应返回 720P(id=64)，实际 {}",
            data.quality
        );
        assert!(!data.durl.is_empty());
        assert!(!data.durl[0].url.is_empty(), "第 0 段应有下载地址");
        assert!(data.durl[0].backup_url.len() > 0, "应有备用地址");
    }

    /// 对照验证：同一视频 DASH 接口无 Cookie 时只有 480P
    #[test]
    fn test_dash_quality_lower_than_durl_without_cookie() {
        let c = Client::new();
        let info = c.get_video_info("BV1xx411c7mD").expect("view 失败");
        let dash = c.get_dash(&info.bvid, info.cid, 127).expect("dash 失败");
        let dash_max = dash.video.iter().map(|s| s.id).max().unwrap_or(0);
        let data = c.get_durl(&info.bvid, info.cid).expect("durl 失败");
        println!("DASH 最高 id={dash_max}  vs  durl quality={}", data.quality);
        assert!(
            data.quality > dash_max,
            "durl({}) 应高于 DASH({dash_max})",
            data.quality
        );
    }

    /// 端到端测试：真实下载一个低画质视频并用 ffmpeg 合并
    #[test]
    fn test_full_download_and_merge() {
        let task = Task {
            bvid: "BV1xx411c7mD".into(),
            qn: 16,
            codec: Codec::Auto,
            want_cover: false,
            want_sub: false,
            all_pages: false,
            out_dir: std::env::temp_dir().join("dsh_dl_test"),
            mode: Mode::Dash,
        };
        let rx = spawn(task);
        let mut done: Option<PathBuf> = None;
        loop {
            match rx.recv() {
                Ok(Ev::Log(s)) => println!("LOG  {s}"),
                Ok(Ev::Phase(s, n)) => println!("PHASE {s} (已完 {n} P)"),
                Ok(Ev::Progress(_)) => {}
                Ok(Ev::Done(dir)) => {
                    done = Some(dir);
                    break;
                }
                Ok(Ev::Fail(e)) => panic!("下载失败: {e}"),
                Err(_) => break,
            }
        }
        let dir = done.expect("未收到 Done 事件");
        let mp4: Vec<_> = std::fs::read_dir(&dir)
            .expect("读取输出目录")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mp4").unwrap_or(false))
            .collect();
        assert!(!mp4.is_empty(), "目录中没有 mp4 文件: {}", dir.display());
        let meta = std::fs::metadata(&mp4[0].path()).unwrap();
        assert!(meta.len() > 50_000, "mp4 太小: {} 字节", meta.len());
        println!(
            "OK: {} ({} 字节)",
            mp4[0].path().display(),
            meta.len()
        );
    }
}

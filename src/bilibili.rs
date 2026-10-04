//! B站 API 客户端：视频信息、WBI 签名、DASH 播放地址、字幕、流下载
use anyhow::{anyhow, bail, Context, Result};
use md5::Digest as _;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
pub const REFERER: &str = "https://www.bilibili.com/";

/// WBI mixin key 重排表（B站前端算法）
const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

// ---------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
pub struct VideoInfo {
    pub aid: u64,
    pub bvid: String,
    pub title: String,
    #[serde(default)]
    pub pic: String,
    pub cid: u64,
    pub owner: Owner,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub pages: Vec<Page>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Owner {
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Page {
    pub cid: u64,
    #[serde(default)]
    pub part: String,
    #[serde(default)]
    pub page: u32,
    #[serde(default)]
    pub duration: u64,
}

#[derive(Debug, Deserialize)]
pub struct PlayData {
    #[serde(default)]
    pub dash: Option<Dash>,
}

// ---------------------------------------------------------------
// durl 老格式（fnval=1，音视频已合成）
// ---------------------------------------------------------------

/// durl 响应：无需 WBI 签名，未登录时画质上限比 DASH 更高（实测 720P vs 480P）
#[derive(Debug, Deserialize)]
pub struct DurlData {
    /// 实际返回的画质 id
    #[serde(default)]
    pub quality: u32,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub accept_quality: Vec<u32>,
    #[serde(default)]
    pub support_formats: Vec<SupportFormat>,
    #[serde(default)]
    pub durl: Vec<DurlSeg>,
}

#[derive(Debug, Deserialize)]
pub struct SupportFormat {
    #[serde(default)]
    pub quality: u32,
    #[serde(default)]
    pub new_description: String,
    #[serde(default)]
    pub format: String,
}

/// durl 中的一段（通常整段视频只有一个，但长视频可能分段）
#[derive(Clone, Debug, Deserialize)]
pub struct DurlSeg {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub backup_url: Vec<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub length: u64,
}

impl DurlSeg {
    pub fn all_urls(&self) -> Vec<String> {
        let mut v = Vec::new();
        if !self.url.is_empty() {
            v.push(self.url.clone());
        }
        for b in &self.backup_url {
            if !b.is_empty() && !v.contains(b) {
                v.push(b.clone());
            }
        }
        v
    }
}


#[derive(Debug, Deserialize)]
pub struct Dash {
    #[serde(default)]
    pub video: Vec<Stream>,
    #[serde(default)]
    pub audio: Vec<Stream>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Stream {
    // B站接口同时返回 camelCase 与 snake_case 两种字段，分别接收后合并
    #[serde(rename = "baseUrl", default)]
    pub base_url: String,
    #[serde(rename = "base_url", default)]
    pub base_url_snake: String,
    #[serde(rename = "backupUrl", default)]
    pub backup_url: Vec<String>,
    #[serde(rename = "backup_url", default)]
    pub backup_url_snake: Vec<String>,
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub bandwidth: u64,
    #[serde(default)]
    pub codecs: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl Stream {
    /// 主下载地址（兼容 camelCase / snake_case）
    pub fn primary_url(&self) -> &str {
        if !self.base_url.is_empty() {
            &self.base_url
        } else {
            &self.base_url_snake
        }
    }
    /// 全部备用地址（去重、去空）
    pub fn backup_urls(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for u in self.backup_url.iter().chain(self.backup_url_snake.iter()) {
            if !u.is_empty() && !v.contains(u) {
                v.push(u.clone());
            }
        }
        v
    }
}

// ---------------------------------------------------------------
// HTTP 客户端
// ---------------------------------------------------------------

pub struct Client {
    agent: ureq::Agent,
    cookie: Option<String>,
    mixin: OnceLock<String>,
}

impl Client {
    pub fn new() -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .tls_config(tls)
            .build()
            .into();
        let cookie = load_cookies();
        Self {
            agent,
            cookie,
            mixin: OnceLock::new(),
        }
    }

    /// 读取完整 JSON 文本
    pub fn get_text(&self, url: &str) -> Result<String> {
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Referer", REFERER)
            .header("Origin", "https://www.bilibili.com");
        if let Some(c) = &self.cookie {
            req = req.header("Cookie", c);
        }
        let mut resp = req.call().with_context(|| format!("请求失败: {url}"))?;
        resp.body_mut()
            .read_to_string()
            .with_context(|| format!("读取响应失败: {url}"))
    }

    /// 请求 B站 JSON 接口并校验 code 字段
    fn api_get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let text = self.get_text(url)?;
        let v: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            let head: String = text.chars().take(160).collect();
            format!("JSON 解析失败（响应开头: {head}）")
        })?;
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误");
            bail!("B站接口错误 {code}: {msg}");
        }
        let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
        serde_json::from_value(data).context("接口 data 字段解析失败")
    }

    /// 下载任意字节内容（封面、字幕 JSON 等）
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Referer", REFERER);
        if let Some(c) = &self.cookie {
            req = req.header("Cookie", c);
        }
        let resp = req.call().with_context(|| format!("请求失败: {url}"))?;
        let mut buf = Vec::new();
        resp.into_body()
            .into_reader()
            .read_to_end(&mut buf)
            .with_context(|| format!("读取内容失败: {url}"))?;
        Ok(buf)
    }

    /// 下载字幕 SRT 文本
    pub fn get_text_url(&self, url: &str) -> Result<String> {
        let bytes = self.get_bytes(url)?;
        String::from_utf8(bytes).context("字幕不是有效的 UTF-8 文本")
    }

    // ---------------------------------------------------------------
    // 视频信息
    // ---------------------------------------------------------------

    pub fn get_video_info(&self, bvid: &str) -> Result<VideoInfo> {
        let url = format!("https://api.bilibili.com/x/web-interface/view?bvid={bvid}");
        let mut info: VideoInfo = self.api_get(&url)?;
        if info.pages.is_empty() {
            info.pages.push(Page {
                cid: info.cid,
                part: info.title.clone(),
                page: 1,
                duration: info.duration,
            });
        }
        Ok(info)
    }

    // ---------------------------------------------------------------
    // WBI 签名
    // ---------------------------------------------------------------

    fn mixin_key(&self) -> Result<String> {
        let k = self
            .mixin
            .get_or_init(|| self.fetch_mixin_key().unwrap_or_default());
        if k.is_empty() {
            bail!("获取 WBI 签名密钥失败（请检查网络）");
        }
        Ok(k.clone())
    }

    fn fetch_mixin_key(&self) -> Result<String> {
        // 注意：nav 接口未登录时返回 code=-101，但 wbi_img 字段仍然有效，因此不走 code 校验
        let text = self.get_text("https://api.bilibili.com/x/web-interface/nav")?;
        let v: serde_json::Value =
            serde_json::from_str(&text).context("nav 接口 JSON 解析失败")?;
        let img_url = v
            .pointer("/data/wbi_img/img_url")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("nav 接口缺少 wbi_img.img_url"))?;
        let sub_url = v
            .pointer("/data/wbi_img/sub_url")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("nav 接口缺少 wbi_img.sub_url"))?;
        let key = shuffle_key(&url_file_stem(img_url), &url_file_stem(sub_url));
        if key.is_empty() {
            bail!("WBI 密钥长度异常");
        }
        Ok(key)
    }

    /// 生成带 w_rid 的签名查询串
    fn signed_query(&self, params: Vec<(&str, String)>) -> Result<String> {
        let mixin = self.mixin_key()?;
        let wts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow!("系统时间异常"))?
            .as_secs();

        let mut pairs: Vec<(String, String)> = params
            .into_iter()
            .map(|(k, v)| {
                let mut v = v;
                v.retain(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*'));
                (k.to_string(), v)
            })
            .collect();
        pairs.push(("wts".to_string(), wts.to_string()));
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect::<Vec<_>>()
            .join("&");
        // 与 B站前端一致：从查询串中剔除 !'()*（含百分号编码形式）
        for pat in ["!", "'", "(", ")", "*", "%21", "%27", "%28", "%29", "%2A", "%2a"] {
            query = query.replace(pat, "");
        }
        let mut hasher = md5::Md5::new();
        md5::Digest::update(&mut hasher, format!("{query}{mixin}").as_bytes());
        let w_rid: String = md5::Digest::finalize(hasher)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(format!("{query}&w_rid={w_rid}"))
    }

    // ---------------------------------------------------------------
    // 播放地址（DASH）
    // ---------------------------------------------------------------

    pub fn get_dash(&self, bvid: &str, cid: u64, qn: u32) -> Result<Dash> {
        let q = self.signed_query(vec![
            ("bvid", bvid.to_string()),
            ("cid", cid.to_string()),
            ("qn", qn.to_string()),
            ("fnver", "0".to_string()),
            ("fnval", "4048".to_string()),
            ("fourk", "1".to_string()),
        ])?;
        let url = format!("https://api.bilibili.com/x/player/wbi/playurl?{q}");
        let data: PlayData = self.api_get(&url)?;
        data.dash.ok_or_else(|| {
            anyhow!("该视频没有返回可用的 DASH 流（可能需要登录、大会员或受地区限制）")
        })
    }

    // ---------------------------------------------------------------
    // 播放地址（try_look：免登录 1080P）
    // ---------------------------------------------------------------

    /// 请求免登录 1080P 的 DASH 播放地址。
    ///
    /// 实测结论（无 Cookie，2026-10 验证）：
    /// - 普通 DASH（`try_look=0`）：未登录只有 480P (id=32)
    /// - **非 WBI 老接口 + `try_look=1`：未登录可达 1080P (id=80)**，
    ///   且 AVC / HEVC / AV1 三种编码均完整可用，可自由选择
    /// - 该参数的天花板是 1080P，不会给 1080P+(112) 或 4K(120)；
    ///   要更高画质仍需 Cookie
    ///
    /// 注意必须走**非 WBI** 老接口：WBI 接口即使带 `try_look=1` 也只返回 480P。
    pub fn get_dash_try_look(&self, bvid: &str, cid: u64, qn: u32) -> Result<Dash> {
        let url = format!(
            "https://api.bilibili.com/x/player/playurl?bvid={bvid}&cid={cid}&qn={qn}&fnver=0&fnval=4048&fourk=1&try_look=1"
        );
        let data: PlayData = self.api_get(&url)?;
        data.dash.ok_or_else(|| {
            anyhow!("try_look 接口未返回 DASH 流（该视频可能仅提供 durl，或存在地区/付费限制）")
        })
    }

    // ---------------------------------------------------------------
    // 播放地址（durl 老格式：无需 WBI，未登录画质更高）
    // ---------------------------------------------------------------

    /// 请求 durl 格式播放地址（`fnval=1`，音视频已合成为单文件）。
    ///
    /// 与 [`Client::get_dash`] 的关键区别：
    /// - 不需要 WBI 签名
    /// - 未登录时画质上限为 **720P(id=64)**，而 DASH 接口只有 **480P(id=32)**
    ///
    /// 实测结论（无 Cookie，2026-10）：`qn` 取 64 及以上时接口一律返回
    /// quality=64；取 32/16 时按请求值降级。因此这里把 qn 固定为 64。
    pub fn get_durl(&self, bvid: &str, cid: u64) -> Result<DurlData> {
        let url = format!(
            "https://api.bilibili.com/x/player/playurl?bvid={bvid}&cid={cid}&qn=64&fnver=0&fnval=1&fourk=1"
        );
        let data: DurlData = self.api_get(&url)?;
        if data.durl.is_empty() {
            bail!("durl 接口没有返回可用地址（该视频可能仅提供 DASH，或存在地区/付费限制）");
        }
        Ok(data)
    }

    // ---------------------------------------------------------------
    // 字幕
    // ---------------------------------------------------------------

    /// 返回最佳字幕（优先中文字幕）的下载地址
    pub fn get_subtitle_url(&self, bvid: &str, cid: u64) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct SubItem {
            #[serde(default)]
            lan: String,
            #[serde(default)]
            subtitle_url: String,
        }
        #[derive(Deserialize)]
        struct SubData {
            #[serde(default)]
            subtitles: Vec<SubItem>,
        }
        #[derive(Deserialize)]
        struct SubResp {
            #[serde(default)]
            subtitle: Option<SubData>,
        }
        let q = self.signed_query(vec![
            ("bvid", bvid.to_string()),
            ("cid", cid.to_string()),
        ])?;
        let url = format!("https://api.bilibili.com/x/player/wbi/v2?{q}");
        let resp: SubResp = match self.api_get(&url) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        let subs = resp.subtitle.map(|s| s.subtitles).unwrap_or_default();
        if subs.is_empty() {
            return Ok(None);
        }
        let pick = subs
            .iter()
            .find(|s| s.lan.starts_with("zh") || s.lan.starts_with("ai"))
            .or_else(|| subs.first())
            .unwrap();
        if pick.subtitle_url.is_empty() {
            return Ok(None);
        }
        let mut u = pick.subtitle_url.clone();
        if u.starts_with("//") {
            u = format!("https:{u}");
        }
        Ok(Some(u))
    }

    // ---------------------------------------------------------------
    // 流下载（带进度回调，失败时自动尝试备用地址）
    // ---------------------------------------------------------------

    pub fn download_to_file(
        &self,
        primary: &str,
        backups: &[String],
        path: &Path,
        mut on_progress: impl FnMut(u64, u64),
    ) -> Result<u64> {
        let mut urls = vec![primary.to_string()];
        for b in backups {
            if !b.is_empty() && !urls.contains(b) {
                urls.push(b.clone());
            }
        }
        let mut last_err: Option<anyhow::Error> = None;
        for u in &urls {
            match self.try_download(u, path, &mut on_progress) {
                Ok(n) => return Ok(n),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("没有可用的下载地址")))
    }

    fn try_download(
        &self,
        url: &str,
        path: &Path,
        on_progress: &mut impl FnMut(u64, u64),
    ) -> Result<u64> {
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", UA)
            .header("Referer", REFERER);
        if let Some(c) = &self.cookie {
            req = req.header("Cookie", c);
        }
        let resp = req.call().with_context(|| format!("连接失败: {}", short_url(url)))?;
        let total = resp.body().content_length().unwrap_or(0);
        let mut reader = resp.into_body().into_reader();
        let mut file = std::fs::File::create(path)
            .with_context(|| format!("无法写入文件 {}", path.display()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            on_progress(done, total);
        }
        drop(file);
        if total > 0 && done < total {
            bail!("传输中断: {done}/{total} 字节");
        }
        Ok(done)
    }
}

// ---------------------------------------------------------------
// 工具函数
// ---------------------------------------------------------------

/// 从 URL 或 BV 号文本中提取 BV 号
pub fn parse_bv(input: &str) -> Result<String> {
    let s = input.trim();
    if let Some(i) = s.find("BV") {
        let rest = &s[i..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if id.len() == 12 {
            return Ok(id);
        }
        if id.len() > 12 {
            return Ok(id[..12].to_string());
        }
    }
    bail!("无法从输入中识别 BV 号（支持完整链接或 BV 号，如 BV1xx411c7mD）")
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn url_file_stem(url: &str) -> String {
    let name = url.rsplit('/').next().unwrap_or("");
    name.split('.').next().unwrap_or("").to_string()
}

fn shuffle_key(img_key: &str, sub_key: &str) -> String {
    let raw: Vec<u8> = format!("{img_key}{sub_key}").into_bytes();
    if raw.len() < 64 {
        return String::new();
    }
    MIXIN_KEY_ENC_TAB
        .iter()
        .take(32)
        .map(|&i| raw[i] as char)
        .collect()
}

fn short_url(url: &str) -> String {
    if url.len() > 90 {
        format!("{}...", &url[..90])
    } else {
        url.to_string()
    }
}

/// 读取同目录下的 cookies.txt（Netscape 格式）
fn load_cookies() -> Option<String> {    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("cookies.txt"));
        }
    }
    candidates.push(std::path::PathBuf::from("cookies.txt"));
    for p in candidates {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let mut pairs: Vec<String> = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let cols: Vec<&str> = line.split('\t').collect();
                if cols.len() >= 7 {
                    pairs.push(format!("{}={}", cols[5], cols[6]));
                }
            }
            if !pairs.is_empty() {
                return Some(pairs.join("; "));
            }
        }
    }
    None
}

// ---------------------------------------------------------------
// 测试
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bv() {
        assert_eq!(
            parse_bv("https://www.bilibili.com/video/BV1xx411c7mD?spm_id_from=333.337").unwrap(),
            "BV1xx411c7mD"
        );
        assert_eq!(parse_bv("BV1xx411c7mD").unwrap(), "BV1xx411c7mD");
        assert_eq!(parse_bv("  BV1xx411c7mD  ").unwrap(), "BV1xx411c7mD");
        assert!(parse_bv("hello world").is_err());
    }

    /// 真实网络测试：视频信息 + WBI 签名播放地址
    #[test]
    fn test_network_view_and_dash() {
        let c = Client::new();
        let info = c
            .get_video_info("BV1xx411c7mD")
            .expect("view 接口失败");
        assert!(!info.title.is_empty());
        println!(
            "view ok: title={} cid={} pages={}",
            info.title,
            info.cid,
            info.pages.len()
        );

        let dash = c
            .get_dash(&info.bvid, info.cid, 127)
            .expect("playurl 接口失败（WBI 签名问题或网络）");
        assert!(!dash.video.is_empty(), "没有视频流");
        for v in &dash.video {
            println!(
                "video id={} height={} codecs={} bw={}",
                v.id,
                v.height.unwrap_or(0),
                v.codecs,
                v.bandwidth
            );
        }
        if !dash.audio.is_empty() {
            println!("audio count={}", dash.audio.len());
        }
        let best = dash.video.iter().max_by_key(|s| s.id).unwrap();
        println!("best quality id={} (期望 80/112/120 等)", best.id);
        assert!(best.id <= 127);
    }
}

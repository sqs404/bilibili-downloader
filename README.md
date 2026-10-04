# B站视频高清下载器（Rust + egui 图形界面）

一个使用 **Rust** 编写、基于 **egui** 图形界面的 B 站视频高清下载工具。

![](https://img.shields.io/badge/Rust-1.99-orange) ![](https://img.shields.io/badge/GUI-egui%200.36-blue) ![](https://img.shields.io/badge/size-9.16%20MB-brightgreen)

## ⬇️ 下载

**[前往 Releases 下载最新版 →](https://github.com/sqs404/bilibili-downloader/releases/latest)**

单文件绿色版，下载 `B站视频高清下载器.exe` 双击即用，**无需安装任何依赖**
（ffmpeg 已内嵌，首次合并时自动释放）。

> 仓库只保留源码。构建产物通过 Releases 分发，避免每次构建都让仓库膨胀。

## 🆕 下载模式（三种）

程序提供三种下载模式，可在界面顶部切换：

| 模式 | 接口 | 需要 Cookie | 免登录画质 | 编码选择 | 说明 |
|------|------|------------|-----------|---------|------|
| **模式一 · DASH**（原功能） | `playurl` (DASH, fnval=4048) | 想要 1080P+/4K 需要 | 480P | ✅ AVC/HEVC/AV1 | 音视频分离，**有 Cookie 时可达 8K/4K** |
| **模式二** | `playurl` (durl, fnval=1) | 不需要 | **720P** | ❌ 固定 | 音视频已合成，下载即成品，无需 ffmpeg |
| **模式三**（推荐免登录） | `playurl` (非WBI + `try_look=1`) | **不需要** | **1080P** | ✅ AVC/HEVC/AV1 | 免登录画质天花板，音视频分离需合并 |

### 模式三：免登录 1080P（实测结论）

这是穷举接口参数后**实测得出**的结果（无 Cookie 环境，2026-10 验证）：

| 请求方式 | 免登录最高画质 |
|---------|--------------|
| WBI 签名 DASH（模式一默认） | 480P |
| 非 WBI DASH + `try_look=1`（**模式三**） | **1080P (id=80)** |

关键点：

1. **必须走非 WBI 老接口**。WBI 接口即使加 `try_look=1` 仍然只给 480P。
2. `try_look=1` 时 1080P 的 **AVC / HEVC / AV1 三种编码全部可用**，
   可自由选择（AVC 码率最高，AV1 最省空间，HEVC 居中）。
3. **天花板是 1080P**，给不了 1080P+(112) 或 4K(120)。
4. 只对**本身存在 1080P 档位**的视频有效；老视频本身最高就 480P 时，
   模式三也只会给到它自己的最高档（程序会在日志中提示）。

实测产出的模式三文件（`BV1GJ411x7h7`，AVC 编码）：

```
Stream #0:0: Video: h264 (High), 1920x1080 [SAR 1:1 DAR 16:9], 2627 kb/s, 25 fps
Stream #0:1: Audio: aac (LC), 48000 Hz, stereo, 201 kb/s
```

### 怎么选

| 你的需求 | 选择 |
|---------|------|
| 免登录 + 最高画质 + 可选编码 | **模式三** |
| 免登录 + 快速下载（免 ffmpeg） | 模式二 |
| 要 4K / 8K / 1080P+ | 模式一 + `cookies.txt` |

## 功能特性

- 🖥️ **原生图形界面**：粘贴链接 → 点击解析 → 选择画质 → 一键下载
- 🎚️ **双模式下载**：模式一 DASH（8K/4K/HEVC/AV1，需 Cookie 解锁高清）
  与模式二 durl（免登录 720P），界面顶部一键切换
- 🎚️ **三模式下载**：模式一 DASH（配 Cookie 可 8K/4K）、模式二 durl（免登录 720P）、
  模式三 try_look（**免登录 1080P，且可 AVC/HEVC/AV1**）
- 🎨 **多画质选择**：最高(8K/4K) / 4K / 1080P60 / 1080P+ / 1080P / 720P / 480P / 360P
- 🎞️ **多编码支持**：AVC(H.264) / HEVC(H.265) / AV1 / 自动
- 🔗 **WBI 签名**：完整实现 B 站 WBI 签名算法，直接调用官方接口
- 📥 **DASH 流下载**：视频流 + 音频流分块下载，断线自动尝试备用地址
- 🎬 **自动合并**：调用内置 ffmpeg 合并为 MP4（保留原画质无损封装）
- 📝 **字幕下载**：自动获取中文字幕，转为 SRT 并内嵌进 MP4
- 🖼️ **封面下载**：保存视频封面图片
- 📋 **全分P支持**：一键下载合集中的所有分P
- 🍪 **登录支持**：放置 `cookies.txt` 可突破未登录画质限制（详见下文）
- 📊 **实时进度**：进度条 + 详细日志（画质、码率、文件大小）

## 使用方法

### 图形界面（推荐）

双击 **`B站视频高清下载器.exe`**：

1. 粘贴 B 站视频链接（或直接输入 BV 号）
2. 点击 **解析** — 显示标题、UP主、封面
3. 选择画质、编码，勾选需要的选项（封面/字幕/全部分P）
4. 点击 **开始下载**，等待进度条完成
5. 点击 **打开文件夹** 查看结果

### ⚠️ 画质说明（重要）

B 站接口对**未登录用户限制最高 480P**。想要 1080P 及以上画质，需要登录 Cookie：

1. 在浏览器中登录 B 站
2. 安装浏览器扩展 **"Get cookies.txt LOCALLY"**（Chrome/Edge 商店可搜到）
3. 在 B 站任意页面点击扩展图标，导出 `cookies.txt`
4. 将 `cookies.txt` 放到 **exe 同目录**下，重新启动程序

> 登录后：1080P；大会员：1080P+/4K/8K/HDR 等

## 输出文件

```
downloads/
└── 视频标题/
    ├── 视频标题.mp4        # 合并后的视频（含内嵌字幕）
    ├── 视频标题.srt         # 字幕文件
    └── 视频标题_cover.jpg   # 封面
```

多分P视频会按 `P01 - 分P标题.mp4` 逐个保存。

## 📦 单文件打包（体积优化）

**`B站视频高清下载器.exe` 就是全部**，无需任何外部文件。

程序内嵌了一份**自行编译的精简 ffmpeg**，首次需要合并音视频时会自动释放并复用。

### 体积演进

| 阶段 | 体积 | 说明 |
|------|------|------|
| 起点 | **197.2 MB** | 官方全功能 ffmpeg（160.5 MB）+ 原 exe |
| 精简 ffmpeg | 36.7 MB | ffmpeg 降到 5.3 MB（−96.7%），单文件 |
| **最终** | **9.16 MB** | 见下方两项优化 |

### 优化项

**1. 精简 ffmpeg（5.3 → 2.89 MB）**

编译时额外加 `--disable-optimizations --enable-small`，针对体积而非速度生成代码。
本程序只做 `-c copy` 无转码封装，**完全不受编译优化影响**。

精简版只保留本程序用到的能力（`build/ffmpeg-minimal.sh`）：

- **解复用**：mov（即 MP4，DASH 分片格式）、srt、ass
- **封装**：mp4
- **协议**：file
- **解码器**：h264 / hevc / av1 / aac / ass / subrip（`-c copy` 需用于探测流信息）
- **编码器**：movtext（字幕内嵌）
- **关闭**：全部滤镜、缩放、色彩转换、后处理、设备、**所有转码器**

> 因为程序只做 `-c copy`，所以不需要任何编码器。
> 若你手动用它做转码会失败——这是预期行为。

**2. Rust 编译优化（31.4 → 6.3 MB 代码部分）**

```toml
[profile.release]
opt-level = "z"    # 针对体积优化
lto = true         # 跨模块内联，裁掉未使用代码
codegen-units = 1  # 单编译单元，利于整体裁剪
strip = true       # 去掉符号表
panic = "abort"    # 去掉展开表
```

**3. 渲染后端 wgpu → glow（−2.8 MB）**

改用 OpenGL 而非 Vulkan/DX12，画面完全一致，但省下 `wgpu-core`/`naga`/
`wgpu-hal` 一整套着色器编译栈（原先是最大的编译产物）。

**想换回完整版 ffmpeg**：把官方 `ffmpeg.exe` 放到 exe 同目录或 `ffmpeg\` 子目录，
程序会优先使用你自备的版本。

## 从源码构建

环境要求：Rust 1.99+（windows-gnu 工具链）

```powershell
cargo build --release
# 产物: target/release/bilibili-downloader.exe
cargo test        # 含真实网络端到端测试
```

> 本项目使用 llvm-mingw 作为链接器（无需安装 Visual Studio），链接器已在
> `~/.cargo/config.toml` 中配置：`[target.x86_64-pc-windows-gnu] linker = "x86_64-w64-mingw32-gcc"`
>
> ⚠️ **必须使用完整的 llvm-mingw**（例如 [llvm-mingw releases](https://github.com/mstorsjo/llvm-mingw/releases) 的
> `ucrt-x86_64` / `msvcrt-x86_64` 压缩包，解压后把其 `bin` 加入 PATH）。
> Rust 自带的 `rust-mingw` 组件**不可用**：它只提供一个「仅供链接」的精简 gcc，
> 缺少 `as` / 可用的 `dlltool`，生成导入库时会失败：
>
> ```
> error: error calling dlltool 'dlltool.exe': program not found
> ```
>
> 另外注意 llvm-mingw 以 `libunwind` 替代 `libgcc`，若链接器报
> `unable to find library -lgcc` / `-lgcc_eh`，把
> `<llvm-mingw>\x86_64-w64-mingw32\lib\libunwind.a` 复制一份为
> `libgcc.a` 与 `libgcc_eh.a` 即可。

## 项目结构

```
bilibili-downloader/
├── src/
│   ├── main.rs         # 程序入口（eframe 启动）
│   ├── app.rs          # egui 图形界面（解析/模式选择/选项/进度/日志）
│   ├── bilibili.rs     # B站 API：view/playurl/durl/try_look/字幕 + WBI 签名 + 流下载
│   ├── dl.rs           # 下载任务：流选择/进度事件/ffmpeg 合并
│   └── ffmpeg_min.exe  # 内嵌的精简 ffmpeg（2.89 MB，编译自源码）
├── build/
│   └── ffmpeg-minimal.sh   # 精简 ffmpeg 的 configure 脚本
├── B站视频高清下载器.exe    # 唯一需要的文件（9.16 MB，已内嵌 ffmpeg）
├── Cargo.toml
└── README.md
```

## 技术要点

| 模块 | 选型 | 说明 |
|------|------|------|
| GUI | eframe/egui 0.36 + glow | 纯 Rust 即时模式 GUI，自动加载微软雅黑中文字体；用 OpenGL 后端以减小体积 |
| HTTP | ureq 3 + native-tls | 阻塞式 HTTP，Windows 系统证书验证，无 C 依赖 |
| 签名 | md-5 + WBI 算法 | mixin key 重排 + 参数排序 + md5 |
| 合并 | 内嵌精简 ffmpeg -c copy | 音视频无损封装为 MP4，仅 2.89 MB（源码自编） |

## 常见问题

- **解析失败**：检查网络，或更新 B 站接口已知变更
- **没有可用 DASH 流**：番剧/地区限制内容需要对应 Cookie 或会员
- **合并失败**：ffmpeg 已内嵌，通常无需处理。若 exe 所在目录不可写
  （如装在 `Program Files`），程序会自动改用临时目录释放副本
- **想用完整版 ffmpeg**：把官方 `ffmpeg.exe` 放到 exe 同目录即可覆盖内嵌版本

## 变更记录

- **体积优化到 9.16 MB**（从 197.2 MB 缩减 95.4%）：
  - ffmpeg 改用 `--disable-optimizations --enable-small` 编译，5.3 → 2.89 MB
  - Cargo release 加 `opt-level="z" + lto + codegen-units=1 + strip + panic=abort`，
    代码部分 31.4 → 6.3 MB
  - 渲染后端由 wgpu 换成 glow（OpenGL），画面一致，省下 naga/wgpu 着色器栈
- **单文件打包 + ffmpeg 精简**：ffmpeg 由官方全功能版（160.5 MB）换成自行编译的
  精简版，并用 `include_bytes!` 内嵌进 exe，首次合并时自动释放并复用
- **封面预览修复**：`image` 依赖启用 `png` feature（B站封面为 PNG，此前只启用
  `jpeg`，导致预览永远报「封面解码失败」）
- **ffmpeg 探测修复**：探测结果全局缓存，不再每合并一个文件就 spawn 一次
  `ffmpeg -version`；缺失时给出明确的「未找到 ffmpeg」提示
- **进度条修复**：视频流按 90%、音频流按 10% 分配权重，多分P时映射到全局区间，
  不再出现「下载音频流时进度条倒退」和分P之间反复横跳；多分P时显示 `P{n}/{total}`
- **临时文件清理**：下载中途失败时也会删除 `.video.tmp` / `.audio.tmp`，不再残留
- **按钮不可见修复**：链接输入框与保存目录输入框曾使用
  `desired_width(f32::INFINITY)`，会占满整行，把「解析」「浏览...」按钮挤出
  可视区域（表现为按钮消失或点不到）。改为按 `available_width()` 预留按钮宽度，
  并新增 3 个布局回归测试锁定该行为。

## License

MIT

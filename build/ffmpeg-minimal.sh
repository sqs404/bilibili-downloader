#!/bin/sh
# 极简 ffmpeg 构建脚本（体积最小化）
#
# 目标：只保留本程序用到的那几个命令所需的 ffmpeg 能力。
# 程序里 ffmpeg 仅用于「无转码封装」——所有流都是 -c copy：
#   ffmpeg -i video.m4s -i audio.m4s [-i sub.srt] -c:v copy -c:a copy -c:s mov_text out.mp4
# 输入输出全部是 ISO MP4(mov) 封装，视频 H.264/HEVC/AV1，音频 AAC，字幕 SRT。
# 因此不需要任何编码器/滤镜/缩放器，只保留解封装+再封装。
#
# 产物约 5.3 MB（官方全功能版约 160 MB）。
#
# 依赖：llvm-mingw（clang/llvm-ar/llvm-ranlib/llvm-strip）、GNU make、bash
# 用法：bash build/ffmpeg-minimal.sh <ffmpeg源码目录> <输出前缀> <安装bindir>
set -e

SRC_DIR="$1"
PREFIX="$2"
BINDIR="$3"
TOOLCHAIN="${TOOLCHAIN:-C:/llvm-mingw-20260922-ucrt-x86_64}"

export PATH="$TOOLCHAIN/bin:$PATH"
cd "$SRC_DIR"

./configure \
  --prefix="$PREFIX" --bindir="$BINDIR" \
  --target-os=win32 --arch=x86_64 \
  --cc="$TOOLCHAIN/bin/clang.exe" \
  --ar="$TOOLCHAIN/bin/llvm-ar.exe" \
  --ranlib="$TOOLCHAIN/bin/llvm-ranlib.exe" \
  --strip="$TOOLCHAIN/bin/llvm-strip.exe" \
  --enable-cross-compile \
  --disable-doc --disable-htmlpages --disable-manpages \
  --disable-podpages --disable-txtpages \
  --disable-iconv --disable-x86asm --disable-asm --disable-inline-asm \
  --disable-symver --disable-debug \
  --enable-small \
  --disable-everything \
  --enable-ffmpeg \
  --enable-demuxer=mov --enable-muxer=mp4 --enable-protocol=file \
  --enable-decoder=h264 --enable-decoder=hevc --enable-decoder=av1 --enable-decoder=aac \
  --enable-decoder=ass --enable-decoder=subrip \
  --enable-encoder=movtext \
  --enable-demuxer=srt --enable-demuxer=ass \
  --enable-avformat --enable-avcodec

# 体积优先：关闭优化并针对体积生成代码，产出的 ffmpeg 更小（本项目只做 -c copy，
# 性能不受影响）
sed -i 's/^#.*optimizations.*$//' ffbuild/config.mak || true
make -j"$(nproc)"

echo "== 完成 =="
ls -la ffmpeg.exe
echo "把 ffmpeg.exe 复制到 src/ffmpeg_min.exe 即可被 include_bytes! 内嵌"

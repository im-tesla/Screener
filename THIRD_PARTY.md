# Bundled video engines

The application and its combined distribution are offered under GPL-3.0-or-later. See LICENSE. The video engines are unmodified; zstd compression only changes their storage inside the executables.

## FFmpeg

- FFmpeg 9.0.1 full build by Gyan Doshi, built with GPL and version3 enabled.
- Executable SHA-256: `57c56e369d5b4873b4d93fc1a1d833cb7cd8bc9325c14b05c34ce60b22842d8a`.
- [Build downloads, configuration and source links](https://www.gyan.dev/ffmpeg/builds/).
- [Pinned package](https://github.com/GyanD/codexffmpeg/releases/download/9.0.1/ffmpeg-9.0.1-full_build.7z).
- [Exact upstream FFmpeg source revision](https://github.com/FFmpeg/FFmpeg/commit/bf1b838f2a).
- [License information](https://ffmpeg.org/legal.html).

## libmpv

- shinchiro x86_64 development build 20261111, mpv revision `bcde6b93cc`.
- DLL SHA-256: `88504ee5560942a955405e99dfb5971368e4cf851321f36565bf2a667271db1b`.
- [Exact build release](https://github.com/shinchiro/mpv-winbuild-cmake/releases/tag/20261111).
- [Build recipes and dependency source revisions](https://github.com/shinchiro/mpv-winbuild-cmake).
- [mpv source](https://github.com/mpv-player/mpv/tree/bcde6b93cc).
- [mpv copyright and licensing](https://github.com/mpv-player/mpv/blob/master/Copyright).

The runtimes include further open source libraries as documented by those projects and their build recipes. Rust dependency versions are recorded in Cargo.lock. The source repository does not include the runtime binaries. Distributions that bundle those components must also provide their applicable notices and complete corresponding sources.

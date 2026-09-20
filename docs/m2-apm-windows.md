# M2 的 APM 半边：`webrtc-audio-processing` 在 Windows/MSVC 上编不过

调查时间：2026-09-20。机器：Windows 11 Pro / rustc 1.96 MSVC / VS BuildTools 18（MSVC 14.51）。

## 一句话结论

**`webrtc-audio-processing` 2.1.0 开箱在 Windows/MSVC 上编不过，但不是死路。**

libwebrtc 的 APM 本体（含 abseil）在 MSVC 下**能完整编出来** —— 我编出来了，
59814 个符号也前缀成功了。缺的是那个 Rust `-sys` crate 的构建脚本里
**一个 Windows 分支都没有**：它无条件按 Linux 的习惯调工具和传编译参数。

所以这是「上游缺 Windows 支持」，不是「技术选型错了」。

## 走到哪一步了

按 `cargo build --features bundled` 的实际执行顺序：

| 步骤 | 结果 |
|---|---|
| meson setup（MSVC 后端） | ✅ 过 |
| meson 下载 abseil-cpp 20240722 子项目 | ✅ 过 |
| ninja 编 abseil（~300 个目标） | ✅ 过（需要两处变通，见下） |
| ninja 编 APM 本体（85 个目标） | ✅ 过（需要 C++20） |
| ninja install | ✅ 过 |
| `nm` 枚举符号 + `objcopy` 加 `v2_` 前缀 | ✅ 过（需要变通） |
| **cc-rs 编 crate 自己的 `src/wrapper.cpp`** | ❌ **卡在这里** |
| bindgen 生成绑定 | 没走到 |
| 链接 | 没走到 |

最后那一步的报错：

```
cl: 命令行 error D8021: 无效的数值参数"/Wno-unused-parameter"
```

## 五个缺口

### 1. `build.rs` 给 `cl` 喂 GCC 风格的编译参数（**当前的拦路虎**）

`webrtc-audio-processing-sys-2.1.0/build.rs:403-404`：

```rust
.flag("-std=c++17")
.flag("-Wno-unused-parameter")
```

无条件加，没有 `cfg!(target_env = "msvc")` 分支。`cl` 认 `-` 当 `/`，于是
`-Wno-unused-parameter` 变成 `/Wno-unused-parameter` —— 无效参数，直接 D8021。
`-std=c++17` 同理（MSVC 要的是 `/std:c++17`，冒号不是等号）。

修法是三行：MSVC 下换成 `/std:c++20`，并且不要传 `-Wno-*`。

### 2. 上游 `meson.build` 把 C++ 标准钉死在 c++17，但代码用了 C++20 特性

WebRTC 的 `gain_controller2.cc`、`agc2/input_volume_stats_reporter.cc` 用了
**designated initializer**（`.field = value`）。GCC/Clang 在 c++17 下当扩展接受，
MSVC 严格拒绝：

```
error C7555: 使用指定的初始值设定项需要"/std:c++20"
```

变通：`meson configure -Dcpp_std=c++20`。实测这么改之后 APM 本体 85 个目标全过。

`meson setup --reconfigure` 会保留已经设过的选项，所以「先失败一次、手工 configure、
再跑一次」能绕过去 —— 但那不是能写进 CI 的东西。

### 3. MAX_PATH（这个最阴，报错完全误导人）

`cl` 会报：

```
fatal error C1083: 无法打开源文件"../webrtc-audio-processing/subprojects/
abseil-cpp-20240722.0/absl/base/internal/strerror.cc": No such file or directory
```

**文件明明存在，手工用同样的相对路径调 `cl` 也能编。** 真正的原因是
Windows 拿 `当前目录 + 相对路径`（规范化**之前**）去比 260 字符上限。

实测：构建目录 136 字符 + 相对路径 128 字符 = 265 > 260 就炸；
同一次构建里短文件名的目标能过、长文件名的过不去，所以看起来像是随机失败。

变通：构建根目录必须极短。`C:\Users\admin\AppData\Local\Temp\kapm`（38 字符）
还是不够，换成 `C:\k`（4 字符）才全过。

crate 的 out 目录本身就吃掉 ~100 字符
（`target\debug\build\webrtc-audio-processing-sys-<16位哈希>\out\`），
没有多少余地。

### 4. `nm` 是裸的 PATH 查找

`build.rs:295` 直接 `Command::new("nm")`。Windows 上没有。

注意 `objcopy` 反而是好的 —— `determine_objcopy_path()` 会去 rustc sysroot 里找，
而 Rust 自带 `rust-objcopy.exe`。只有 `nm` 漏了。

变通：`rustup component add llvm-tools` 拿到 `llvm-nm.exe`，复制一份改名 `nm.exe`
放进 PATH。`llvm-nm` 自称 "compatible with GNU nm"，`--defined-only --format=posix`
都认，实测能用。

修法：让 `nm` 走跟 `objcopy` 一样的 sysroot 查找。

### 5. `cp -a` 和 `patch` 是 Unix 工具

`build.rs` 用它们拷源码树和打补丁。Git for Windows 自带，但要在 PATH 上，
而从 PowerShell 跑 cargo 时通常不在。

## 复现配方

全套都要，缺一不可：

```bash
# 1. meson（pip 装到隔离的 venv，别污染全局）
python -m venv buildtools && ./buildtools/Scripts/pip install meson ninja

# 2. nm 的影子
rustup component add llvm-tools
cp "$(rustc --print sysroot)/lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-nm.exe" shim/nm.exe

# 3. MSVC 环境（meson 要在 PATH 上看得见 cl.exe 才会选 MSVC 后端）
#    从 vcvars64.bat 里把 INCLUDE / LIB / LIBPATH / PATH 捞出来注入 shell

# 4. 构建根必须极短
mkdir /c/k && cd /c/k

# 5. 先跑一次（会在 ninja 那步失败），然后手工开 C++20，再跑
cargo build                                    # 失败：C7555
cd target/debug/build/webrtc-*/out/webrtc-audio-processing-build
meson configure -Dcpp_std=c++20
cd /c/k && cargo build                         # 走到 wrapper.cpp 才失败
```

第 5 步之后就卡在缺口 1，**没有纯环境变量的绕法** —— cc-rs 只能往上加参数，
没法把 build.rs 已经加的 `-Wno-unused-parameter` 拿掉。

## 两个副产品发现

### APM 强制 10 ms 帧

`api/audio/audio_processing.h:83`：

> APM accepts only linear PCM audio data in chunks of ~10 ms

这跟 M1 的帧长冲突是同一个战场：M1 算出来延迟要 10 ms 帧、带宽要 20 ms 帧。
APM 这条把**处理**粒度钉死在 10 ms，但**打包**粒度不受影响 ——
20 ms 一个包就跑两次 APM 即可。所以它不改变 M1 的结论，只是说明
「10 ms 是这条链路的天然节拍」。

### 静态库 strip 之后 38 MB

带调试信息 160 MB，`llvm-objcopy --strip-debug` 之后 **38 MB**。

**这不等于安装包会大 38 MB** —— 链接器会丢掉用不到的 section，
APM 真正进二进制的部分通常是几 MB 量级。但 30 MB 的安装包红线摆在那儿，
这一项必须在 M5 打包时实测，不能想当然。

## 选项

| 方案 | 代价 | 说明 |
|---|---|---|
| **A. vendor 一份打过补丁的 `-sys`** | 仓库里多 5.4 MB 第三方源码，自己维护 | 补丁本身很小：build.rs 三行 + meson 一个选项 + nm 查找。同时把补丁提给上游 |
| B. 等上游支持 Windows | 不可控 | 上游 meson.build **有** windows 分支，说明不是没人管，但 Rust 侧没人测过 |
| C. 换 Windows 自带的 Voice Capture DSP | 零依赖、零体积 | 回声消除质量明显不如 AEC3，跟「外放开黑不啸叫」的目标有落差 |
| D. 先不做 AEC，只做降噪/AGC | 拖延 | 外放用户会啸叫，等于这个场景不能用 |

推荐 **A**：补丁小、可控，而且顺手能回馈上游。B 不可控，C 牺牲的正是设计里点名
「绝对不要自己写」的那块核心价值，D 不解决问题。

在做出选择之前，M2 的 APM 半边保持未完成状态 —— 设备那半边（30.4 ms）已经完成，
见 `m2-baseline.txt`。

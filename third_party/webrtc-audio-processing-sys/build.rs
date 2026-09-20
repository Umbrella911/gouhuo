// ===========================================================================
// 这是 webrtc-audio-processing-sys 2.1.0 的**打过补丁的副本**（kaimai 项目）。
//
// 上游这个 crate 的构建脚本里一个 Windows 分支都没有，开箱在 MSVC 上编不过。
// 打补丁的地方全部用 `kaimai patch:` 注释标出来了，一共六处：
//   1. meson 在 MSVC 下要 -Dcpp_std=c++20（WebRTC 用了 designated initializer）
//   2. nm 要能从 rustc sysroot 里找（Windows 上 PATH 里没有 nm）
//   3. cc-rs 的编译参数在 MSVC 下不能用 GCC 风格
//   4. bindgen 解析 MSVC 的 STL 头要 _ALLOW_COMPILER_AND_STL_VERSION_MISMATCH
//   5. MSVC 上要跳过符号前缀（对 COFF 归档不生效，反而让两边对不上）
//   6. meson 产出的 lib<name>.a 要补一个 MSVC 认的 <name>.lib 名字
//
// 诊断全文见 docs/m2-apm-windows.md。这些补丁应该提给上游，合并之后就能
// 删掉整个 third_party/ 目录、回到 crates.io 的版本。
//
// 除了带 `kaimai patch:` 的地方，其余内容跟上游 2.1.0 一字不差。
// ===========================================================================

use anyhow::{bail, Context, Result};
use bindgen::callbacks::{AttributeInfo, DeriveInfo, ParseCallbacks};
use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    process::Command,
};

/// Name and minimum version of the library that we are binding to.
const LIB_NAME: &str = "webrtc-audio-processing-2";
#[cfg(not(feature = "bundled"))]
const LIB_MIN_VERSION: &str = "2.1";

const MACOSX_DEPLOYMENT_TARGET_VAR: &str = "MACOSX_DEPLOYMENT_TARGET";

/// Symbol prefix for the webrtc-audio-processing library to allow multiple versions to coexist.
const SYMBOL_PREFIX: &str = "v2_";

/// kaimai patch: 目标平台是不是 MSVC。
///
/// 注意不能用 `cfg!(target_env = "msvc")` —— 构建脚本是给**宿主**编译的，
/// `cfg!` 判的是宿主不是目标。交叉编译时那样写就错了。
fn target_is_msvc() -> bool {
    env::var("CARGO_CFG_TARGET_ENV").map(|v| v == "msvc").unwrap_or(false)
}

fn out_dir() -> PathBuf {
    std::env::var("OUT_DIR").expect("OUT_DIR environment var not set.").into()
}

/// Prefix specified symbols in a static library using objcopy --redefine-sym.
fn prefix_archive_symbols(
    archive_path: &std::path::Path,
    symbols: &[String],
    prefix: &str,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }

    eprintln!(
        "Prefixing {} symbols in {} with '{}'",
        symbols.len(),
        archive_path.display(),
        prefix
    );

    let temp_path = archive_path.with_extension("prefixed.a");

    let objcopy = determine_objcopy_path()?;

    // Write arguments to a temp file to avoid "Argument list too long" errors.
    let args_path = archive_path.with_extension("args");
    let mut writer = BufWriter::new(File::create(&args_path)?);
    for symbol in symbols {
        writeln!(writer, "--redefine-sym={}={}{}", symbol, prefix, symbol)?;
    }
    writer.flush()?;
    drop(writer);

    let mut cmd = Command::new(&objcopy);
    cmd.arg(format!("@{}", args_path.display()));
    cmd.arg(archive_path);
    cmd.arg(&temp_path);

    eprintln!("Running {cmd:?}");
    let status = cmd.status().context(format!("Failed to execute {:?}", objcopy))?;

    if !status.success() {
        anyhow::bail!("{:?} failed with status: {}", objcopy, status);
    }

    std::fs::rename(&temp_path, archive_path).with_context(|| {
        format!("Failed to rename {} to {}", temp_path.display(), archive_path.display())
    })?;

    Ok(())
}

#[cfg(not(feature = "bundled"))]
mod webrtc {
    use super::*;

    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let (pkgconfig_include_path, pkgconfig_lib_path) = find_pkgconfig_paths()?;

        let include_path = std::env::var("WEBRTC_AUDIO_PROCESSING_INCLUDE")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_include_path);
        let lib_path = std::env::var("WEBRTC_AUDIO_PROCESSING_LIB")
            .ok()
            .map(PathBuf::from)
            .or(pkgconfig_lib_path);

        if include_path.is_none() || lib_path.is_none() {
            bail!(
                "Couldn't find {}. Please install it or set WEBRTC_AUDIO_PROCESSING_INCLUDE and WEBRTC_AUDIO_PROCESSING_LIB environment variables.",
                LIB_NAME
            );
        }

        Ok((vec![include_path.unwrap()], vec![lib_path.unwrap()]))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        Ok(())
    }

    fn find_pkgconfig_paths() -> Result<(Option<PathBuf>, Option<PathBuf>)> {
        let lib = match pkg_config::Config::new()
            .atleast_version(LIB_MIN_VERSION)
            .statik(false)
            .probe(LIB_NAME)
        {
            Ok(lib) => lib,
            Err(e) => {
                eprintln!("Couldn't find {LIB_NAME} with pkg-config:");
                eprintln!("{e}");
                return Ok((None, None));
            },
        };

        Ok((lib.include_paths.first().cloned(), lib.link_paths.first().cloned()))
    }

    pub(super) fn prefix_library_symbols(
        _lib_dirs: &[PathBuf],
        _prefix: &str,
    ) -> Result<Vec<String>> {
        // For non-bundled builds, we can't prefix symbols in the system library.
        // Users would need to build with bundled feature for multi-version support.
        println!(
            "cargo:warning=Symbol prefixing is only supported with the 'bundled' feature. \
            Without it, linking multiple versions of this crate may cause symbol conflicts."
        );

        Ok(vec![])
    }
}

#[cfg(feature = "bundled")]
mod webrtc {
    use super::*;
    use std::{collections::HashSet, path::Path};

    const BUNDLED_SOURCE_PATH: &str = "./webrtc-audio-processing";

    pub(super) fn get_build_paths() -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut include_paths = vec![
            out_dir().join("include"),
            out_dir().join("include").join(LIB_NAME),
            webrtc_source_dir(),
            webrtc_source_dir().join("webrtc"),
        ];
        // TODO(strohel): instead of hardcoding the paths, we should consult the pkgconfig file that
        // the bundled webrtc-audio-processing build produces.
        let mut lib_paths = vec![
            // MacOS, Arch Linux, baseline default
            out_dir().join("lib"),
            // Ubuntu Linux (our CI)
            out_dir().join("lib").join("x86_64-linux-gnu"),
            // Ubuntu Linux (Arm 64bit)
            out_dir().join("lib").join("aarch64-linux-gnu"),
            // Gentoo Linux (x86_64 multilib)
            out_dir().join("lib64"),
        ];

        // Notes: c8896801 added support for 20250814, but the meson.build is still expecting
        // >=20240722 and the subproject will fetch 20240722. If the build environment has 20250814
        // installed, it should still pick it up and build successfully, though.
        if let Ok(mut lib) =
            pkg_config::Config::new().atleast_version("20240722").probe("absl_base")
        {
            // If abseil package is installed locally, meson would have linked it for
            // webrtc-audio-processing-2. Use the same library for our wrapper, too.
            include_paths.append(&mut lib.include_paths);
            lib_paths.append(&mut lib.link_paths);
        } else {
            // Otherwise use the local build fetched and built by meson.
            include_paths
                .push(webrtc_source_dir().join("subprojects").join("abseil-cpp-20240722.0"));
            lib_paths.push(webrtc_build_dir().join("subprojects").join("abseil-cpp-20240722.0"));
        }

        Ok((include_paths, lib_paths))
    }

    pub(super) fn build_if_necessary() -> Result<()> {
        let bundled_source_path = Path::new(BUNDLED_SOURCE_PATH);
        if bundled_source_path.read_dir()?.next().is_none() {
            eprintln!("The webrtc-audio-processing source directory is empty.");
            eprintln!("See the crate README for installation instructions.");
            eprintln!("Remember to clone the repo recursively if building from source.");
            bail!("Aborting compilation because bundled source directory is empty.");
        }

        let webrtc_source_dir = webrtc_source_dir();
        let webrtc_build_dir = webrtc_build_dir();
        eprintln!(
            "Copying webrtc-audio-processing to {} and building it in {}",
            webrtc_source_dir.display(),
            webrtc_build_dir.display()
        );

        // Copy the sources to under out directory so that we can patch it without consequences.
        let mut cp = Command::new("cp");
        // Copy recursively, preserve attributes. Use trailing dot trick to prevent creating
        // `webrtc-audio-processing/webrtc-audio-processing` nesting on a 2nd invocation.
        cp.arg("-a").arg(bundled_source_path.join(".")).arg(&webrtc_source_dir);
        let status = cp.status().context("executing cp")?;
        assert!(status.success(), "Command failed: {:?}", &cp);

        #[cfg(feature = "experimental-unlink-ns")]
        apply_patch("unlink-multichannel-noise-suppression-filters.patch")?;

        let mut meson = Command::new("meson");
        meson.arg("setup").arg("--prefix").arg(out_dir().as_os_str());
        meson.arg("--reconfigure");

        if cfg!(target_os = "macos") {
            let link_args = "['-framework', 'CoreFoundation', '-framework', 'Foundation']";
            meson.arg(format!("-Dc_link_args={}", link_args));
            meson.arg(format!("-Dcpp_link_args={}", link_args));
        }

        // kaimai patch: 上游 meson.build 把 C++ 标准钉在 c++17，但 WebRTC 的
        // gain_controller2.cc 和 agc2/input_volume_stats_reporter.cc 用了
        // designated initializer（`.field = value`）。GCC/Clang 在 c++17 下当扩展
        // 接受，MSVC 严格拒绝：error C7555。开到 c++20 就全过了。
        if super::target_is_msvc() {
            meson.arg("-Dcpp_std=c++20");
        }

        let status = meson
            .arg("-Ddefault_library=static")
            .arg(webrtc_build_dir.as_os_str())
            .arg(webrtc_source_dir.as_os_str())
            .status()
            .context("Failed to execute meson. Do you have it installed?")?;
        assert!(status.success(), "Command failed: {:?}", &meson);

        let mut ninja = Command::new("ninja");
        let status = ninja
            .current_dir(&webrtc_build_dir)
            .status()
            .context("Failed to execute ninja. Do you have it installed?")?;
        assert!(status.success(), "Command failed: {:?}", &ninja);

        let mut install = Command::new("ninja");
        let status = install
            .current_dir(&webrtc_build_dir)
            .arg("install")
            .status()
            .context("Failed to execute ninja install")?;
        assert!(status.success(), "Command failed: {:?}", &install);

        Ok(())
    }

    // Patch with `patch`.
    #[cfg(feature = "experimental-unlink-ns")]
    fn apply_patch(patch_name: &str) -> Result<()> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let patch = manifest.join("patches").join(patch_name);

        let status = Command::new("patch")
            .args(["-p1", "--forward"])
            .arg("-i")
            .arg(&patch)
            .current_dir(webrtc_source_dir())
            .status()
            .context("Failed to execute patch")?;

        anyhow::ensure!(status.success(), "Patch '{}' failed with status: {}", patch_name, status);
        Ok(())
    }

    /// Prefix symbols in the built webrtc-audio-processing static library.
    /// Returns the list of symbols that were renamed.
    pub(super) fn prefix_library_symbols(
        lib_dirs: &[PathBuf],
        prefix: &str,
    ) -> Result<Vec<String>> {
        // kaimai patch: MSVC 上整个跳过符号前缀。
        //
        // 前缀这套机制只有一个用途：让同一个二进制里能共存多个版本的这个库。
        // 我们不需要。而它在 Windows 上不但没用还有害：
        //
        // - 库是 COFF 归档，llvm-objcopy --redefine-sym 对它不生效（不报错，但没改）
        // - cc-rs 在 MSVC 下产出的是 wrapper 的 .lib，而这段代码只找 .a，
        //   于是库被"前缀"了、wrapper 没有，两边符号对不上，链接时一堆
        //   unresolved external symbol
        //
        // 跳过之后两边都是原始符号，自然对得上。
        if super::target_is_msvc() {
            return Ok(vec![]);
        }

        let static_lib_filename = format!("lib{LIB_NAME}.a");

        for lib_dir in lib_dirs {
            let lib_path = lib_dir.join(&static_lib_filename);
            if lib_path.exists() {
                let symbols = get_defined_symbols(&lib_path)?;
                prefix_archive_symbols(&lib_path, &symbols, prefix)?;
                return Ok(symbols);
            }
        }

        bail!("Cannot find {static_lib_filename} in {lib_dirs:?} to prefix its symbols.");
    }

    fn webrtc_source_dir() -> PathBuf {
        out_dir().join("webrtc-audio-processing")
    }

    fn webrtc_build_dir() -> PathBuf {
        out_dir().join("webrtc-audio-processing-build")
    }

    /// Extract defined (non-external) symbols from a static library using nm.
    fn get_defined_symbols(archive_path: &std::path::Path) -> Result<Vec<String>> {
        // kaimai patch: 不要裸调 PATH 上的 nm。Windows 上根本没有，而 objcopy
        // 那边已经会去 rustc sysroot 里找了（见 determine_objcopy_path），
        // nm 只是漏了。llvm-nm 自称 "compatible with GNU nm"，这两个参数都认。
        let output = Command::new(super::determine_nm_path())
            .arg("--defined-only")
            .arg("--format=posix")
            .arg(archive_path)
            .output()
            .context("Failed to execute nm")?;

        if !output.status.success() {
            anyhow::bail!("nm failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut symbols = HashSet::new();

        for line in stdout.lines() {
            // POSIX format: "symbol_name type value size"
            // We just need the first field (symbol name)
            if let Some(symbol) = line.split_whitespace().next() {
                symbols.insert(symbol.to_string());
            }
        }

        Ok(symbols.into_iter().collect())
    }
}

#[derive(Debug)]
struct CustomDeriveCallbacks;

impl ParseCallbacks for CustomDeriveCallbacks {
    fn add_derives(&self, info: &DeriveInfo) -> Vec<String> {
        // Matches EchoCanceller3Config, EchoCanceller3Config_Suppressor etc
        if info.name.starts_with("EchoCanceller3Config") && cfg!(feature = "serde") {
            vec!["serde::Deserialize".into(), "serde::Serialize".into()]
        // Matches AudioProcessing_Config, AudioProcessing_Config_EchoCanceller etc
        } else if info.name.starts_with("AudioProcessing_Config") {
            // Only derive Default for AudioProcessing_Config and its inner structs. bindgen Default
            // implementation ignores C/C++ struct default values and thus misleading to enable
            // globally. Note that we don't expose these defaults on `webrtc-audio-processing`
            // level: they are needed only by the code that converts from prettified Rust config
            // structs into their FFI variants to construct disabled/dummy values.
            vec!["Default".into()]
        } else {
            vec![]
        }
    }

    fn add_attributes(&self, info: &AttributeInfo<'_>) -> Vec<String> {
        if info.name.starts_with("EchoCanceller3Config") {
            // Prohibit construction of ffi EchoCanceller3Config and its children structs.
            // The only allowed API is through the wrapper struct in the webrtc_audio_processing crate.
            vec!["#[non_exhaustive]".into()]
        } else {
            vec![]
        }
    }
}

fn main() -> Result<()> {
    webrtc::build_if_necessary()?;
    let (include_dirs, lib_dirs) = webrtc::get_build_paths()?;

    // Prefix defined symbols in the webrtc library (bundled builds only)
    // Returns the list of renamed symbols to update wrapper references later
    let renamed_symbols = webrtc::prefix_library_symbols(&lib_dirs, SYMBOL_PREFIX)?;

    // kaimai patch: meson 在 MSVC 下产出的静态库叫 `lib<name>.a`（GNU 风格的文件名，
    // 内容其实是正经的 COFF 归档）。而 `cargo:rustc-link-lib=static=<name>` 在 MSVC
    // 目标下找的是 `<name>.lib`，名字对不上就一个符号都链不到。
    // 复制一份改成 MSVC 的名字，内容完全一样。
    if target_is_msvc() {
        for dir in &lib_dirs {
            let gnu_style = dir.join(format!("lib{LIB_NAME}.a"));
            let msvc_style = dir.join(format!("{LIB_NAME}.lib"));
            if gnu_style.exists() && !msvc_style.exists() {
                std::fs::copy(&gnu_style, &msvc_style)
                    .with_context(|| format!("copying {gnu_style:?} to {msvc_style:?}"))?;
            }
        }
    }

    for dir in &lib_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }

    let mut cc_build = cc::Build::new();

    if cfg!(feature = "experimental-aec3-config") {
        cc_build.define("WEBRTC_AEC3_CONFIG", None);
    }

    // Set macos minimum version
    if cfg!(target_os = "macos") {
        let min_version = match env::var(MACOSX_DEPLOYMENT_TARGET_VAR) {
            Ok(ver) => ver,
            Err(_) => {
                String::from(match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
                    "x86_64" => "10.10", // Using what I found here https://github.com/webrtc-uwp/chromium-build/blob/master/config/mac/mac_sdk.gni#L17
                    "aarch64" => "11.0", // Apple silicon started here.
                    arch => panic!("unknown arch: {}", arch),
                })
            },
        };

        // `cc` doesn't try to pick up on this automatically, but `clang` needs it to
        // generate a "correct" Objective-C symbol table which better matches XCode.
        // See https://github.com/h4llow3En/mac-notification-sys/issues/45.
        cc_build.flag(format!("-mmacos-version-min={}", min_version));
    }

    // This automatically emits "cargo:rustc-link-lib=static=webrtc_audio_processing_wrapper".
    // The wrapper library should be linked before webrtc-audio-processing-2, otherwise strict
    // linkers (like when passing -Wl,--as-needed) may discard the c++ library (automatically
    // added by cc) from the linking list, resulting in build failure.
    // The linking order should respect the dependency graph, i.e. wrapper -> webrtc-2.
    cc_build.cpp(true).file("src/wrapper.cpp").includes(&include_dirs).out_dir(out_dir());

    // kaimai patch: 上游无条件加这两个 GCC 风格参数。cl 把 `-` 当 `/`，于是
    // `-Wno-unused-parameter` 变成无效参数，直接 D8021 编译失败；
    // `-std=c++17` 也不对（MSVC 要 `/std:c++17`，冒号不是等号）。
    // MSVC 下用 c++20 的理由跟上面 meson 那处一样。
    if target_is_msvc() {
        cc_build.flag("/std:c++20");
    } else {
        cc_build.flag("-std=c++17").flag("-Wno-unused-parameter");
    }

    // Inform wrapper code that headers for internal classes (ResidualEchoDetector) are available.
    #[cfg(feature = "bundled")]
    cc_build.define("WEBRTC_HAS_INTERNAL_HEADERS", None);

    cc_build.compile("webrtc_audio_processing_wrapper");

    // The the cc and bindgen commands emit `cargo:rerun-if-env-changed=...`, and these deactivate
    // the default behavior to rerun if _any_ source file changes. So state these explicitly.
    // build.rs is always included and doesn't have to be specified.
    println!("cargo:rerun-if-changed=src/wrapper.hpp");
    println!("cargo:rerun-if-changed=src/wrapper.cpp");

    // Prefix the wrapper library's references to webrtc symbols to match the renamed webrtc library.
    let wrapper_lib = out_dir().join("libwebrtc_audio_processing_wrapper.a");
    if wrapper_lib.exists() {
        prefix_archive_symbols(&wrapper_lib, &renamed_symbols, SYMBOL_PREFIX)?;
    }

    if cfg!(feature = "bundled") {
        println!("cargo:rustc-link-lib=static={LIB_NAME}");
        println!("cargo:rustc-link-lib=absl_strings");
    } else {
        println!("cargo:rustc-link-lib=dylib={LIB_NAME}");
    }

    let binding_file = out_dir().join("bindings.rs");
    let mut builder = bindgen::Builder::default()
        .header("src/wrapper.hpp")
        .clang_args(&["-x", "c++", "-std=c++17", "-fparse-all-comments"])
        .generate_comments(true)
        .enable_cxx_namespaces();

    builder = builder
        // Transitive dependencies are automatically included.
        .allowlist_function("webrtc_audio_processing_wrapper::.*")
        .opaque_type("std::.*")
        .parse_callbacks(Box::new(CustomDeriveCallbacks))
        .derive_debug(true)
        // The default implementation ignores C++11's brace-or-equal-initializers,
        // and thus misleading to enable. See also CustomDeriveCallbacks.
        .derive_default(false)
        .derive_partialeq(true);
    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }

    // kaimai patch: 新版 MSVC 的 STL 头（yvals_core.h）里有个 static_assert，
    // 要求 clang 版本足够新，否则直接 "error STL1000: Unexpected compiler version"。
    // VS 自带的 libclang 通常比这个要求老一截，于是 bindgen 连头文件都解析不了。
    //
    // `_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` 是 MSVC 官方给的逃生开关。
    // 这里用它是安全的：bindgen 只解析头文件生成 FFI 声明，不编译 STL 本身，
    // 真正编译 wrapper.cpp 的是 cl.exe，那条路上编译器和 STL 是配套的。
    if target_is_msvc() {
        builder = builder.clang_arg("-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH");
    }
    builder
        .generate()
        .expect("Unable to generate bindings")
        .write_to_file(&binding_file)
        .expect("Couldn't write bindings!");

    Ok(())
}

/// Reliably determine a path to objcopy binary bundled with the active Rust toolchain (rust-objcopy)
/// kaimai patch: 按 determine_objcopy_path 的同样办法找 nm。
///
/// 优先用 rustc sysroot 里的 llvm-nm（`rustup component add llvm-tools`），
/// 找不到就退回 PATH 上的 `nm`（Linux/macOS 上本来就有）。
fn determine_nm_path() -> PathBuf {
    let fallback = PathBuf::from("nm");

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let Ok(output) = Command::new(&rustc).arg("--print").arg("sysroot").output() else {
        return fallback;
    };
    if !output.status.success() {
        return fallback;
    }
    let Ok(sysroot) = String::from_utf8(output.stdout) else {
        return fallback;
    };
    let Ok(host) = env::var("HOST") else {
        return fallback;
    };

    let bin = PathBuf::from(sysroot.trim()).join("lib").join("rustlib").join(host).join("bin");
    for name in ["llvm-nm.exe", "llvm-nm"] {
        let candidate = bin.join(name);
        if candidate.exists() {
            return candidate;
        }
    }

    println!(
        "cargo:warning=llvm-nm not found in the rustc sysroot; falling back to `nm` on PATH.          On Windows run: rustup component add llvm-tools"
    );
    fallback
}

fn determine_objcopy_path() -> Result<PathBuf> {
    // 1. Get the rustc command (this might be a path or just "rustc")
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());

    // 2. Ask rustc for the sysroot. This works even if RUSTC="rustc"
    let output = Command::new(&rustc)
        .arg("--print")
        .arg("sysroot")
        .output()
        .context("Failed to execute rustc to find sysroot")?;

    if !output.status.success() {
        bail!("Failed to get sysroot from rustc: {:?}", output);
    }

    let sysroot_str = String::from_utf8(output.stdout).context("Invalid UTF-8 in sysroot")?;
    let sysroot = PathBuf::from(sysroot_str.trim());

    // 3. Construct the path: <sysroot>/lib/rustlib/<HOST_TRIPLE>/bin/rust-objcopy
    // We use HOST because that is where the compiler (and tools) are running.
    let host = env::var("HOST").context("HOST env var not found")?;

    let objcopy = sysroot.join("lib").join("rustlib").join(host).join("bin").join("rust-objcopy");

    // Optional: verification
    if !objcopy.exists() {
        println!("cargo:warning=rust-objcopy not found at {:?}", objcopy);
        println!("cargo:warning=Ensure the 'llvm-tools' component is installed: 'rustup component add llvm-tools'");
    }

    Ok(objcopy)
}

// SPDX-License-Identifier: MIT OR Apache-2.0

//! 把 `proto/control.proto` 编成 Rust。
//!
//! 用 protox 而不是 prost-build 的默认路径，是为了**不依赖 protoc**。
//! protox 是纯 Rust 的 .proto 编译器，`cargo build` 就够了，不需要贡献者
//! 先去装一个外部二进制 —— M2 在 meson/ninja 上已经吃够苦头了。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTO: &str = "proto/control.proto";

    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-changed=build.rs");

    let descriptors = protox::compile([PROTO], ["proto"])?;

    // prost 默认就会把 .proto 里的注释搬成 Rust 文档注释 ——
    // 协议的解释只写一份，不要在 .proto 和 Rust 侧各维护一套然后慢慢对不上。
    prost_build::Config::new().compile_fds(descriptors)?;

    Ok(())
}

// SPDX-License-Identifier: GPL-3.0-or-later

//! 从 `ui/icons/flame.svg` 生成 `ui/icons/gouhuo.ico`：exe 自己的图标、开始菜单快捷方式、
//! 「添加或删除程序」里显示的都是它。
//!
//! 改了 SVG 之后跑一次，把生成的 .ico 一起提交：
//!
//! ```text
//! cargo run -p client --example make_icon
//! ```
//!
//! 一个 .ico 里放好几个尺寸，系统按场合挑：16（任务栏小图标、标题栏）、24、32（桌面默认）、
//! 48、64、128、256（大图标视图）。每个尺寸都从矢量图重新画，而不是拿大的缩小 ——
//! 小尺寸下缩出来的边缘是糊的。每一张都是 PNG 压缩的，Vista 之后都认。

use std::path::Path;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/icons");
    let svg = std::fs::read(dir.join("flame.svg")).expect("读不到 flame.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
        .expect("flame.svg 解析不了");

    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("尺寸不对");
            let scale = size as f32 / tree.size().width().max(tree.size().height());
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            (size, pixmap.encode_png().expect("PNG 编码失败"))
        })
        .collect();

    let ico = pack_ico(&images);
    let out = dir.join("gouhuo.ico");
    std::fs::write(&out, &ico).expect("写不了 gouhuo.ico");
    println!(
        "写好了 {}（{} 个尺寸，{} 字节）",
        out.display(),
        images.len(),
        ico.len()
    );
}

/// ICO 文件：一个 6 字节的头，每张图一条 16 字节的目录项，然后是各张图的 PNG 数据。
fn pack_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // 保留
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = 图标（2 是光标）
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in images {
        // 宽高是一个字节，256 写成 0。
        let edge = if *size >= 256 { 0 } else { *size as u8 };
        out.push(edge);
        out.push(edge);
        out.push(0); // 调色板颜色数：不用调色板
        out.push(0); // 保留
        out.extend_from_slice(&1u16.to_le_bytes()); // 色彩平面
        out.extend_from_slice(&32u16.to_le_bytes()); // 每像素位数
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in images {
        out.extend_from_slice(png);
    }
    out
}

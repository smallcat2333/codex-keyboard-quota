//! 生成并嵌入 Windows 可执行文件图标。

use std::env;
use std::fs;
use std::path::PathBuf;

/// 构建 Windows 资源；其他平台仅声明重建条件。
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR 未设置"));
    let icon_path = output_dir.join("codex_keyboard_quota.ico");
    fs::write(&icon_path, build_icon()).expect("无法生成应用图标");

    let mut resource = winres::WindowsResource::new();
    resource.set_icon(icon_path.to_str().expect("图标路径不是 UTF-8"));
    resource
        .set("ProductName", "Codex 键盘额度工具")
        .set("FileDescription", "Codex 键盘额度工具")
        .set("LegalCopyright", "smallcat")
        .set("FileVersion", "0.2.0.0")
        .set("ProductVersion", "0.2.0.0");
    resource.compile().expect("无法编译 Windows 资源");
}

/// 生成一个 32×32、32 位色的 ICO 文件，避免分发额外图标资产。
fn build_icon() -> Vec<u8> {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    const HEADER_SIZE: u32 = 40;
    const PIXEL_BYTES: u32 = (WIDTH * HEIGHT * 4) as u32;
    const MASK_BYTES: u32 = (HEIGHT * 4) as u32;
    const IMAGE_SIZE: u32 = HEADER_SIZE + PIXEL_BYTES + MASK_BYTES;

    let mut icon = Vec::with_capacity(22 + IMAGE_SIZE as usize);
    push_u16(&mut icon, 0);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 1);
    icon.extend_from_slice(&[WIDTH as u8, HEIGHT as u8, 0, 0]);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 32);
    push_u32(&mut icon, IMAGE_SIZE);
    push_u32(&mut icon, 22);

    push_u32(&mut icon, HEADER_SIZE);
    push_i32(&mut icon, WIDTH as i32);
    push_i32(&mut icon, (HEIGHT * 2) as i32);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 32);
    push_u32(&mut icon, 0);
    push_u32(&mut icon, PIXEL_BYTES);
    push_i32(&mut icon, 0);
    push_i32(&mut icon, 0);
    push_u32(&mut icon, 0);
    push_u32(&mut icon, 0);

    for source_y in (0..HEIGHT).rev() {
        for x in 0..WIDTH {
            let border = !(2..=29).contains(&x) || !(2..=29).contains(&source_y);
            let key_line = (6..=25).contains(&x) && matches!(source_y, 8 | 15 | 22)
                || (6..=25).contains(&source_y) && matches!(x, 6 | 12 | 19 | 25);
            let quota_mark = (11..=20).contains(&x)
                && ((10..=12).contains(&source_y) || (19..=21).contains(&source_y));
            let (red, green, blue) = if border {
                (139, 92, 246)
            } else if quota_mark {
                (52, 211, 153)
            } else if key_line {
                (71, 85, 105)
            } else {
                (17, 24, 39)
            };
            icon.extend_from_slice(&[blue, green, red, 255]);
        }
    }
    icon.resize(icon.len() + MASK_BYTES as usize, 0);
    icon
}

/// 以小端序追加 16 位无符号整数。
fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

/// 以小端序追加 32 位无符号整数。
fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

/// 以小端序追加 32 位有符号整数。
fn push_i32(buffer: &mut Vec<u8>, value: i32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

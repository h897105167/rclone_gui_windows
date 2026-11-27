//! 应用图标：一份画法，三处用途。
//!
//! 托盘和窗口要 RGBA 像素，exe 的图标资源要 `.ico` 文件，安装包要的也是那个 `.ico`。
//! 三者共用这里的画法，所以托盘、任务栏、快捷方式看到的永远是同一个图案。
//!
//! 为什么用代码画而不是放 PNG：图标要跟随强调色变，还要能出任意尺寸。
//! 画一次就够，不必维护一堆位图，也不会出现「PNG 换了这个地方忘了换」的偏差。
//!
//! `.ico` 里每一档尺寸都是 32 位 BMP（带 alpha 和 AND 掩码），不做 PNG 压缩条目：
//! PNG 条目需要额外的编码器依赖（编解码两边都要），而 128px 以内 BMP 加起来只有
//! 一百多 KB，比多一个依赖便宜。

// 这个文件有两个使用者：运行时（本 crate）只用 `make_icon_rgba` 画 HICON，
// 而 `build.rs` 用 `#[path]` 把整个文件编进去，靠 `ico_file` 生成 exe 与安装包的图标。
// 所以对 bin 目标来说 `.ico` 那几个函数「没人调用」，但删掉构建就断——不是死代码。
#![allow(dead_code)]

/// 画一张 `size × size` 的 RGBA 图标，行序自上而下。
///
/// 造型与 Slint 里的 `AppMark` 保持一致：圆角方块底 + 白色云 + 云内的终端窗口。
/// `size` 为 0 时返回空表；任意正尺寸都能画（16 到 256 都试过）。
pub fn make_icon_rgba(size: usize, accent: [u8; 3]) -> Vec<u8> {
    let mut buf = vec![0u8; size * size * 4];
    let s = size as f32;
    let radius = s * 0.24; // 与 AppMark 的 border-radius 一致
    let (ar, ag, ab) = (accent[0], accent[1], accent[2]);

    // 圆角方块内部判定：四个角按圆角半径切掉
    let in_rounded_rect = |x: f32, y: f32| -> bool {
        let (cx, cy) = (x.clamp(radius, s - radius), y.clamp(radius, s - radius));
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= radius * radius
            || (x >= radius && x <= s - radius)
            || (y >= radius && y <= s - radius)
    };

    // 三个圆的并集构成云（与 AppMark 的几何比例一致）
    let circle = |x: f32, y: f32, cx: f32, cy: f32, r: f32| -> bool {
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= r * r
    };
    let in_cloud = |x: f32, y: f32| -> bool {
        // 左圆
        circle(x, y, s * 0.35, s * 0.53, s * 0.21)
            // 右大圆
            || circle(x, y, s * 0.63, s * 0.45, s * 0.25)
            // 底部横条：用矩形近似（原 Slint 版本是圆角条）
            || (x >= s * 0.14 && x <= s * 0.88 && y >= s * 0.52 && y <= s * 0.74)
    };
    // 云内终端窗口（主色挖空）
    let in_terminal = |x: f32, y: f32| -> bool {
        let (x0, y0) = (s * 0.30, s * 0.44);
        let (w, h) = (s * 0.46, s * 0.34);
        let r = s * 0.07;
        if x < x0 || x > x0 + w || y < y0 || y > y0 + h {
            return false;
        }
        let (cx, cy) = (x.clamp(x0 + r, x0 + w - r), y.clamp(y0 + r, y0 + h - r));
        let dx = x - cx;
        let dy = y - cy;
        dx * dx + dy * dy <= r * r
            || (x >= x0 + r && x <= x0 + w - r)
            || (y >= y0 + r && y <= y0 + h - r)
    };
    // 终端里的 ">_"：用斜线 + 横线近似，小尺寸下也能看出是命令行
    let in_chevron = |x: f32, y: f32| -> bool {
        let (bx, by) = (s * 0.355, s * 0.585); // 尖角位置
                                               // 上斜边：从左上到尖角
        let up = (y >= by - s * 0.075 && y <= by) && (x - (bx - (by - y) * 0.9)).abs() <= s * 0.035;
        // 下斜边：从尖角到左下
        let dn = (y >= by && y <= by + s * 0.075) && (x - (bx - (y - by) * 0.9)).abs() <= s * 0.035;
        up || dn
    };
    let in_underscore = |x: f32, y: f32| -> bool {
        x >= s * 0.53 && x <= s * 0.67 && y >= s * 0.645 && y <= s * 0.685
    };

    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32, y as f32);
            let i = (y * size + x) * 4;
            if !in_rounded_rect(fx, fy) {
                continue; // 方块外保持透明
            }
            let (r, g, b) = if in_terminal(fx, fy) {
                if in_chevron(fx, fy) || in_underscore(fx, fy) {
                    (255, 255, 255)
                } else {
                    (ar, ag, ab) // 终端底色 = 主色
                }
            } else if in_cloud(fx, fy) {
                (255, 255, 255)
            } else {
                (ar, ag, ab)
            };
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = 255;
        }
    }
    buf
}

/// 把一张 RGBA 图包成 Windows 图标资源里那一段 DIB。
///
/// 格式是图标规定的样子，和普通 BMP 有两处不同，写错了图标就会糊掉或者整块黑：
/// - `biHeight` 写两倍高度：文件里「彩色位图 + 掩码位图」上下拼在一起算一张。
/// - 每行都要 4 字节对齐：1bpp 的掩码行因此要补到 `(size+31)/32*4` 字节。
///
/// 彩色部分用 BGRA 且自下而上（BMP 的老规矩）；全透明像素在掩码里置 1，
/// 这样只看掩码、不看 alpha 的老程序也不会给它画一圈黑边。
///
/// `rgba` 必须正好是 `size × size × 4` 字节，否则 panic——这是编程错误，不是运行时输入。
pub fn dib_image(size: usize, rgba: &[u8]) -> Vec<u8> {
    assert_eq!(rgba.len(), size * size * 4, "RGBA 缓冲区尺寸与 size 不符");
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + size * size * 4 + mask_row * size);

    // BITMAPINFOHEADER（40 字节）
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((size as i32) * 2).to_le_bytes()); // biHeight：彩色 + 掩码
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((size * size * 4) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // 分辨率 + 调色板字段，图标不用

    // XOR 位图：BGRA，自下而上
    for y in (0..size).rev() {
        for x in 0..size {
            let i = (y * size + x) * 4;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }

    // AND 掩码：1bpp，自下而上，全透明处为 1
    for y in (0..size).rev() {
        let row = out.len();
        out.resize(row + mask_row, 0);
        for x in 0..size {
            if rgba[(y * size + x) * 4 + 3] == 0 {
                out[row + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    out
}

/// 打包成 `.ico` 文件，`sizes` 里的每一档都完整画一遍。
///
/// 同一档尺寸重复出现不会报错，只是多占一段数据；实际用途里各档尺寸应互不相同。
pub fn ico_file(sizes: &[usize], accent: [u8; 3]) -> Vec<u8> {
    let images: Vec<(usize, Vec<u8>)> = sizes
        .iter()
        .map(|&s| (s, dib_image(s, &make_icon_rgba(s, accent))))
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // ICONDIR.reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // ICONDIR.type = 1（图标）
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    // 目录项共 14 + 2 字节由上式补齐到 16 字节，顺序与后面的数据一一对应
    let mut offset = 6 + 16 * images.len();
    for (size, data) in &images {
        // 256 在这里写 0（一个字节放不下 256）
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(if *size >= 256 { 0 } else { *size as u8 });
        out.push(0); // 调色板颜色数，真彩写 0
        out.push(0); // 保留字节
        out.extend_from_slice(&1u16.to_le_bytes()); // 平面数
        out.extend_from_slice(&32u16.to_le_bytes()); // 位深
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// exe 图标资源与安装包用的尺寸档位。
///
/// 覆盖面按 Windows 的取图场景挑：16（小图标、托盘）、20/24（任务栏，125%/150% 缩放）、
/// 32（桌面默认）、40/48（中图标）、64/128（大图标与 Alt+Tab）。再大的档位由系统放大顶上，
/// 256px 那档要多占两百多 KB，不值得。
pub const APP_ICON_SIZES: &[usize] = &[16, 20, 24, 32, 40, 48, 64, 128];

/// 写进 exe 图标资源和安装包的强调色（深色主题的默认蓝）。
///
/// 构建期读不到用户的配色设置，只能用默认值。运行期的托盘与窗口图标另按用户选的
/// 强调色现画，所以任务栏在程序运行时仍是用户的主色；这个常量管的是资源管理器、
/// 快捷方式和安装向导里看到的那个图标。
pub const ICON_ACCENT: [u8; 3] = [0x58, 0xa6, 0xff];

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: [u8; 3] = [0x58, 0xa6, 0xff];

    /// 图标必须真的画出内容，而不是全透明。
    ///
    /// 这条测试的价值：图标生成是纯几何运算，写错一个坐标就可能整张空白，
    /// 而空白图标在界面里表现为「任务栏没图标」，很难往几何 bug 上想。
    #[test]
    fn icon_has_visible_pixels() {
        let rgba = make_icon_rgba(32, BLUE);
        assert_eq!(rgba.len(), 32 * 32 * 4, "RGBA 缓冲区大小必须匹配尺寸");
        let opaque = rgba.chunks_exact(4).filter(|p| p[3] == 255).count();
        // 圆角方块占据大部分面积，至少应有 50% 不透明像素
        assert!(
            opaque > 32 * 32 / 2,
            "不透明像素太少（{opaque}），图标可能是空白的"
        );
    }

    /// 圆角：四个角必须是透明的，否则就不是圆角方块。
    #[test]
    fn icon_corners_are_rounded() {
        let size = 32;
        let rgba = make_icon_rgba(size, BLUE);
        let alpha_at = |x: usize, y: usize| rgba[(y * size + x) * 4 + 3];
        assert_eq!(alpha_at(0, 0), 0, "左上角应透明（圆角）");
        assert_eq!(alpha_at(size - 1, 0), 0, "右上角应透明（圆角）");
        assert_eq!(alpha_at(0, size - 1), 0, "左下角应透明（圆角）");
        assert_eq!(alpha_at(size - 1, size - 1), 0, "右下角应透明（圆角）");
        // 中心必须不透明
        assert_eq!(alpha_at(size / 2, size / 2), 255);
    }

    /// 强调色必须真正作用到图标上（否则「随强调色变色」就是假的功能）。
    #[test]
    fn icon_takes_accent_colour() {
        let blue = make_icon_rgba(32, BLUE);
        let green = make_icon_rgba(32, [0x3f, 0xb9, 0x50]);
        assert_ne!(blue, green, "不同强调色必须生成不同图标");
    }

    /// 任意尺寸都要能画（托盘 32 / 任务栏 64 / 小图标 16）。
    #[test]
    fn icon_generates_at_multiple_sizes() {
        for size in [16usize, 32, 64] {
            let rgba = make_icon_rgba(size, BLUE);
            assert_eq!(rgba.len(), size * size * 4);
            let opaque = rgba.chunks_exact(4).filter(|p| p[3] == 255).count();
            assert!(opaque > 0, "{size}px 图标不能是空白的");
        }
    }

    /// DIB 头部字段必须是图标规范要的那一组，否则系统会当成别的位图。
    #[test]
    fn dib_header_matches_icon_format() {
        let size = 32usize;
        let dib = dib_image(size, &make_icon_rgba(size, BLUE));
        let u32_at = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(dib[o..o + 2].try_into().unwrap());
        assert_eq!(u32_at(0), 40, "biSize 必须是 40");
        assert_eq!(u32_at(4), size as u32, "biWidth 等于边长");
        assert_eq!(
            u32_at(8),
            (size * 2) as u32,
            "biHeight 是两倍高（彩色 + 掩码）"
        );
        assert_eq!(u16_at(12), 1, "biPlanes 必须是 1");
        assert_eq!(u16_at(14), 32, "biBitCount 必须是 32");
        assert_eq!(u32_at(16), 0, "biCompression 必须是 BI_RGB");
        // 40 字节头 + 彩色位图 + 一行对齐后的掩码 × 行数
        let mask_row = size.div_ceil(32) * 4;
        assert_eq!(dib.len(), 40 + size * size * 4 + mask_row * size);
    }

    /// 位图自下而上、颜色通道是 BGRA：这两点错了图标会上下颠倒/红蓝互换。
    #[test]
    fn dib_rows_are_bottom_up_bgra() {
        let size = 2usize;
        // 左上角像素设为红色，其余透明
        let mut rgba = vec![0u8; size * size * 4];
        rgba[0] = 0xff; // R
        rgba[1] = 0x11; // G
        rgba[2] = 0x22; // B
        rgba[3] = 0xff; // A
        let dib = dib_image(size, &rgba);
        // 2×2 的彩色位图共 16 字节，最后一行的第一个像素在偏移 40+8 处（自下而上的末行 = 顶行）
        let top_row = 40 + size * 4;
        assert_eq!(
            &dib[top_row..top_row + 4],
            &[0x22, 0x11, 0xff, 0xff],
            "顶行应出现在最后，且通道顺序是 BGRA"
        );
    }

    /// 全透明像素在 AND 掩码里要置位，否则老程序会把它画成黑块。
    #[test]
    fn dib_mask_marks_transparent_pixels() {
        let size = 32usize;
        let rgba = make_icon_rgba(size, BLUE);
        let dib = dib_image(size, &rgba);
        let mask_row = size.div_ceil(32) * 4;
        let mask = &dib[40 + size * size * 4..];
        // 左上角是圆角外（透明），掩码第一行最高位应为 1
        let first_mask_byte = mask[mask.len() - mask_row];
        assert_eq!(first_mask_byte & 0x80, 0x80, "透明角必须在掩码里标记出来");
        // 中心不透明，掩码对应位应为 0。行是自下而上写的，第 y 行从
        // `mask.len() - mask_row * (y + 1)` 开始。
        let mid_y = size / 2;
        let mid_byte = mask[mask.len() - mask_row * (mid_y + 1) + (size / 2) / 8];
        assert_eq!(mid_byte & (0x80 >> ((size / 2) % 8)), 0, "中心不应被掩掉");
    }

    /// `.ico` 目录项与数据偏移要能对上：偏移错了系统整张图读不出来。
    #[test]
    fn ico_container_is_self_consistent() {
        let sizes = [16usize, 32, 64];
        let ico = ico_file(&sizes, BLUE);
        let u16_at = |o: usize| u16::from_le_bytes(ico[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(ico[o..o + 4].try_into().unwrap());
        assert_eq!(u16_at(0), 0, "保留字段为 0");
        assert_eq!(u16_at(2), 1, "类型必须是图标");
        assert_eq!(u16_at(4), sizes.len() as u16, "目录项数量");

        for (i, size) in sizes.iter().enumerate() {
            let e = 6 + i * 16;
            assert_eq!(ico[e] as usize, *size, "宽字段");
            assert_eq!(ico[e + 1] as usize, *size, "高字段");
            assert_eq!(u16_at(e + 4), 1, "平面数");
            assert_eq!(u16_at(e + 6), 32, "位深");
            let len = u32_at(e + 8) as usize;
            let off = u32_at(e + 12) as usize;
            assert!(off + len <= ico.len(), "第 {i} 段数据越界");
            // 每段都必须以 BITMAPINFOHEADER 的 biSize=40 开头
            assert_eq!(u32_at(off), 40, "第 {i} 段不是 DIB");
            assert_eq!(u32_at(off + 8), (*size * 2) as u32, "第 {i} 段高度不对");
        }
        // 最后一段必须刚好用完文件
        let last = 6 + (sizes.len() - 1) * 16;
        assert_eq!(
            u32_at(last + 12) as usize + u32_at(last + 8) as usize,
            ico.len()
        );
    }

    /// 把生成的图标导出为 PNG，供人工确认造型（非断言式测试）。
    ///
    /// 跑法：`cargo test -p rclone-gui dump_icon_png -- --ignored --nocapture`
    /// 产物在 `target/rcgui-icon-*.png`。
    /// 注意不能写 `%TEMP%`：本机沙箱下它不可写（os error 5），
    /// 项目里既有测试也因此统一把临时目录放在测试可执行文件旁。
    #[test]
    #[ignore]
    fn dump_icon_png() {
        let base = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir);
        for (size, name) in [(64usize, "64"), (32, "32")] {
            let rgba = make_icon_rgba(size, BLUE);
            let mut p = base.join(format!("rcgui-icon-{name}.png"));
            let buf =
                image::RgbaImage::from_raw(size as u32, size as u32, rgba).expect("RGBA 尺寸匹配");
            // 失败时退到 CWD，避免整个测试因目录权限问题失败
            if buf.save(&p).is_err() {
                p = std::path::PathBuf::from(format!("rcgui-icon-{name}.png"));
                buf.save(&p).expect("保存 PNG 失败");
            }
            eprintln!("wrote {}", p.display());
        }
    }
}

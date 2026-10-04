// 主链路探针：draw_panel 渲染表情网格，检查 😀 的像素通道序
use xime_ui::iced_view::IcedSurface;
use xime_ui::menu::*;
use xime_ui::theme::PanelTheme;
use xime_ui::CandidateItem;

fn main() {
    let theme = PanelTheme::for_mode(false, 14.0, 8.0, (0x1A, 0x73, 0xE8));
    let candidates = vec![CandidateItem {
        text: "你好".into(),
        comment: String::new(),
        index: 0,
    }];
    let mut surface = IcedSurface::new();
    let emoji = "😀"; // Noto Color Emoji: 黄脸 R~255 G~221 B~0
    let grid = PanelGrid {
        source: PanelPage::Emoji,
        tab: 1,
        page: 0,
        recent: vec![],
        cells: (0..32).map(|_| Some(emoji.to_string())).collect(),
        item_count: 32,
        tabs: vec!["最近".into(), "常用".into()],
        has_pager: false,
    };
    let w = 320u32.max(PANEL_MIN_WIDTH);
    let h = theme.bar_height() + grid_panel_height(2, false);
    let mut px = vec![0u8; (w * h * 4) as usize];
    surface.draw_panel(
        &mut px,
        w,
        h,
        &candidates,
        0,
        &theme,
        Some(PanelPage::Emoji),
        &PanelList::default(),
        &grid,
    );
    // 第 0 格中心：网格顶 = bar + grid_top()，格高 36 → 中心 y = bar + 44 + 18
    let cy = theme.bar_height() as usize + 44 + 18;
    let cx = 10 + (w as usize - 20 - 7 * 4) / 8 / 2;
    // 在格子内扫描最饱和的像素（避开透明区）
    let mut best = (0usize, 0u8, 0u8, 0u8, 0u32);
    for dy in -12..12i32 {
        for dx in -12..12i32 {
            let y = cy as i32 + dy;
            let x = cx as i32 + dx;
            if x < 0 || y < 0 {
                continue;
            }
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            let (b, g, r, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
            let sat = (r.abs_diff(b) as u32) + (g.abs_diff(b) as u32);
            if a > 200 && sat > best.4 {
                best = (i, r, g, b, sat);
            }
        }
    }
    let (i, r, g, b, _sat) = best;
    println!(
        "最饱和像素 @idx{i}: byte0={b} byte1={g} byte2={r} byte3(alpha)={}",
        px[i + 3]
    );
    println!("😀 黄脸期望: R~255 G~221 B~0");
    println!("  若 byte0(R 位置)~255 → pixels 实为 RGBA（R/B 反）");
    println!("  若 byte2(R 位置)~255 → pixels 为 BGRA（正确）");
}

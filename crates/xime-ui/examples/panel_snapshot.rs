// 离屏渲染面板三页快照，用于 UI 审查
use xime_ui::iced_view::IcedSurface;
use xime_ui::menu::*;
use xime_ui::theme::PanelTheme;
use xime_ui::CandidateItem;

fn main() {
    let theme = PanelTheme::for_mode(false, 14.0, 8.0, (0x1A, 0x73, 0xE8));
    let candidates: Vec<CandidateItem> = vec![CandidateItem {
        text: "你好".into(),
        comment: String::new(),
        index: 0,
    }];
    let mut surface = IcedSurface::new();

    // 1. 菜单页
    let (w, h) = (400u32, theme.bar_height() + menu_panel_height());
    let mut px = vec![0u8; (w * h * 4) as usize];
    surface.draw_panel(
        &mut px,
        w,
        h,
        &candidates,
        0,
        &theme,
        Some(PanelPage::Menu),
        &PanelList::default(),
        &PanelGrid::default(),
    );
    write_bmp("/tmp/xime-ui-snap/menu.bmp", &px, w, h);

    // 2. 快捷发送列表页（空 + 有数据）
    let list = PanelList {
        source: PanelPage::QuickSend,
        items: vec![
            PanelListItem {
                text: "你好，在吗？".into(),
                code: "nhzm".into(),
            },
            PanelListItem {
                text: "收到，马上处理。".into(),
                code: "sdmscl".into(),
            },
        ],
        page: 0,
    };
    let (w2, h2) = (400u32, theme.bar_height() + list_panel_height());
    let mut px2 = vec![0u8; (w2 * h2 * 4) as usize];
    surface.draw_panel(
        &mut px2,
        w2,
        h2,
        &candidates,
        0,
        &theme,
        Some(PanelPage::QuickSend),
        &list,
        &PanelGrid::default(),
    );
    write_bmp("/tmp/xime-ui-snap/quicksend.bmp", &px2, w2, h2);

    // 3. 表情页（网格）
    let emoji_set = [
        "😀", "😄", "😁", "😂", "🤣", "😊", "😍", "🥰", "😘", "😎", "🤔", "🙄", "😅", "😇", "😉",
        "😌", "😴", "😭", "😢", "😡", "😱", "🥺", "🤗", "🤝", "🙏", "👍", "👌", "✌️", "👏", "🎉",
        "❤️", "🔥",
    ];
    let grid = PanelGrid {
        source: PanelPage::Emoji,
        tab: 1,
        page: 0,
        recent: vec!["😀".into(), "🎉".into()],
        cells: emoji_set.iter().map(|e| Some(e.to_string())).collect(),
        item_count: 32,
        tabs: vec!["最近".into(), "常用".into()],
        has_pager: false,
    };
    let (w3, h3) = (400u32, theme.bar_height() + grid_panel_height(2, false));
    let mut px3 = vec![0u8; (w3 * h3 * 4) as usize];
    surface.draw_panel(
        &mut px3,
        w3,
        h3,
        &candidates,
        0,
        &theme,
        Some(PanelPage::Emoji),
        &PanelList::default(),
        &grid,
    );
    write_bmp("/tmp/xime-ui-snap/emoji.bmp", &px3, w3, h3);

    println!("done");
}

fn write_bmp(path: &str, px: &[u8], w: u32, h: u32) {
    // BGRA → BMP（BGRA 是 BMP 原生格式，只需要文件头）
    let stride = (w * 4) as usize;
    let pixel_size = stride * h as usize;
    let file_size = 54 + pixel_size;
    let mut buf = Vec::with_capacity(file_size);
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&(file_size as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&54u32.to_le_bytes());
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(&(w as i32).to_le_bytes());
    buf.extend_from_slice(&(h as i32).to_le_bytes()); // 正数 = 底朝上
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&32u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&(pixel_size as u32).to_le_bytes());
    buf.extend_from_slice(&2835u32.to_le_bytes());
    buf.extend_from_slice(&2835u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    // iced 输出是顶朝下，BMP 底朝上 → 翻转行序
    for y in (0..h as usize).rev() {
        buf.extend_from_slice(&px[y * stride..(y + 1) * stride]);
    }
    std::fs::write(path, buf).unwrap();
}

//! 候选栏菜单面板的纯逻辑：常量、命中测试、菜单项定义。
//!
//! 菜单按钮是候选栏最右侧固定区域（36x36），绘制九宫格 SVG 图标；
//! 点击后候选栏向下展开：候选栏与菜单同处一个圆角容器内，
//! 菜单双列显示（表情/符号/剪切板/快捷发送）。
//! 渲染由 `iced_view` 承担（iced 离屏绘制），本模块不涉及绘制。
/// 菜单按钮区域宽度。
pub const MENU_BUTTON_WIDTH: u32 = 36;

/// 候选栏高度（与 daemon 一致）。
pub const CANDIDATE_HEIGHT: u32 = 36;

/// 菜单行高。
pub const MENU_ITEM_HEIGHT: u32 = 44;

/// 菜单列数。
pub const MENU_COLUMNS: usize = 2;

/// 内容面板（表情/符号网格）列数上限。
pub const CONTENT_COLUMNS_MAX: usize = 10;

/// 内容面板网格行数（固定，空位留白）。
pub const CONTENT_ROWS: usize = 3;

/// 内容面板单元格边长下限。
pub const CONTENT_ITEM_SIZE: u32 = 36;

/// 内容面板网格间距。
pub const CONTENT_GAP: u32 = 6;

/// 内容面板最大期望宽度（超出则减少列数）。
pub const CONTENT_MAX_WIDTH: u32 = 660;

/// 文本渲染宽度估算（16px 字号），用于内容网格单元格定宽。
///
/// 组合标记/变体选择符/键帽等零宽字符按 0 计；估算偏保守（偏大），
/// 避免单元格不够宽导致换行。
pub fn content_text_width(text: &str) -> u32 {
    text.chars()
        .map(|c| match c {
            '\u{0300}'..='\u{036F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{20E3}'
            | '\u{1F3FB}'..='\u{1F3FF}' => 0,
            c if c.is_ascii() => 10,
            _ => 17,
        })
        .sum()
}

/// 内容网格单元格边长：最宽项 + 内边距，下限 CONTENT_ITEM_SIZE。
pub fn content_cell_width(widest: u32) -> u32 {
    (widest + 16).max(CONTENT_ITEM_SIZE)
}

/// 内容网格列数：在 CONTENT_MAX_WIDTH 内尽量多列（4..=10）。
pub fn content_columns_for(cell_width: u32) -> usize {
    let cols = (CONTENT_MAX_WIDTH + CONTENT_GAP) / (cell_width + CONTENT_GAP);
    cols.clamp(4, CONTENT_COLUMNS_MAX as u32) as usize
}

/// 内容面板渲染宽度。
pub fn content_panel_width(cell_width: u32, columns: usize) -> u32 {
    columns as u32 * cell_width + (columns as u32 - 1) * CONTENT_GAP
}

// ── 列表页面板（剪贴板/快捷发送）常量 ─────────────────────────────────────
/// 列表页标题栏高度。
pub const LIST_HEADER_HEIGHT: u32 = 40;
/// 列表页行高（背景块）。
pub const LIST_ROW_HEIGHT: u32 = 28;
/// 行背景相对行点击区的上下内缩（行间视觉间隔 = 2× 该值）。
pub const LIST_ROW_INSET: u32 = 2;
/// 列表页行内按钮边长。
pub const LIST_BUTTON_SIZE: u32 = 20;
/// 列表页最多显示的行数。
pub const LIST_LIMIT: usize = 5;
/// 列表页底部"查看全部"入口高度。
pub const LIST_MORE_HEIGHT: u32 = 30;
/// 列表页水平边距。
pub const LIST_H_INSET: u32 = 10;
/// "← 菜单"返回按钮宽度。
pub const LIST_BACK_WIDTH: u32 = 60;
/// 列表面板最小宽度（无候选词时保证列表可读）。
pub const LIST_MIN_PANEL_WIDTH: u32 = 360;

/// 列表页面板高度（标题栏 + 5 行 + 查看全部）。
pub fn list_panel_height() -> u32 {
    LIST_HEADER_HEIGHT
        + LIST_LIMIT as u32 * (LIST_ROW_HEIGHT + 2 * LIST_ROW_INSET)
        + LIST_MORE_HEIGHT
}

/// 列表页第 i 行背景的 y（面板区局部坐标，从标题栏下 4px 起）。
pub fn list_row_y(i: usize) -> u32 {
    LIST_HEADER_HEIGHT + 4 + i as u32 * (LIST_ROW_HEIGHT + 2 * LIST_ROW_INSET)
}

/// 列表页类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Clipboard,
    QuickSend,
}

impl ListKind {
    pub fn title(self) -> &'static str {
        match self {
            ListKind::Clipboard => "剪切板",
            ListKind::QuickSend => "快捷发送",
        }
    }

    /// 行内是否有"加入快捷发送"按钮（仅剪贴板页）。
    pub fn has_quick_send_button(self) -> bool {
        self == ListKind::Clipboard
    }
}

/// 列表页条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub id: i64,
    pub text: String,
    pub is_pinned: bool,
}

/// 列表行内操作按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRowButton {
    /// 加入快捷发送（仅剪贴板页）。
    QuickSend,
    /// 删除条目。
    Remove,
}

/// 列表页命中结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListHit {
    /// 标题栏"← 菜单"返回按钮。
    Back,
    /// 标题栏"清空"按钮。
    Clear,
    /// 底部"查看全部"入口。
    More,
    /// 命中某行：button 为 None 表示行主体（点选上屏）。
    Row {
        index: usize,
        button: Option<ListRowButton>,
    },
}

/// 行内删除按钮矩形（surface 局部坐标）。
fn list_remove_btn_rect(y: u32, panel_width: u32) -> (u32, u32) {
    let x = panel_width - LIST_H_INSET - LIST_BUTTON_SIZE;
    let btn_y = y + (LIST_ROW_HEIGHT - LIST_BUTTON_SIZE) / 2;
    (x, btn_y)
}

/// 行内"加入快捷发送"按钮矩形（删除按钮左侧）。
fn list_quick_send_btn_rect(y: u32, panel_width: u32) -> (u32, u32) {
    let (rx, by) = list_remove_btn_rect(y, panel_width);
    (rx - LIST_BUTTON_SIZE - 6, by)
}

/// 文本按估算宽度截断并追加 "…"（估算以 16px 为基准，偏保守）。
pub fn truncate_to_width(text: &str, max_width: u32) -> String {
    if content_text_width(text) <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let candidate = format!("{out}{ch}");
        if !out.is_empty() && content_text_width(&candidate) + 17 > max_width {
            break;
        }
        out = candidate;
    }
    format!("{out}…")
}

/// 列表页命中测试（绘制几何与点击共用，面板区 y ∈ [bar, bar+list_panel_height)）。
pub fn list_page_hit(
    x: i32,
    y: i32,
    panel_width: u32,
    bar_height: u32,
    item_count: usize,
    is_quick_send: bool,
) -> Option<ListHit> {
    let panel_start = bar_height as i32;
    let panel_end = panel_start + list_panel_height() as i32;
    if y < panel_start || y >= panel_end || x < 0 || x >= panel_width as i32 {
        return None;
    }
    let w = panel_width;
    let local_y = (y - panel_start) as u32;

    // 标题栏按钮："← 菜单"（右上）与"清空"（其左侧）
    let back_x = w - LIST_BACK_WIDTH - LIST_H_INSET;
    let title_btn_y = LIST_HEADER_HEIGHT - 24 - 8;
    if local_y >= title_btn_y
        && local_y < title_btn_y + 24
        && x >= back_x as i32
        && x < (back_x + LIST_BACK_WIDTH) as i32
    {
        return Some(ListHit::Back);
    }
    let clear_x = back_x - 8 - LIST_BUTTON_SIZE;
    if local_y >= title_btn_y + 2
        && local_y < title_btn_y + 2 + LIST_BUTTON_SIZE
        && x >= clear_x as i32
        && x < (clear_x + LIST_BUTTON_SIZE) as i32
    {
        return Some(ListHit::Clear);
    }

    // 底部"查看全部"入口条
    let more_y = list_panel_height() - LIST_MORE_HEIGHT;
    if local_y >= more_y {
        return Some(ListHit::More);
    }

    // 行区域
    for i in 0..item_count.min(LIST_LIMIT) {
        let row_y = list_row_y(i);
        let row_end = row_y + LIST_ROW_HEIGHT;
        if local_y < row_y || local_y >= row_end {
            continue;
        }
        // 按钮优先
        let (rx, ry) = list_remove_btn_rect(row_y, w);
        if local_y >= ry
            && local_y < ry + LIST_BUTTON_SIZE
            && x >= rx as i32
            && x < (rx + LIST_BUTTON_SIZE) as i32
        {
            return Some(ListHit::Row {
                index: i,
                button: Some(ListRowButton::Remove),
            });
        }
        if !is_quick_send {
            let (qx, qy) = list_quick_send_btn_rect(row_y, w);
            if local_y >= qy
                && local_y < qy + LIST_BUTTON_SIZE
                && x >= qx as i32
                && x < (qx + LIST_BUTTON_SIZE) as i32
            {
                return Some(ListHit::Row {
                    index: i,
                    button: Some(ListRowButton::QuickSend),
                });
            }
        }
        // 行主体（含行背景左缘至按钮区之间的空隙）
        let actions_w = if is_quick_send {
            LIST_BUTTON_SIZE
        } else {
            LIST_BUTTON_SIZE * 2 + 6
        };
        let body_end = w - LIST_H_INSET - actions_w;
        if x < body_end as i32 {
            return Some(ListHit::Row {
                index: i,
                button: None,
            });
        }
        return None;
    }
    None
}

/// 内容面板网格项（表情/符号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridItem {
    pub text: String,
    pub comment: String,
}

/// 面板路由视图：候选栏下方展开区的当前内容。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PanelView {
    /// 未展开（仅候选栏）。
    #[default]
    Closed,
    /// 菜单网格（active_index 高亮）。
    Menu(Option<usize>),
    /// 内容网格（表情/符号），highlighted 为页内索引。
    Content {
        items: Vec<GridItem>,
        highlighted: Option<usize>,
    },
    /// 列表页（剪贴板/快捷发送），highlighted 为页内行索引。
    List {
        kind: ListKind,
        items: Vec<ListItem>,
        highlighted: Option<usize>,
    },
}

/// 展开区高度（视当前视图而定）。
pub fn panel_height_for(view: &PanelView) -> u32 {
    match view {
        PanelView::Closed => 0,
        PanelView::Menu(_) => menu_panel_height(),
        PanelView::Content { .. } => content_panel_height(),
        PanelView::List { .. } => list_panel_height(),
    }
}

/// 内容面板每页容量（网格容量）。
pub fn content_capacity(columns: usize) -> usize {
    columns * CONTENT_ROWS
}

/// 内容面板总高度（固定行数网格 + 行间距）。
pub fn content_panel_height() -> u32 {
    CONTENT_ROWS as u32 * (CONTENT_ITEM_SIZE + CONTENT_GAP) - CONTENT_GAP
}

/// 菜单功能入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Emoji,
    Symbols,
    Clipboard,
    QuickSend,
}

impl MenuAction {
    pub fn label(self) -> &'static str {
        match self {
            MenuAction::Emoji => "表情",
            MenuAction::Symbols => "符号",
            MenuAction::Clipboard => "剪切板",
            MenuAction::QuickSend => "快捷发送",
        }
    }

    /// 网格中的行（0 起）。
    pub fn row(self) -> usize {
        self.index() / MENU_COLUMNS
    }

    /// 网格中的列（0 起）。
    pub fn col(self) -> usize {
        self.index() % MENU_COLUMNS
    }

    /// 面板内第几个入口（0 起）。
    pub fn index(self) -> usize {
        match self {
            MenuAction::Emoji => 0,
            MenuAction::Symbols => 1,
            MenuAction::Clipboard => 2,
            MenuAction::QuickSend => 3,
        }
    }

    pub const ALL: [MenuAction; 4] = [
        MenuAction::Emoji,
        MenuAction::Symbols,
        MenuAction::Clipboard,
        MenuAction::QuickSend,
    ];
}

/// 菜单面板总高度（双列，2 行）。
pub fn menu_panel_height() -> u32 {
    MENU_ITEM_HEIGHT * (MenuAction::ALL.len() as u32 / MENU_COLUMNS as u32)
}

/// 展开后容器总高度（候选栏 + 菜单）。
pub fn expanded_height(bar_height: u32) -> u32 {
    bar_height + menu_panel_height()
}

/// 菜单按钮是否包含坐标（候选栏区域，surface 局部坐标）。
pub fn menu_button_hit(x: i32, y: i32, panel_width: u32, bar_height: u32) -> bool {
    let start = panel_width as i32 - MENU_BUTTON_WIDTH as i32;
    x >= start && x < panel_width as i32 && y >= 0 && y < bar_height as i32
}

/// 菜单面板中某坐标命中的入口。
///
/// 面板在候选栏下方展开：候选栏 y ∈ [0, bar_height)，面板
/// y ∈ [bar_height, bar_height+panel_height)。
/// 面板为双列网格，单元格宽 = 容器宽/2，行高 = MENU_ITEM_HEIGHT。
pub fn menu_item_hit(x: i32, y: i32, container_width: u32, bar_height: u32) -> Option<MenuAction> {
    let panel_start = bar_height as i32;
    let panel_end = panel_start + menu_panel_height() as i32;
    if y < panel_start || y >= panel_end {
        return None;
    }
    if x < 0 || x >= container_width as i32 {
        return None;
    }
    let row = (y - panel_start) as usize / MENU_ITEM_HEIGHT as usize;
    let col = x as usize * MENU_COLUMNS / container_width as usize;
    let idx = row * MENU_COLUMNS + col;
    MenuAction::ALL.get(idx).copied()
}

/// 内容面板网格中某坐标命中的项（页内索引）。
///
/// 网格在候选栏下方展开：y ∈ [bar_height, bar_height+content_panel_height)，
/// 固定 CONTENT_ROWS 行、指定列数，单元格宽 = 容器宽/列数，
/// 行高 = CONTENT_ITEM_SIZE + CONTENT_GAP。
pub fn content_item_hit(
    x: i32,
    y: i32,
    panel_width: u32,
    bar_height: u32,
    columns: usize,
    item_count: usize,
) -> Option<usize> {
    let panel_start = bar_height as i32;
    let panel_end = panel_start + content_panel_height() as i32;
    if y < panel_start || y >= panel_end {
        return None;
    }
    if x < 0 || x >= panel_width as i32 {
        return None;
    }
    let row = (y - panel_start) as usize / (CONTENT_ITEM_SIZE + CONTENT_GAP) as usize;
    let col = x as usize * columns / panel_width as usize;
    let idx = row * columns + col;
    (idx < item_count).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_text_width() {
        // 单 CJK/符号字符
        assert_eq!(content_text_width("。"), 17);
        // ASCII 字符较窄
        assert_eq!(content_text_width("abc"), 30);
        // 组合序列（如 0️⃣）零宽字符不计
        assert_eq!(content_text_width("0️⃣"), 10);
        // 空字符串
        assert_eq!(content_text_width(""), 0);
    }

    #[test]
    fn test_content_grid_sizing() {
        // 窄内容：单元格取下限 36，10 列，宽 414
        let cell = content_cell_width(content_text_width("。"));
        assert_eq!(cell, 36);
        let cols = content_columns_for(cell);
        assert_eq!(cols, 10);
        assert_eq!(content_panel_width(cell, cols), 414);
        assert_eq!(content_capacity(cols), 30);

        // 颜文字（如 (ﾟ∀ﾟ)）：单元格加宽、列数减少、宽度增大
        let kaomoji = "(ﾟ∀ﾟ)";
        let cell = content_cell_width(content_text_width(kaomoji));
        assert!(cell >= 36);
        let cols = content_columns_for(cell);
        assert!(cols < 10, "wide items should reduce columns");
        assert!(content_panel_width(cell, cols) <= CONTENT_MAX_WIDTH + cell);
        assert!(content_panel_width(cell, cols) > 414);

        // 列数下限 4
        let cols = content_columns_for(200);
        assert_eq!(cols, 4);
    }

    #[test]
    fn test_content_panel_geometry() {
        // 3 行 × (36+6) - 6 = 120
        assert_eq!(content_panel_height(), 120);
    }

    #[test]
    fn test_content_item_hit() {
        let w = 414u32;
        let bar = 36u32;
        let cols = 10usize;
        // 第 1 行第 1 列
        assert_eq!(content_item_hit(0, 36, w, bar, cols, 30), Some(0));
        // 第 1 行第 10 列
        assert_eq!(
            content_item_hit(w as i32 - 1, 36, w, bar, cols, 30),
            Some(9)
        );
        // 第 2 行（y = 36 + 42）
        assert_eq!(content_item_hit(0, 78, w, bar, cols, 30), Some(10));
        // 第 3 行（y = 36 + 2*42 = 120）
        assert_eq!(content_item_hit(0, 120, w, bar, cols, 30), Some(20));
        assert_eq!(content_item_hit(0, 155, w, bar, cols, 30), Some(20));
        // 超出网格范围
        assert_eq!(content_item_hit(0, 35, w, bar, cols, 30), None);
        assert_eq!(content_item_hit(0, 36 + 120, w, bar, cols, 30), None);
        assert_eq!(content_item_hit(-1, 36, w, bar, cols, 30), None);
        // 超出实际项数
        assert_eq!(content_item_hit(0, 36, w, bar, cols, 0), None);
        assert_eq!(content_item_hit(0, 36, w, bar, cols, 5), Some(0));
        assert_eq!(content_item_hit(w as i32 - 1, 36, w, bar, cols, 5), None);
    }

    #[test]
    fn test_panel_height_for() {
        assert_eq!(panel_height_for(&PanelView::Closed), 0);
        assert_eq!(
            panel_height_for(&PanelView::Menu(None)),
            menu_panel_height()
        );
        assert_eq!(
            panel_height_for(&PanelView::Content {
                items: vec![],
                highlighted: None,
            }),
            content_panel_height()
        );
    }

    #[test]
    fn test_menu_button_hit_with_bar_height() {
        // 默认 36px 候选栏：按钮区在右下角
        assert!(menu_button_hit(378, 10, 414, 36));
        assert!(!menu_button_hit(100, 10, 414, 36));
        assert!(!menu_button_hit(378, 40, 414, 36));
        // 大字号 48px 候选栏：y=40 仍属候选栏
        assert!(menu_button_hit(378, 40, 414, 48));
    }

    #[test]
    fn test_list_panel_geometry() {
        // 40 + 5*(28+4) + 30 = 230
        assert_eq!(list_panel_height(), 230);
        assert_eq!(list_row_y(0), LIST_HEADER_HEIGHT + 4);
        assert_eq!(list_row_y(4), LIST_HEADER_HEIGHT + 4 + 4 * 32);
        assert_eq!(list_row_y(5), list_row_y(4) + 32);
    }

    #[test]
    fn test_list_page_hit_rows() {
        let w = 400u32;
        let bar = 36u32;
        // 第 1 行主体（行 y = 36+44 = 80，中心 y ≈ 94）
        let row0 = 36 + list_row_y(0) as i32 + 10;
        assert_eq!(
            list_page_hit(50, row0, w, bar, 3, false),
            Some(ListHit::Row {
                index: 0,
                button: None
            })
        );
        // 第 3 行
        let row2 = 36 + list_row_y(2) as i32 + 10;
        assert_eq!(
            list_page_hit(50, row2, w, bar, 3, false),
            Some(ListHit::Row {
                index: 2,
                button: None
            })
        );
        // 超出实际行数（第 4 行点击无效）
        let row3 = 36 + list_row_y(3) as i32 + 10;
        assert_eq!(list_page_hit(50, row3, w, bar, 3, false), None);
        // 删除按钮（行右侧）
        let rx = w as i32 - LIST_H_INSET as i32 - LIST_BUTTON_SIZE as i32 + 5;
        assert_eq!(
            list_page_hit(rx, row0, w, bar, 3, false),
            Some(ListHit::Row {
                index: 0,
                button: Some(ListRowButton::Remove)
            })
        );
        // 剪贴板页：删除按钮左侧是"加入快捷发送"
        let qx = rx - LIST_BUTTON_SIZE as i32 - 6 + 5;
        assert_eq!(
            list_page_hit(qx, row0, w, bar, 3, false),
            Some(ListHit::Row {
                index: 0,
                button: Some(ListRowButton::QuickSend)
            })
        );
        // 快捷发送页：同一位置是行主体（无快捷发送按钮）
        assert_eq!(
            list_page_hit(qx, row0, w, bar, 3, true),
            Some(ListHit::Row {
                index: 0,
                button: None
            })
        );
    }

    #[test]
    fn test_list_page_hit_header_and_more() {
        let w = 400u32;
        let bar = 36u32;
        // "← 菜单"返回按钮（标题栏右上）
        let back_x = (w - LIST_BACK_WIDTH - LIST_H_INSET) as i32 + 10;
        let back_y = 36 + (LIST_HEADER_HEIGHT - 24 - 8) as i32 + 10;
        assert_eq!(
            list_page_hit(back_x, back_y, w, bar, 3, false),
            Some(ListHit::Back)
        );
        // "清空"按钮（返回按钮左侧）
        let clear_x = back_x - 8 - LIST_BUTTON_SIZE as i32 + 5;
        assert_eq!(
            list_page_hit(clear_x, back_y + 2, w, bar, 3, false),
            Some(ListHit::Clear)
        );
        // 底部"查看全部"
        let more_y = 36 + (list_panel_height() - LIST_MORE_HEIGHT) as i32 + 10;
        assert_eq!(
            list_page_hit(50, more_y, w, bar, 3, false),
            Some(ListHit::More)
        );
        // 面板外
        assert_eq!(list_page_hit(50, 10, w, bar, 3, false), None);
        assert_eq!(
            list_page_hit(50, 36 + list_panel_height() as i32, w, bar, 3, false),
            None
        );
        assert_eq!(list_page_hit(-1, back_y, w, bar, 3, false), None);
    }

    #[test]
    fn test_truncate_to_width() {
        // 短文本不截断
        assert_eq!(truncate_to_width("你好", 100), "你好");
        // 长文本截断加省略号
        let long = "这是一段很长很长很长的剪贴板内容需要被截断显示";
        let t = truncate_to_width(long, 120);
        assert!(t.ends_with('…'));
        assert!(t.chars().count() < long.chars().count() + 1);
        // 空文本
        assert_eq!(truncate_to_width("", 10), "");
    }

    #[test]
    fn test_menu_item_hit_with_bar_height() {
        // 默认 36px 候选栏：面板从 y=36 起
        assert_eq!(menu_item_hit(10, 40, 414, 36), Some(MenuAction::Emoji));
        // 大字号 48px 候选栏：y=40 仍在候选栏内，未命中面板
        assert_eq!(menu_item_hit(10, 40, 414, 48), None);
        assert_eq!(menu_item_hit(10, 52, 414, 48), Some(MenuAction::Emoji));
    }
}

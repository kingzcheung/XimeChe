//! 基于 iced 的候选栏/菜单/字根 UI 渲染（离屏）。
//!
//! 用 iced 的 widget 树（Element）描述 UI，经 `iced_tiny_skia`
//! 离屏渲染到 SHM buffer。布局/圆角/文本/图标交给 iced，避免手绘。
//! 样式来自 [`PanelTheme`]（配置 + 亮/暗模式），不再硬编码。

use iced_tiny_skia::core::widget::tree::Tree;
use iced_tiny_skia::core::Renderer as CoreRenderer;
use iced_tiny_skia::core::{layout, Element};
use iced_tiny_skia::core::{mouse, renderer::Style, Color, Font, Pixels, Rectangle, Size, Theme};
use iced_tiny_skia::graphics::Viewport;
use iced_tiny_skia::Renderer;
use iced_widget::{container, row, text, Space, Svg};
// 与 iced_tiny_skia 同版本的 tiny-skia（0.11），避免版本冲突
type Pixmap = tiny_skia11::Pixmap;
type Mask = tiny_skia11::Mask;

use crate::theme::PanelTheme;
use crate::CandidateItem;
use crate::{
    content_cell_width, content_columns_for, content_text_width, truncate_to_width, GridItem,
    ListItem, ListKind, MenuAction, PanelView, CONTENT_GAP, CONTENT_ITEM_SIZE, CONTENT_ROWS,
    LIST_BUTTON_SIZE, LIST_HEADER_HEIGHT, LIST_H_INSET, LIST_LIMIT, LIST_MIN_PANEL_WIDTH,
    LIST_MORE_HEIGHT, LIST_ROW_HEIGHT, LIST_ROW_INSET, MENU_BUTTON_WIDTH, MENU_COLUMNS,
    MENU_ITEM_HEIGHT,
};

const MENU_SVG: &[u8] = include_bytes!("../resources/menu.svg");

/// 菜单入口图标颜色（固定点缀色，不随主题变化）。
fn menu_item_color(idx: usize) -> Color {
    match idx {
        0 => Color::from_rgb8(0x8F, 0x73, 0xE2),
        1 => Color::from_rgb8(0x1A, 0x73, 0xE8),
        2 => Color::from_rgb8(0x2E, 0xA0, 0x7D),
        _ => Color::from_rgb8(0xE5, 0x8F, 0x2A),
    }
}

/// 离屏渲染器（内部持有 iced Renderer + 树状态）。
pub struct IcedSurface {
    renderer: Renderer,
    tree: Tree,
}

impl IcedSurface {
    pub fn new() -> Self {
        let renderer = Renderer::new(Font::default(), Pixels::from(14.0));
        let tree = Tree::empty();
        Self { renderer, tree }
    }

    /// 渲染任意 Element 到 BGRA buffer（通用入口）。
    pub fn render<M: 'static>(
        &mut self,
        element: &mut Element<'_, M, Theme, Renderer>,
        pixels: &mut [u8],
        width: u32,
        height: u32,
    ) {
        let mut pixmap = Pixmap::new(width, height).expect("pixmap");

        // 清空渲染层（iced_tiny_skia 的 layers 不会自动清空，
        // 正常 iced 应用由 window 调用 reset；这里必须手动 reset，
        // 否则上一次渲染的内容会叠加重复绘制）
        self.renderer.reset(Rectangle {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        });

        // diff：让 tree 状态与 widget 结构匹配（container 等需要 state）
        self.tree.diff(element.as_widget());

        // layout
        let widget = element.as_widget_mut();
        let limits = layout::Limits::new(Size::ZERO, Size::new(width as f32, height as f32));
        let node = widget.layout(&mut self.tree, &self.renderer, &limits);

        // draw
        let layout = layout::Layout::new(&node);
        let viewport = Rectangle {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        };
        let style = Style {
            text_color: Color::BLACK,
        };
        let cursor = mouse::Cursor::default();
        widget.draw(
            &self.tree,
            &mut self.renderer,
            &Theme::Dark,
            &style,
            layout,
            cursor,
            &viewport,
        );

        // 渲染到 pixmap
        let viewport =
            Viewport::with_physical_size(iced_tiny_skia::core::Size::new(width, height), 1.0);
        let mut clip_mask = Mask::new(width, height).expect("mask");
        let damage = vec![Rectangle {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        }];
        let bg = Color::TRANSPARENT;
        let mut pixmap_mut = pixmap.as_mut();
        self.renderer
            .draw(&mut pixmap_mut, &mut clip_mask, &viewport, &damage, bg);

        // RGBA → BGRA 拷贝
        let data = pixmap.data();
        for i in (0..data.len()).step_by(4) {
            if i + 3 < pixels.len() {
                pixels[i] = data[i + 2];
                pixels[i + 1] = data[i + 1];
                pixels[i + 2] = data[i];
                pixels[i + 3] = data[i + 3];
            }
        }
    }

    /// 测量 Element 自然尺寸（内容自适应宽度）。
    pub fn measure<M: 'static>(&mut self, element: &mut Element<'_, M, Theme, Renderer>) -> Size {
        self.tree.diff(element.as_widget());
        let widget = element.as_widget_mut();
        let limits = layout::Limits::new(Size::ZERO, Size::new(10000.0, 10000.0));
        let node = widget.layout(&mut self.tree, &self.renderer, &limits);
        node.bounds().size()
    }

    /// 候选栏自然宽度（内容 + 菜单按钮）。
    pub fn measure_candidates(&mut self, candidates: &[CandidateItem], theme: &PanelTheme) -> u32 {
        let mut view = candidate_bar_view(candidates, 0, theme);
        let size = self.measure(&mut view);
        size.width.ceil() as u32
    }

    /// 绘制候选栏（面板关闭）。
    pub fn draw_candidates(
        &mut self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
        candidates: &[CandidateItem],
        highlighted_index: usize,
        theme: &PanelTheme,
    ) {
        self.draw_panel(
            pixels,
            width,
            height,
            candidates,
            highlighted_index,
            theme,
            &PanelView::Closed,
        );
    }

    /// 绘制完整面板（候选栏 + 菜单按钮 + 可选展开视图：菜单网格/内容网格）。
    #[allow(clippy::too_many_arguments)]
    pub fn draw_panel(
        &mut self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
        candidates: &[CandidateItem],
        highlighted_index: usize,
        theme: &PanelTheme,
        panel_view: &PanelView,
    ) {
        // 1. 自绘圆角背景 + 边框（SDF 精确，绕开 tiny-skia 曲线光栅化偏差）
        paint_rounded_panel(
            pixels,
            width,
            height,
            theme.corner_radius,
            2.0,
            theme.border,
            theme.bg,
        );

        // 2. iced 渲染内容（透明背景）到临时 buffer
        let mut content = vec![0u8; (width * height * 4) as usize];
        let mut view = build_panel_view(candidates, highlighted_index, theme, panel_view);
        self.render(&mut view, &mut content, width, height);

        // 3. 内容合成到背景
        blend_over(pixels, &content);
    }

    /// 字根窗口自然宽度。
    pub fn measure_root(&mut self, key: char, root: &str, theme: &PanelTheme) -> u32 {
        let mut view = root_view(key, root, theme);
        let size = self.measure(&mut view);
        size.width.ceil() as u32
    }

    /// 绘制字根窗口。
    pub fn draw_root(
        &mut self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
        key: char,
        root: &str,
        theme: &PanelTheme,
    ) {
        // 1. 自绘圆角背景 + 边框（SDF 精确）
        paint_rounded_panel(
            pixels,
            width,
            height,
            theme.corner_radius,
            2.0,
            theme.border,
            theme.bg,
        );

        // 2. iced 渲染内容（透明背景）到临时 buffer
        let mut content = vec![0u8; (width * height * 4) as usize];
        let mut view = root_view(key, root, theme);
        self.render(&mut view, &mut content, width, height);

        // 3. 内容合成到背景
        blend_over(pixels, &content);
    }
}

impl Default for IcedSurface {
    fn default() -> Self {
        Self::new()
    }
}

/// 圆角矩形有符号距离场（负 = 内部）。
fn rounded_rect_sdf(x: f32, y: f32, w: f32, h: f32, r: f32, px: f32, py: f32) -> f32 {
    let half_w = w / 2.0;
    let half_h = h / 2.0;
    let qx = (px - (x + half_w)).abs() - (half_w - r);
    let qy = (py - (y + half_h)).abs() - (half_h - r);
    let dx = qx.max(0.0);
    let dy = qy.max(0.0);
    let d = (dx * dx + dy * dy).sqrt();
    d - r + qx.max(qy).min(0.0)
}

/// 圆角矩形抗锯齿覆盖因子（0..=1）。
fn rounded_alpha(sdf: f32) -> f32 {
    (0.5 - sdf).clamp(0.0, 1.0)
}

/// 在 BGRA buffer 上绘制圆角矩形背景 + 边框（SDF 精确，绕开 tiny-skia 曲线偏差）。
fn paint_rounded_panel(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    radius: f32,
    border_width: f32,
    border_color: Color,
    fill_color: Color,
) {
    let (fw, fh) = (width as f32, height as f32);
    let (fr, fg, fb) = (
        fill_color.r * 255.0,
        fill_color.g * 255.0,
        fill_color.b * 255.0,
    );
    let (br, bg, bb) = (
        border_color.r * 255.0,
        border_color.g * 255.0,
        border_color.b * 255.0,
    );
    let w = width as usize;
    for y in 0..height as usize {
        for x in 0..width as usize {
            let d = rounded_rect_sdf(0.0, 0.0, fw, fh, radius, x as f32 + 0.5, y as f32 + 0.5);
            // 边框区域：d ∈ [-border_width, 0]
            let border_a = rounded_alpha(d);
            let fill_a = rounded_alpha(d + border_width);
            let idx = (y * w + x) * 4;
            // 合成：边框在上层，填充在下层（目标为透明背景）
            let mut r_ = fill_a * fr;
            let mut g_ = fill_a * fg;
            let mut b_ = fill_a * fb;
            let mut a_ = fill_a;
            let ba = (border_a - fill_a).max(0.0);
            if ba > 0.0 {
                // 边框叠加在填充上
                let oa = a_;
                r_ = r_ * (1.0 - ba) + br * ba;
                g_ = g_ * (1.0 - ba) + bg * ba;
                b_ = b_ * (1.0 - ba) + bb * ba;
                a_ = oa * (1.0 - ba) + ba;
            }
            pixels[idx] = b_.clamp(0.0, 255.0) as u8;
            pixels[idx + 1] = g_.clamp(0.0, 255.0) as u8;
            pixels[idx + 2] = r_.clamp(0.0, 255.0) as u8;
            pixels[idx + 3] = (a_ * 255.0).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 将内容 buffer（BGRA, 半透明）合成到目标 buffer。
fn blend_over(dst: &mut [u8], src: &[u8]) {
    for i in (0..src.len()).step_by(4) {
        let sa = src[i + 3] as f32 / 255.0;
        if sa <= 0.0 {
            continue;
        }
        let da = dst[i + 3] as f32 / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            continue;
        }
        for c in 0..3 {
            let s = src[i + c] as f32;
            let d = dst[i + c] as f32;
            let v = (s * sa + d * da * (1.0 - sa)) / out_a;
            dst[i + c] = v.clamp(0.0, 255.0) as u8;
        }
        dst[i + 3] = (out_a * 255.0) as u8;
    }
}

/// 候选内容行（不带按钮，Shrink）。
fn candidate_items<'a>(
    candidates: &'a [CandidateItem],
    highlighted_index: usize,
    theme: &'a PanelTheme,
) -> Element<'a, (), Theme, Renderer> {
    let mut content = row![].spacing(6);
    let comment_size = (theme.font_size - 2.0).max(10.0);
    for (idx, candidate) in candidates.iter().enumerate() {
        let is_hl = idx == highlighted_index;
        let label = format!("{}. {}", idx + 1, candidate.text);
        let t = text(label).size(theme.font_size).color(if is_hl {
            Color::WHITE
        } else {
            theme.text_main
        });
        let comment = if !candidate.comment.is_empty() {
            text(candidate.comment.clone())
                .size(comment_size)
                .color(theme.text_comment)
        } else {
            text("").size(comment_size)
        };
        let item = row![t, comment]
            .spacing(4)
            .align_y(iced_widget::core::alignment::Vertical::Center);

        let radius = theme.highlight_radius();
        if is_hl {
            content =
                content.push(
                    container(item)
                        .padding([4, 8])
                        .style(move |_| container::Style {
                            background: Some(iced_widget::core::Background::Color(theme.primary)),
                            border: iced_widget::core::border::Border {
                                radius: iced_widget::core::border::Radius::from(radius),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                );
        } else {
            // 与选中同构（container + padding），保证字形垂直位置一致
            content = content.push(container(item).padding([4, 8]));
        }
    }
    content.into()
}

/// 候选栏内容 + 按钮（Shrink，含左右 padding，用于宽度测量）。
///
/// 必须与渲染版 `candidate_bar` 的 padding 一致，否则渲染时
/// 按钮会被 padding 挤压（剩余空间不足导致按钮/图标变小）。
fn candidate_bar_view<'a>(
    candidates: &'a [CandidateItem],
    highlighted_index: usize,
    theme: &'a PanelTheme,
) -> Element<'a, (), Theme, Renderer> {
    container(row![
        candidate_items(candidates, highlighted_index, theme),
        menu_button(false, theme),
    ])
    .padding([0, 12])
    .into()
}

/// 菜单按钮（九宫格 SVG 图标，active 时高亮）。
fn menu_button(active: bool, theme: &PanelTheme) -> Element<'static, (), Theme, Renderer> {
    let icon = Svg::new(iced_widget::core::svg::Handle::from_memory(MENU_SVG))
        .width(32)
        .height(32);
    let (r, g, b) = (
        (theme.primary.r * 255.0) as u8,
        (theme.primary.g * 255.0) as u8,
        (theme.primary.b * 255.0) as u8,
    );
    container(icon)
        .width(MENU_BUTTON_WIDTH)
        .height(theme.bar_height())
        .align_x(iced_widget::core::alignment::Horizontal::Center)
        .align_y(iced_widget::core::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: if active {
                Some(iced_widget::core::Background::Color(Color::from_rgba8(
                    r, g, b, 0.13,
                )))
            } else {
                None
            },
            ..Default::default()
        })
        .into()
}

/// 完整面板：候选栏 + 可选展开视图（菜单网格 / 内容网格）。
fn build_panel_view<'a>(
    candidates: &'a [CandidateItem],
    highlighted_index: usize,
    theme: &'a PanelTheme,
    view: &'a PanelView,
) -> Element<'a, (), Theme, Renderer> {
    let bar = candidate_bar(candidates, highlighted_index, theme);
    let content: Element<'a, (), Theme, Renderer> = match view {
        PanelView::Closed => bar,
        PanelView::Menu(active_index) => {
            let grid = menu_grid(*active_index, theme);
            let divider = panel_divider(theme);
            iced_widget::column![bar, divider, grid].into()
        }
        PanelView::Content { items, highlighted } => {
            let grid = content_grid(items, *highlighted, theme);
            let divider = panel_divider(theme);
            iced_widget::column![bar, divider, grid].into()
        }
        PanelView::List {
            kind,
            items,
            highlighted,
        } => {
            let page = list_page(*kind, items, *highlighted, theme);
            let divider = panel_divider(theme);
            iced_widget::column![bar, divider, page].into()
        }
    };

    // 背景圆角/边框由 paint_rounded_panel 自绘（SDF 精确），这里透明
    container(content)
        .width(iced_widget::core::Length::Fill)
        .height(iced_widget::core::Length::Fill)
        .style(|_| container::Style::default())
        .into()
}

/// 面板区与候选栏之间的分隔线。
fn panel_divider<'a>(theme: &'a PanelTheme) -> Element<'a, (), Theme, Renderer> {
    container(Space::new())
        .width(iced_widget::core::Length::Fill)
        .height(1)
        .style(move |_| container::Style {
            background: Some(iced_widget::core::Background::Color(theme.border)),
            ..Default::default()
        })
        .into()
}

/// 候选栏行（渲染用：容器撑满 buffer 宽，内容左对齐 + 按钮右对齐）。
fn candidate_bar<'a>(
    candidates: &'a [CandidateItem],
    highlighted_index: usize,
    theme: &'a PanelTheme,
) -> Element<'a, (), Theme, Renderer> {
    container(
        row![
            candidate_items(candidates, highlighted_index, theme),
            Space::new().width(iced_widget::core::Length::Fill),
            menu_button(false, theme),
        ]
        .align_y(iced_widget::core::alignment::Vertical::Center),
    )
    .width(iced_widget::core::Length::Fill)
    .height(theme.bar_height())
    .padding([0, 12])
    .align_y(iced_widget::core::alignment::Vertical::Center)
    .into()
}

/// 菜单双列网格（2 行 × 2 列）。
fn menu_grid(
    active_index: Option<usize>,
    theme: &PanelTheme,
) -> Element<'static, (), Theme, Renderer> {
    let mut grid = iced_widget::column![].spacing(0);
    for r in 0..(MenuAction::ALL.len() / MENU_COLUMNS) {
        let mut row_widget = iced_widget::row![].spacing(0);
        for c in 0..MENU_COLUMNS {
            let idx = r * MENU_COLUMNS + c;
            let cell: Element<'static, (), Theme, Renderer> = match MenuAction::ALL.get(idx) {
                Some(action) => menu_cell(*action, active_index == Some(idx), theme),
                None => Space::new()
                    .width(iced_widget::core::Length::Fill)
                    .height(MENU_ITEM_HEIGHT)
                    .into(),
            };
            row_widget = row_widget.push(cell);
        }
        grid = grid.push(
            row_widget
                .width(iced_widget::core::Length::Fill)
                .height(MENU_ITEM_HEIGHT),
        );
    }
    grid.width(iced_widget::core::Length::Fill)
        .height(MENU_ITEM_HEIGHT * 2)
        .into()
}

/// 单个菜单入口：图标 chip（首字 + 色底）+ 文字。
/// 未实现的入口（is_available() == false）置灰显示，点击无效。
fn menu_cell(
    action: MenuAction,
    active: bool,
    theme: &PanelTheme,
) -> Element<'static, (), Theme, Renderer> {
    let idx = action.index();
    let color = menu_item_color(idx);
    let label_color = theme.text_main;
    let first = action.label().chars().next().unwrap_or('?');
    let chip = container(text(first.to_string()).size(theme.font_size).color(color))
        .width(32)
        .height(32)
        .align_x(iced_widget::core::alignment::Horizontal::Center)
        .align_y(iced_widget::core::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: Some(iced_widget::core::Background::Color(Color::from_rgba8(
                (color.r * 255.0) as u8,
                (color.g * 255.0) as u8,
                (color.b * 255.0) as u8,
                0.13,
            ))),
            border: iced_widget::core::border::Border {
                radius: iced_widget::core::border::Radius::from(8.0),
                ..Default::default()
            },
            ..Default::default()
        });
    let label = text(action.label())
        .size(theme.font_size)
        .color(label_color);
    let item = row![chip, label]
        .spacing(10)
        .align_y(iced_widget::core::alignment::Vertical::Center);

    let (pr, pg, pb) = (
        (theme.primary.r * 255.0) as u8,
        (theme.primary.g * 255.0) as u8,
        (theme.primary.b * 255.0) as u8,
    );
    container(item)
        .width(iced_widget::core::Length::Fill)
        .height(MENU_ITEM_HEIGHT)
        .padding([0, 16])
        .align_y(iced_widget::core::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: if active {
                Some(iced_widget::core::Background::Color(Color::from_rgba8(
                    pr, pg, pb, 0.05,
                )))
            } else {
                None
            },
            ..Default::default()
        })
        .into()
}

/// 内容网格（表情/符号）：列数按最宽项自适应（4..=10），固定 CONTENT_ROWS 行，
/// 空位留白，高亮项着色。
///
/// 字号固定 16：`menu.rs::content_text_width` 的宽度估算以 16px 为基准，
/// 同时驱动渲染与点击命中几何，改动会破坏两者一致性。
fn content_grid(
    items: &[GridItem],
    highlighted: Option<usize>,
    theme: &PanelTheme,
) -> Element<'static, (), Theme, Renderer> {
    let widest = items
        .iter()
        .map(|i| content_text_width(&i.text))
        .max()
        .unwrap_or(0);
    let cell_width = content_cell_width(widest);
    let columns = content_columns_for(cell_width);
    let mut grid = iced_widget::column![].spacing(CONTENT_GAP as f32);
    for r in 0..CONTENT_ROWS {
        let mut row_widget = iced_widget::row![].spacing(CONTENT_GAP as f32);
        for c in 0..columns {
            let idx = r * columns + c;
            let cell: Element<'static, (), Theme, Renderer> = match items.get(idx) {
                Some(item) => content_cell(item, highlighted == Some(idx), theme),
                None => Space::new()
                    .width(iced_widget::core::Length::Fill)
                    .height(CONTENT_ITEM_SIZE)
                    .into(),
            };
            row_widget = row_widget.push(cell);
        }
        grid = grid.push(
            row_widget
                .width(iced_widget::core::Length::Fill)
                .height(CONTENT_ITEM_SIZE),
        );
    }
    grid.width(iced_widget::core::Length::Fill)
        .height(CONTENT_ROWS as f32 * CONTENT_ITEM_SIZE as f32)
        .into()
}

/// 单个内容单元格：文本居中不换行，高亮时着底色。
fn content_cell(
    item: &GridItem,
    highlighted: bool,
    theme: &PanelTheme,
) -> Element<'static, (), Theme, Renderer> {
    let (pr, pg, pb) = (
        (theme.primary.r * 255.0) as u8,
        (theme.primary.g * 255.0) as u8,
        (theme.primary.b * 255.0) as u8,
    );
    container(
        text(item.text.clone())
            .size(16)
            .color(theme.text_main)
            .wrapping(iced_widget::core::text::Wrapping::None),
    )
    .width(iced_widget::core::Length::Fill)
    .height(CONTENT_ITEM_SIZE)
    .align_x(iced_widget::core::alignment::Horizontal::Center)
    .align_y(iced_widget::core::alignment::Vertical::Center)
    .style(move |_| container::Style {
        background: if highlighted {
            Some(iced_widget::core::Background::Color(Color::from_rgba8(
                pr, pg, pb, 0.13,
            )))
        } else {
            None
        },
        ..Default::default()
    })
    .into()
}

// ── 列表页面板（剪贴板/快捷发送） ─────────────────────────────────────────

/// 行背景色（静态中性色：亮色偏黑、暗色偏亮灰，对齐 macOS 版 neutral_overlay）。
fn neutral_row_bg(theme: &PanelTheme, alpha: f32) -> Color {
    if theme.dark {
        Color::from_rgba8(0x59, 0x5E, 0x6B, alpha)
    } else {
        Color::from_rgba8(0x00, 0x00, 0x00, alpha)
    }
}

/// 列表页：标题栏（标题 + 清空 + ← 菜单）+ 最多 5 行条目 + 查看全部。
fn list_page<'a>(
    kind: ListKind,
    items: &'a [ListItem],
    highlighted: Option<usize>,
    theme: &'a PanelTheme,
) -> Element<'a, (), Theme, Renderer> {
    let title_size = theme.font_size + 1.0;
    let small_size = theme.font_size;

    // 标题栏
    let header = container(
        row![
            text(kind.title()).size(title_size).color(theme.text_main),
            Space::new().width(iced_widget::core::Length::Fill),
            text("清空").size(small_size).color(theme.text_comment),
            text("← 菜单").size(small_size).color(theme.text_comment),
        ]
        .spacing(16)
        .align_y(iced_widget::core::alignment::Vertical::Center),
    )
    .width(iced_widget::core::Length::Fill)
    .height(LIST_HEADER_HEIGHT)
    .padding([0, LIST_H_INSET as u16])
    .align_y(iced_widget::core::alignment::Vertical::Center);

    // 条目行（固定 LIST_LIMIT 行，空位留白）
    let mut rows = iced_widget::column![].spacing((2 * LIST_ROW_INSET) as f32);
    for i in 0..LIST_LIMIT {
        let row_widget: Element<'static, (), Theme, Renderer> = match items.get(i) {
            Some(item) => list_row(item, highlighted == Some(i), kind, theme),
            None => Space::new()
                .width(iced_widget::core::Length::Fill)
                .height(LIST_ROW_HEIGHT)
                .into(),
        };
        rows = rows.push(row_widget);
    }

    // 空态 / 底部"查看全部"
    let body: Element<'a, (), Theme, Renderer> = if items.is_empty() {
        container(text("暂无内容").size(small_size).color(theme.text_comment))
            .width(iced_widget::core::Length::Fill)
            .align_x(iced_widget::core::alignment::Horizontal::Center)
            .into()
    } else {
        container(rows)
            .padding([4, 0])
            .width(iced_widget::core::Length::Fill)
            .into()
    };

    let more_bg = neutral_row_bg(theme, 0.05);
    let more_color = theme.text_comment;
    let more = container(
        row![
            text("查看全部").size(small_size).color(more_color),
            Space::new().width(iced_widget::core::Length::Fill),
            text("›").size(small_size).color(more_color),
        ]
        .align_y(iced_widget::core::alignment::Vertical::Center),
    )
    .width(iced_widget::core::Length::Fill)
    .height(LIST_MORE_HEIGHT)
    .padding([0, (LIST_H_INSET + 4) as u16])
    .align_y(iced_widget::core::alignment::Vertical::Center)
    .style(move |_| container::Style {
        background: Some(iced_widget::core::Background::Color(more_bg)),
        ..Default::default()
    });

    iced_widget::column![header, body, more]
        .width(iced_widget::core::Length::Fill)
        .into()
}

/// 单条列表行：置顶标记 + 文本（超宽截断）+ 行内按钮。
fn list_row(
    item: &ListItem,
    highlighted: bool,
    kind: ListKind,
    theme: &PanelTheme,
) -> Element<'static, (), Theme, Renderer> {
    let small_size = theme.font_size;
    let (pr, pg, pb) = (
        (theme.primary.r * 255.0) as u8,
        (theme.primary.g * 255.0) as u8,
        (theme.primary.b * 255.0) as u8,
    );

    // 行主体宽度 = 容器宽 - 右侧按钮区（截断估算用，偏保守）
    let actions_w = if kind.has_quick_send_button() {
        LIST_BUTTON_SIZE * 2 + 6
    } else {
        LIST_BUTTON_SIZE
    };
    let body_max = LIST_MIN_PANEL_WIDTH.saturating_sub(2 * LIST_H_INSET + actions_w + 24);

    let mut content = iced_widget::row![]
        .spacing(8)
        .align_y(iced_widget::core::alignment::Vertical::Center);
    if item.is_pinned {
        content = content.push(text("★").size(small_size).color(theme.primary));
    }
    content = content.push(
        text(truncate_to_width(&item.text, body_max))
            .size(small_size)
            .color(theme.text_main)
            .wrapping(iced_widget::core::text::Wrapping::None),
    );

    let mut row_widget =
        iced_widget::row![content].align_y(iced_widget::core::alignment::Vertical::Center);
    row_widget = row_widget.push(Space::new().width(iced_widget::core::Length::Fill));
    let btn_text_color = theme.text_comment;
    let btn_bg = neutral_row_bg(theme, 0.10);
    let btn_radius = (theme.highlight_radius() - 2.0).max(2.0);
    let btn_font = theme.font_size - 2.0;
    if kind.has_quick_send_button() {
        row_widget = row_widget.push(round_text_button(
            "＋",
            btn_text_color,
            btn_bg,
            btn_radius,
            btn_font,
        ));
    }
    row_widget = row_widget.push(round_text_button(
        "×",
        btn_text_color,
        btn_bg,
        btn_radius,
        btn_font,
    ));

    let neutral_bg = neutral_row_bg(theme, 0.06);
    let radius = theme.highlight_radius();
    container(row_widget)
        .width(iced_widget::core::Length::Fill)
        .height(LIST_ROW_HEIGHT)
        .padding([0, LIST_H_INSET as u16])
        .align_y(iced_widget::core::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: if highlighted {
                Some(iced_widget::core::Background::Color(Color::from_rgba8(
                    pr, pg, pb, 0.13,
                )))
            } else {
                Some(iced_widget::core::Background::Color(neutral_bg))
            },
            border: iced_widget::core::border::Border {
                radius: iced_widget::core::border::Radius::from(radius),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// 行内小圆角文字按钮（＋ / ×）。
fn round_text_button(
    label: &'static str,
    text_color: Color,
    bg: Color,
    radius: f32,
    font_size: f32,
) -> Element<'static, (), Theme, Renderer> {
    container(text(label.to_string()).size(font_size).color(text_color))
        .width(LIST_BUTTON_SIZE)
        .height(LIST_BUTTON_SIZE)
        .align_x(iced_widget::core::alignment::Horizontal::Center)
        .align_y(iced_widget::core::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: Some(iced_widget::core::Background::Color(bg)),
            border: iced_widget::core::border::Border {
                radius: iced_widget::core::border::Radius::from(radius),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// 字根窗口：`[key] root`。
fn root_view<'a>(
    key: char,
    root: &'a str,
    theme: &'a PanelTheme,
) -> Element<'a, (), Theme, Renderer> {
    let key_box = container(
        text(key.to_string())
            .size(theme.font_size + 6.0)
            .color(Color::WHITE),
    )
    .width(32)
    .height(32)
    .align_x(iced_widget::core::alignment::Horizontal::Center)
    .align_y(iced_widget::core::alignment::Vertical::Center)
    .style(move |_| container::Style {
        background: Some(iced_widget::core::Background::Color(theme.primary)),
        border: iced_widget::core::border::Border {
            radius: iced_widget::core::border::Radius::from(4.0),
            ..Default::default()
        },
        ..Default::default()
    });
    let root_text = text(root.to_string())
        .size(theme.font_size + 2.0)
        .color(theme.text_main);
    // 背景圆角/边框由 paint_rounded_panel 自绘（SDF 精确），这里透明
    container(
        row![key_box, root_text]
            .spacing(8)
            .align_y(iced_widget::core::alignment::Vertical::Center),
    )
    .padding([4, 12])
    .align_y(iced_widget::core::alignment::Vertical::Center)
    .into()
}

/// 供测试/验证用：无操作消息类型。
pub type IcedElement<'a> = Element<'a, (), Theme, Renderer>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CandidateItem;

    fn sample_candidates() -> Vec<CandidateItem> {
        vec![
            CandidateItem {
                text: "式".into(),
                comment: "aa".into(),
                index: 0,
            },
            CandidateItem {
                text: "是".into(),
                comment: "bb".into(),
                index: 1,
            },
            CandidateItem {
                text: "时".into(),
                comment: "cc".into(),
                index: 2,
            },
        ]
    }

    fn test_theme() -> PanelTheme {
        PanelTheme::light(16.0, 6.0, (0x8F, 0x73, 0xE2))
    }

    /// 测量宽度应合理（内容自适应，不会撑满极限尺寸）。
    #[test]
    fn test_measure_candidates_width_reasonable() {
        let candidates = sample_candidates();
        let theme = test_theme();
        let mut surface = IcedSurface::new();
        let w = surface.measure_candidates(&candidates, &theme);
        assert!(
            (80..=800).contains(&w),
            "candidate width should be reasonable, got: {}",
            w
        );
    }

    /// 四角应透明（圆角裁剪生效），边缘中部应有背景色。
    #[test]
    fn test_panel_corners_rounded() {
        let candidates = sample_candidates();
        let theme = test_theme();
        let mut surface = IcedSurface::new();
        let w = surface.measure_candidates(&candidates, &theme);
        let h = theme.bar_height();
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        surface.draw_candidates(&mut pixels, w, h, &candidates, 0, &theme);

        // 角部 1px 内应透明（圆角），检查 (0,0) 与 (w-1, h-1)
        let at = |x: u32, y: u32| pixels[((y * w + x) * 4 + 3) as usize];
        assert_eq!(at(0, 0), 0, "top-left corner should be transparent");
        assert_eq!(at(w - 1, 0), 0, "top-right corner should be transparent");
        assert_eq!(at(0, h - 1), 0, "bottom-left corner should be transparent");
        assert_eq!(
            at(w - 1, h - 1),
            0,
            "bottom-right corner should be transparent"
        );

        // 上边缘中部应有内容（圆角弧线之外）
        assert!(at(w / 2, 1) != 0, "top edge middle should have background");
    }

    /// 连续多次渲染面板，菜单按钮图标不应叠加重复（复现"打一次多一个图标"）。
    #[test]
    fn test_repeated_draw_no_icon_doubling() {
        let candidates = sample_candidates();
        let theme = test_theme();
        let mut surface = IcedSurface::new();
        let w = surface.measure_candidates(&candidates, &theme);
        let h = theme.bar_height() + crate::menu_panel_height();

        let mut frames: Vec<Vec<u8>> = Vec::new();
        for _ in 0..3 {
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            surface.draw_panel(
                &mut pixels,
                w,
                h,
                &candidates,
                0,
                &theme,
                &crate::PanelView::Menu(None),
            );
            frames.push(pixels);
        }
        let a = &frames[0];
        let b = &frames[1];
        let mut minx = w;
        let mut maxx = 0;
        let mut miny = h;
        let mut maxy = 0;
        let mut count = 0;
        for y in 0..h {
            for x in 0..w {
                let ia = ((y * w + x) * 4) as usize;
                if a[ia..ia + 4] != b[ia..ia + 4] {
                    count += 1;
                    minx = minx.min(x);
                    maxx = maxx.max(x);
                    miny = miny.min(y);
                    maxy = maxy.max(y);
                }
            }
        }
        assert_eq!(
            count, 0,
            "frames differ: {} px, bbox x:[{},{}] y:[{},{}]",
            count, minx, maxx, miny, maxy
        );
    }

    /// 亮/暗主题渲染都应正常完成（暗色背景非透明）。
    #[test]
    fn test_dark_theme_renders() {
        let candidates = sample_candidates();
        let theme = PanelTheme::dark(16.0, 6.0, (0x8F, 0x73, 0xE2));
        let mut surface = IcedSurface::new();
        let w = surface.measure_candidates(&candidates, &theme);
        let h = theme.bar_height();
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        surface.draw_candidates(&mut pixels, w, h, &candidates, 0, &theme);
        let at = |x: u32, y: u32| pixels[((y * w + x) * 4 + 3) as usize];
        assert!(at(w / 2, 1) != 0, "dark theme should paint background");
    }
}

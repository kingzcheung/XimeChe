//! 面板渲染主题：颜色 + 字号 + 圆角（移植自 macOS 版的 `UiStyle` + 亮/暗双模式）。
//!
//! 颜色取自 XimeYi `candidate_window.rs::ui_colors`：
//! 亮色背景 ≈ #F5F5F7（α 0.98）、暗色背景 ≈ #24262B（α 0.96）。
//! 字号/圆角/高亮色来自 `xime.yaml` 的 `style` 配置，由 daemon 构建后传入。

use iced_tiny_skia::core::Color;

use crate::CANDIDATE_HEIGHT;

/// 面板渲染主题（候选栏/菜单面板/字根窗共用）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelTheme {
    /// 是否暗色模式（决定配色，颜色字段本身已按模式填好）。
    pub dark: bool,
    /// 候选字号（`style.font_size`，默认 14）。
    pub font_size: f32,
    /// 容器圆角半径（`style.corner_radius`，默认 8）。
    pub corner_radius: f32,
    /// 主题高亮色（`color_schemes.<scheme>.primary_color`）。
    pub primary: Color,
    /// 面板背景。
    pub bg: Color,
    /// 面板边框。
    pub border: Color,
    /// 主文字颜色。
    pub text_main: Color,
    /// 次要文字颜色（序号/注释/占位）。
    pub text_comment: Color,
}

impl PanelTheme {
    /// 亮色主题。
    pub fn light(font_size: f32, corner_radius: f32, primary: (u8, u8, u8)) -> Self {
        Self {
            dark: false,
            font_size,
            corner_radius,
            primary: Color::from_rgb8(primary.0, primary.1, primary.2),
            bg: Color::from_rgba8(0xF5, 0xF5, 0xF7, 0.98),
            border: Color::from_rgb8(0xE0, 0xE0, 0xE0),
            text_main: Color::from_rgb8(0x1F, 0x1F, 0x24),
            text_comment: Color::from_rgb8(0x75, 0x75, 0x80),
        }
    }

    /// 暗色主题。
    pub fn dark(font_size: f32, corner_radius: f32, primary: (u8, u8, u8)) -> Self {
        Self {
            dark: true,
            font_size,
            corner_radius,
            primary: Color::from_rgb8(primary.0, primary.1, primary.2),
            bg: Color::from_rgba8(0x24, 0x26, 0x2B, 0.96),
            border: Color::from_rgb8(0x46, 0x49, 0x50),
            text_main: Color::from_rgb8(0xED, 0xED, 0xF2),
            text_comment: Color::from_rgb8(0x9E, 0xA0, 0xA8),
        }
    }

    /// 按模式选择亮/暗配色。
    pub fn for_mode(dark: bool, font_size: f32, corner_radius: f32, primary: (u8, u8, u8)) -> Self {
        if dark {
            Self::dark(font_size, corner_radius, primary)
        } else {
            Self::light(font_size, corner_radius, primary)
        }
    }

    /// 候选栏高度：默认字号（≤16）保持 36px 命中几何不变，
    /// 更大字号按 2×字号 + 8 适配（上限 72，避免布局失控）。
    pub fn bar_height(&self) -> u32 {
        if self.font_size <= 16.0 {
            CANDIDATE_HEIGHT
        } else {
            (self.font_size as u32 * 2 + 8).min(72)
        }
    }

    /// 高亮块圆角（比容器圆角略小，对齐 macOS 版 `corner_radius - 3`）。
    pub fn highlight_radius(&self) -> f32 {
        (self.corner_radius - 2.0).max(3.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bar_height_default_font() {
        let t = PanelTheme::light(14.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.bar_height(), CANDIDATE_HEIGHT);
        let t = PanelTheme::light(16.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.bar_height(), CANDIDATE_HEIGHT);
    }

    #[test]
    fn test_bar_height_large_font() {
        let t = PanelTheme::light(20.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.bar_height(), 48);
        // 上限 72
        let t = PanelTheme::dark(40.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.bar_height(), 72);
    }

    #[test]
    fn test_highlight_radius() {
        let t = PanelTheme::light(14.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.highlight_radius(), 6.0);
        // 极小圆角钳到 3
        let t = PanelTheme::light(14.0, 2.0, (0x8F, 0x73, 0xE2));
        assert_eq!(t.highlight_radius(), 3.0);
    }

    #[test]
    fn test_for_mode_colors_differ() {
        let light = PanelTheme::for_mode(false, 14.0, 8.0, (0x8F, 0x73, 0xE2));
        let dark = PanelTheme::for_mode(true, 14.0, 8.0, (0x8F, 0x73, 0xE2));
        assert_ne!(light.bg, dark.bg);
        assert_ne!(light.text_main, dark.text_main);
        assert_eq!(light.primary, dark.primary);
    }
}

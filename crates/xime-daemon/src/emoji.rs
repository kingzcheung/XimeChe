//! 候选栏面板「表情」页的内置表情表（纯数据，对齐 XimeYao）。
//!
//! 数据是代码内常量：离线可用、零配置、不依赖 rime 方案的 emoji 词典部署
//! （面板只负责「点一下就上屏」，不做输入编码联想）。
//!
//! 版式与寻址在 `xime_ui::menu::PanelGrid`：每类正好一页（32 格）时无翻页条。
//! 分类超过一页时由 daemon 在分类内翻页。
//!
//! 取值原则（对齐 XimeYao）：只用常见且多数 emoji 字体稳定有字形的表情，
//! 不追新（Unicode 14+ 的新表情在旧字体上会画成方框）；不用 ZWJ 组合序列
//! （👨‍👩‍👧 之类）——那些在格子里会被挤成一团，且宽度不稳定。

/// 一个内置分类：标签 + 该分类的字形。
pub struct EmojiGroup {
    pub category: &'static str,
    pub symbols: &'static [&'static str],
}

pub static GROUPS: &[EmojiGroup] = &[
    EmojiGroup {
        category: "常用",
        symbols: &[
            "😀", "😄", "😁", "😂", "🤣", "😊", "😍", "🥰", "😘", "😎", "🤔", "🙄", "😅", "😇",
            "😉", "😌", "😴", "😭", "😢", "😡", "😱", "🥺", "🤗", "🤝", "🙏", "👍", "👌", "✌️",
            "👏", "🎉", "❤️", "🔥",
        ],
    },
    EmojiGroup {
        category: "人物",
        symbols: &[
            "😃", "😆", "☺️", "🙂", "🙃", "😋", "😛", "😜", "🤪", "😝", "🤭", "🤫", "🤐", "😐",
            "😑", "😶", "😏", "😒", "😞", "😔", "😟", "😕", "🙁", "😣", "😖", "😫", "😩", "🥺",
            "😤", "😠", "🤬", "😈",
        ],
    },
    EmojiGroup {
        category: "手势",
        symbols: &[
            "👍", "👎", "👌", "✌️", "🤞", "🤟", "🤘", "🤙", "👈", "👉", "👆", "👇", "☝️", "✋",
            "🤚", "🖐️", "🖖", "👋", "🤝", "🙏", "💪", "🤛", "🤜", "👏", "🙌", "👐", "🤲", "✊",
            "👊", "💅", "🤳", "🙋",
        ],
    },
    EmojiGroup {
        category: "自然",
        symbols: &[
            "🌸", "🌹", "🌺", "🌻", "🌷", "🌱", "🌲", "🌳", "🌴", "🌵", "🌾", "🌿", "🍀", "🍁",
            "🍂", "🍃", "🌍", "🌎", "🌏", "🌙", "⭐", "🌟", "✨", "⚡", "☀️", "⛅", "☁️", "🌧️",
            "⛈️", "❄️", "🌈", "🌊",
        ],
    },
    EmojiGroup {
        category: "食物",
        symbols: &[
            "🍎", "🍐", "🍊", "🍋", "🍌", "🍉", "🍇", "🍓", "🍒", "🍑", "🥭", "🍍", "🥥", "🥝",
            "🍅", "🥑", "🥦", "🥕", "🌽", "🌶️", "🥒", "🍞", "🥐", "🥖", "🧀", "🥚", "🍳", "🥓",
            "🍔", "🍕", "🍟", "🌭",
        ],
    },
    EmojiGroup {
        category: "符号",
        symbols: &[
            "❤️", "🧡", "💛", "💚", "💙", "💜", "🖤", "🤍", "💔", "❣️", "💕", "💞", "💓", "💗",
            "💖", "💘", "💝", "💟", "✅", "❌", "⭕", "❗", "❓", "⚠️", "♻️", "🔰", "⚜️", "🔱",
            "📛", "♠️", "♥️", "♦️",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_category_is_the_common_one() {
        // 打开表情页默认落在「常用」（最近使用是它前面的第 0 个标签）。
        assert_eq!(GROUPS.first().map(|g| g.category), Some("常用"));
    }

    #[test]
    fn every_glyph_is_a_single_emoji_character() {
        // 格子里只画一个表情：1~2 个 char（变体选择符 FE0F 是第 2 个）。
        for group in GROUPS {
            for emoji in group.symbols {
                let chars: Vec<char> = emoji.chars().collect();
                assert!(
                    (1..=2).contains(&chars.len()),
                    "{} 里的 {} 是 {} 个 char，格子放不下",
                    group.category,
                    emoji,
                    chars.len()
                );
                assert!(
                    !emoji.contains('\u{200d}'),
                    "{} 里的 {} 是 ZWJ 组合序列，网格里会挤成一团",
                    group.category,
                    emoji
                );
            }
        }
    }

    #[test]
    fn builtin_categories_are_non_empty_with_unique_glyphs() {
        for group in GROUPS {
            assert!(!group.symbols.is_empty(), "{} 分类为空", group.category);
            let mut seen: Vec<&str> = group.symbols.to_vec();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(
                seen.len(),
                group.symbols.len(),
                "{} 分类内字形重复",
                group.category
            );
        }
    }

    #[test]
    fn every_category_fits_one_page() {
        // 每类恰好 ≤ 32 个（一页）：表情页不出现翻页条（对齐 XimeYao）。
        for group in GROUPS {
            assert!(
                group.symbols.len() <= xime_ui::GRID_PER_PAGE,
                "表情「{}」有 {} 个，超过一页",
                group.category,
                group.symbols.len()
            );
        }
    }
}

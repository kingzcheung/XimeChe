pub mod candidate;
pub mod iced_view;
pub mod menu;
pub mod theme;

pub use candidate::CandidateItem;
pub use candidate::CandidateList;
pub use candidate::MoveDirection;
pub use candidate::PageInfo;
pub use iced_view::IcedSurface;
pub use menu::{
    content_capacity, content_cell_width, content_columns_for, content_item_hit,
    content_panel_height, content_panel_width, content_text_width, expanded_height, list_page_hit,
    list_panel_height, list_row_y, menu_button_hit, menu_item_hit, menu_panel_height,
    panel_height_for, truncate_to_width, GridItem, ListHit, ListItem, ListKind, ListRowButton,
    MenuAction, PanelView, CANDIDATE_HEIGHT, CONTENT_COLUMNS_MAX, CONTENT_GAP, CONTENT_ITEM_SIZE,
    CONTENT_MAX_WIDTH, CONTENT_ROWS, LIST_BACK_WIDTH, LIST_BUTTON_SIZE, LIST_HEADER_HEIGHT,
    LIST_H_INSET, LIST_LIMIT, LIST_MIN_PANEL_WIDTH, LIST_MORE_HEIGHT, LIST_ROW_HEIGHT,
    LIST_ROW_INSET, MENU_BUTTON_WIDTH, MENU_COLUMNS, MENU_ITEM_HEIGHT,
};
pub use theme::PanelTheme;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to create pixmap: {0}")]
    PixmapCreationFailed(String),

    #[error("Failed to render: {0}")]
    RenderFailed(String),
}

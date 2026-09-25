use crate::sni::StatusNotifierItemSignals;
use crate::{DBusMenu, InputMode, MenuAction, StatusNotifierItem};
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver};
use tracing::{debug, warn};
use zbus::object_server::InterfaceRef;
use zbus::Connection;

const SNI_WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";
const SNI_WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const SNI_WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const SNI_OBJECT: &str = "/StatusNotifierItem";
const MENU_OBJECT: &str = "/MenuBar";

pub struct TrayManager {
    connection: Connection,
    sni_ref: InterfaceRef<StatusNotifierItem>,
}

impl TrayManager {
    pub async fn register(
        connection: &Connection,
    ) -> zbus::Result<(Self, Receiver<()>, Receiver<MenuAction>)> {
        let (toggle_tx, toggle_rx) = channel::<()>(1);
        let (action_tx, action_rx) = channel::<MenuAction>(1);

        connection
            .object_server()
            .at(MENU_OBJECT, DBusMenu::with_action_channel(action_tx))
            .await?;
        connection
            .object_server()
            .at(
                SNI_OBJECT,
                StatusNotifierItem::with_toggle_channel(toggle_tx),
            )
            .await?;

        let sni_ref = connection
            .object_server()
            .interface::<_, StatusNotifierItem>(SNI_OBJECT)
            .await?;

        // 开机时 KWin 会先于 Plasma 托盘拉起输入法，此时 StatusNotifierWatcher
        // 还没出现在总线上。注册失败不能让 daemon 退出（否则输入法"开机不自启"），
        // 转入后台指数退避重试，托盘服务就绪后自动补注册。
        if let Err(e) = Self::register_with_watcher(connection).await {
            warn!("SNI watcher unavailable ({}), retrying in background", e);
            Self::spawn_registration_retry(connection.clone());
        } else {
            debug!("SNI registered successfully (initially hidden)");
        }
        Ok((
            Self {
                connection: connection.clone(),
                sni_ref,
            },
            toggle_rx,
            action_rx,
        ))
    }

    async fn register_with_watcher(connection: &Connection) -> zbus::Result<()> {
        connection
            .call_method(
                Some(SNI_WATCHER_SERVICE),
                SNI_WATCHER_OBJECT,
                Some(SNI_WATCHER_INTERFACE),
                "RegisterStatusNotifierItem",
                &(connection
                    .unique_name()
                    .map(|n| n.to_string())
                    .unwrap_or_default()),
            )
            .await
            .map(|_| ())
    }

    fn spawn_registration_retry(connection: Connection) {
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(1);
            loop {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(15));
                match Self::register_with_watcher(&connection).await {
                    Ok(()) => {
                        debug!("SNI registered successfully after retry");
                        break;
                    }
                    Err(e) => debug!("SNI registration retry failed: {}", e),
                }
            }
        });
    }

    /// 请求 KWin 强制激活输入法（org.kde.kwin.VirtualKeyboard.forceActivate）。
    ///
    /// IM 未被 KWin 激活时键盘事件不经过 IM，托盘点击是唯一控制通道；
    /// 用户点击托盘切换模式时顺带调用，让切换在"卡死"状态下也能生效。
    pub async fn force_activate_im(&self) {
        if let Err(e) = self
            .connection
            .call_method(
                Some("org.kde.KWin"),
                "/VirtualKeyboard",
                Some("org.kde.kwin.VirtualKeyboard"),
                "forceActivate",
                &(),
            )
            .await
        {
            debug!("forceActivate failed: {}", e);
        }
    }

    pub async fn set_mode(&self, mode: InputMode) {
        let iface = self.sni_ref.get_mut().await;
        iface.set_mode(mode);
        self.sni_ref.new_icon().await.ok();
        self.sni_ref.new_tool_tip().await.ok();
    }

    pub async fn set_visible(&self, visible: bool) {
        let iface = self.sni_ref.get_mut().await;
        let was_visible = iface.is_visible();
        iface.set_visible(visible);

        if was_visible != visible {
            let status = if visible { "Active" } else { "Passive" };
            self.sni_ref.new_status(status).await.ok();
            if visible {
                self.sni_ref.new_icon().await.ok();
            }
            debug!("Tray visibility changed to {}", status);
        }
    }

    pub async fn get_mode(&self) -> InputMode {
        let iface = self.sni_ref.get().await;
        iface.get_mode()
    }

    pub async fn set_primary_color(&self, color: (u8, u8, u8)) {
        let iface = self.sni_ref.get_mut().await;
        iface.set_primary_color(color);
        self.sni_ref.new_icon().await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sni_watcher_constants() {
        assert_eq!(SNI_WATCHER_SERVICE, "org.kde.StatusNotifierWatcher");
        assert_eq!(SNI_WATCHER_OBJECT, "/StatusNotifierWatcher");
        assert_eq!(SNI_WATCHER_INTERFACE, "org.kde.StatusNotifierWatcher");
    }

    #[test]
    fn test_sni_path_constants() {
        assert_eq!(SNI_OBJECT, "/StatusNotifierItem");
        assert_eq!(MENU_OBJECT, "/MenuBar");
    }

    #[test]
    fn test_input_mode_constants() {
        // Verify InputMode enum values
        assert_eq!(InputMode::Chinese as i32, 0);
        assert_eq!(InputMode::English as i32, 1);
    }

    #[test]
    fn test_input_mode_debug() {
        assert_eq!(format!("{:?}", InputMode::Chinese), "Chinese");
        assert_eq!(format!("{:?}", InputMode::English), "English");
    }

    #[test]
    fn test_input_mode_clone() {
        let mode = InputMode::Chinese;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    #[test]
    fn test_input_mode_equality() {
        assert_eq!(InputMode::Chinese, InputMode::Chinese);
        assert_eq!(InputMode::English, InputMode::English);
        assert_ne!(InputMode::Chinese, InputMode::English);
    }
}

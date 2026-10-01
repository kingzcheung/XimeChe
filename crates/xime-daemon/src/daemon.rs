use nix::unistd::dup;
use std::os::fd::AsFd;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use tracing::debug;
use zbus::interface;
use zbus::zvariant::Fd;

use crate::DaemonCommand;

pub struct XimeDaemon {
    command_tx: Sender<DaemonCommand>,
}

impl Clone for XimeDaemon {
    fn clone(&self) -> Self {
        Self {
            command_tx: self.command_tx.clone(),
        }
    }
}

impl XimeDaemon {
    pub fn new(command_tx: Sender<DaemonCommand>) -> Self {
        Self { command_tx }
    }
}

#[interface(name = "org.xime.Xime.Controller")]
impl XimeDaemon {
    async fn open_wayland_socket(&self, fd: Fd<'_>, display_name: String) -> zbus::fdo::Result<()> {
        let raw_fd = fd.as_raw_fd();
        debug!(
            "Received OpenWaylandSocket(fd={}, display={})",
            raw_fd, display_name
        );

        let owned_fd = dup(fd.as_fd())
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to dup fd: {}", e)))?;

        self.command_tx
            .send(DaemonCommand::OpenWaylandSocket(owned_fd, display_name))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        Ok(())
    }

    async fn deploy(&self) -> zbus::fdo::Result<()> {
        debug!("Received Deploy request");
        self.command_tx
            .send(DaemonCommand::Deploy)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn reload_style(&self) -> zbus::fdo::Result<()> {
        debug!("Received ReloadStyle request");
        self.command_tx
            .send(DaemonCommand::ReloadStyle)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn reload_plugins(&self) -> zbus::fdo::Result<()> {
        debug!("Received ReloadPlugins request");
        self.command_tx
            .send(DaemonCommand::ReloadPlugins)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn select_schema(&self, schema_id: String) -> zbus::fdo::Result<bool> {
        debug!("Received SelectSchema request: {}", schema_id);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(DaemonCommand::SelectSchema(schema_id, result_tx))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        result_rx
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// 优雅退出：进程以状态码 0 结束（kill -15 会被 KWin 记为
    /// QProcess::CrashExit，多次后触发其崩溃保护拒绝重启输入法）。
    async fn shutdown(&self) -> zbus::fdo::Result<()> {
        debug!("Received Shutdown request");
        self.command_tx
            .send(DaemonCommand::Shutdown)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    /// 列出用户词典（设置程序「词典管理」页；JSON 传输，对齐 XimeYao IPC 语义）。
    ///
    /// levers 调用是阻塞的：zbus object server 不在 tokio 上下文，
    /// spawn_blocking 会 panic 导致方法永不回包，改用 std 线程 + oneshot
    /// （tokio oneshot 自身不依赖 runtime）。
    async fn list_user_dicts(&self) -> zbus::fdo::Result<String> {
        debug!("Received ListUserDicts request");
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::user_dict::list_dicts());
        });
        let result = rx
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// 读取一个用户词典的词条（关会话→导出→重建，在 wayland 线程执行）。
    async fn list_dict_entries(&self, dict: String, query: String) -> zbus::fdo::Result<String> {
        debug!("Received ListDictEntries request: {dict} query={query:?}");
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(crate::DaemonCommand::ListDictEntries(
                dict, query, result_tx,
            ))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let result = result_rx
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        match result {
            Ok(entries) => {
                serde_json::to_string(&entries).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }
}

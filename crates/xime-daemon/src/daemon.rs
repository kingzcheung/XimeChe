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

    /// 用户词典写操作（造词/删除/备份/恢复/导出/导入），wayland 线程
    /// `with_user_dict_closed` 内执行。参数为 UserDictOp 的 JSON，返回条数
    /// （Backup/Restore 成功 = 1）。
    async fn user_dict_op(&self, op_json: String) -> zbus::fdo::Result<i64> {
        debug!("Received UserDictOp request: {op_json:?}");
        let op: crate::user_dict::UserDictOp = serde_json::from_str(&op_json)
            .map_err(|e| zbus::fdo::Error::Failed(format!("参数无效: {e}")))?;
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        self.command_tx
            .send(crate::DaemonCommand::UserDictOp(op, result_tx))
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        result_rx
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .map_err(zbus::fdo::Error::Failed)
    }

    /// 读取某方案的快捷短语表（纯文件操作，直接执行）。
    async fn list_custom_phrases(&self, schema_id: String) -> zbus::fdo::Result<String> {
        debug!("Received ListCustomPhrases request: {schema_id}");
        let result = crate::custom_phrase::list_phrases(&crate::get_config_dir(), &schema_id);
        match result {
            Ok(list) => {
                serde_json::to_string(&list).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// 整表保存某方案的快捷短语（写文件 + 视需要注入 patch；不部署）。
    async fn save_custom_phrases(
        &self,
        schema_id: String,
        entries_json: String,
    ) -> zbus::fdo::Result<String> {
        debug!("Received SaveCustomPhrases request: {schema_id}");
        let entries: Vec<crate::custom_phrase::CustomPhraseEntry> =
            serde_json::from_str(&entries_json)
                .map_err(|e| zbus::fdo::Error::Failed(format!("参数无效: {e}")))?;
        let result =
            crate::custom_phrase::save_phrases(&crate::get_config_dir(), &schema_id, &entries);
        match result {
            Ok(saved) => {
                serde_json::to_string(&saved).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Err(e) => Err(zbus::fdo::Error::Failed(e)),
        }
    }

    /// 读取某方案的词表词条（只读，纯文件操作 + 进程内缓存）。
    async fn list_schema_entries(
        &self,
        schema_id: String,
        query: String,
    ) -> zbus::fdo::Result<String> {
        debug!("Received ListSchemaEntries request: {schema_id} query={query:?}");
        // 单目录模型：方案文件与用户数据同一目录（启动时已部署）。
        let result =
            crate::schema_dict::read_schema_dict(&crate::get_config_dir(), &schema_id, &query);
        match result {
            Ok(read) => {
                serde_json::to_string(&read).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
            }
            Err(e) => Err(zbus::fdo::Error::Failed(e.to_string())),
        }
    }

    /// 语音状态快照（设置程序「语音转文本」页 250ms 轮询；JSON 结构对齐
    /// libximecore speech_models 的 SpeechServerStatus，设置端反序列化）。
    async fn get_speech_status(&self) -> zbus::fdo::Result<String> {
        Ok(crate::speech::status_json())
    }

    // 语音操作方法全部返回**操作后的状态快照 JSON**——设置端回调签名是
    // fn(id) -> Option<SpeechServerStatus>，返回空会让页面误报「服务未运行」；
    // 顺带让页面一次往返拿到最新状态（下载进度/选择结果立即可见）。
    async fn download_speech_model(&self, model_id: String) -> zbus::fdo::Result<String> {
        debug!("Received DownloadSpeechModel request: {model_id}");
        crate::speech::download_model(&model_id);
        Ok(crate::speech::status_json())
    }

    async fn delete_speech_model(&self, model_id: String) -> zbus::fdo::Result<String> {
        debug!("Received DeleteSpeechModel request: {model_id}");
        crate::speech::delete_model(&model_id);
        Ok(crate::speech::status_json())
    }

    async fn select_speech_model(&self, model_id: String) -> zbus::fdo::Result<String> {
        debug!("Received SelectSpeechModel request: {model_id}");
        crate::speech::select_model(&model_id);
        Ok(crate::speech::status_json())
    }

    /// 开始试听（与候选栏 🎙️ 同一条听写会话；识别文本进状态快照的 text）。
    async fn speech_test_start(&self) -> zbus::fdo::Result<String> {
        debug!("Received SpeechTestStart request");
        crate::speech::test_start();
        Ok(crate::speech::status_json())
    }

    /// 结束试听。
    async fn speech_test_stop(&self) -> zbus::fdo::Result<String> {
        debug!("Received SpeechTestStop request");
        crate::speech::test_stop();
        Ok(crate::speech::status_json())
    }
}

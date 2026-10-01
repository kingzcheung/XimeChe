//! 用户词典读路径（设置程序「词典管理」页的数据通道）。
//!
//! librime 没有"读词条"的 C 接口，走 levers 的导出通道：导出到临时文件
//! （`/tmp/xime_dict_<dict>_<pid>_<seq>.txt`，读完即删，序列号防同进程
//! 并发撞名）→ 解析文本码表 → 关键词过滤 → 截断 500 条回传。解析只认
//! `词⇥码⇥频率`：头部 `#`/`#@` 元信息行与空行跳过、缺列退化（码为空、
//! 频率 1）、频率 < 0 的 tombstone 不展示。词典名先过文件名清洗。
//!
//! 并发说明：levers 导出不碰会话对象（XimeYao 实测输入法运行中导出成功），
//! 与按键路径并发直调；多条 levers 调用之间经 [`RIME_LEVERS_GATE`] 串行。

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 一次回传的词条上限（DBus 单消息体量约束，对齐 XimeYao winxime-ipc）。
pub const DICT_ENTRIES_MAX: usize = 500;

/// 串行化 levers 调用（librime 全局状态无文档线程保证，self-protect）。
pub static RIME_LEVERS_GATE: Mutex<()> = Mutex::new(());

/// 用户词典中的一条词条（词 / 编码 / 频率）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictEntryRow {
    pub word: String,
    pub code: String,
    pub commits: i32,
}

/// 用户词典列表结果（词典名 + 快照目录）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DictListResult {
    pub dicts: Vec<String>,
    pub sync_dir: String,
}

/// 词条读取结果（词库总数 + 命中数 + 本次返回的词条）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DictEntriesResult {
    /// 词库词条总数（未受关键词过滤影响）。
    pub total: i32,
    /// 命中条数（**未**受回传上限影响，用来判断回传是否被截断）。
    pub matched: i32,
    /// 本次返回的词条（已按关键词过滤，最多 [`DICT_ENTRIES_MAX`] 条）。
    pub entries: Vec<DictEntryRow>,
}

/// 临时文件序列号（防同进程并发导出同名词典撞名）。
static EXPORT_SEQ: AtomicU64 = AtomicU64::new(0);

/// 词典名文件名清洗：只留字母数字与 `_-`，防路径字符进临时文件名。
pub fn sanitize_dict_name(dict: &str) -> String {
    dict.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// 解析导出的文本码表：跳过 `#`/`#@` 头部与空行，`词⇥码[⇥频率]` 缺列退化，
/// 频率 < 0 的 tombstone 不展示。
pub fn parse_user_dict_text(text: &str) -> Vec<DictEntryRow> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut cols = line.split('\t');
        let Some(word) = cols.next().map(str::trim) else {
            continue;
        };
        if word.is_empty() {
            continue;
        }
        let code = cols.next().map(str::trim).unwrap_or("");
        let commits: i32 = cols.next().and_then(|c| c.trim().parse().ok()).unwrap_or(1);
        if commits < 0 {
            continue; // tombstone
        }
        out.push(DictEntryRow {
            word: word.to_string(),
            code: code.to_string(),
            commits,
        });
    }
    out
}

/// 关键词匹配：词或编码包含（大小写不敏感）；空关键词命中全部。
pub fn matches_query(entry: &DictEntryRow, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    entry.word.to_lowercase().contains(needle_lower)
        || entry.code.to_lowercase().contains(needle_lower)
}

/// 列出全部用户词典 + 快照目录。
pub fn list_dicts() -> DictListResult {
    let _gate = RIME_LEVERS_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let dicts = librime::list_user_dicts();
    let (_, user_dir) = xime_config::get_data_dirs();
    DictListResult {
        dicts,
        sync_dir: user_dir.join("sync").to_string_lossy().into_owned(),
    }
}

/// 导出并读取一个用户词典（过滤 + 截断）。
pub fn list_entries(dict: &str, query: &str) -> Result<DictEntriesResult, String> {
    let dict_clean = sanitize_dict_name(dict);
    if dict_clean.is_empty() {
        return Err("词典名无效".to_string());
    }
    let seq = EXPORT_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "xime_dict_{}_{:x}_{}_{seq}.txt",
        dict_clean,
        dict_clean.len(),
        std::process::id()
    ));
    let path_str = path.to_string_lossy().into_owned();

    let count = {
        let _gate = RIME_LEVERS_GATE.lock().unwrap_or_else(|e| e.into_inner());
        librime::export_user_dict(&dict_clean, &path_str).map_err(|e| e.to_string())?
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);

    let needle = query.trim().to_lowercase();
    let entries = parse_user_dict_text(&text);
    let matched_entries: Vec<DictEntryRow> = entries
        .iter()
        .filter(|e| matches_query(e, &needle))
        .cloned()
        .collect();
    Ok(DictEntriesResult {
        total: count,
        matched: matched_entries.len() as i32,
        entries: matched_entries.into_iter().take(DICT_ENTRIES_MAX).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Rime user dict export\n#@/db_name wubi86\n\
词一\tab\t12\n词二\tcd\n大词\tefgh\t0\n墓碑词\txy\t-1\n\n";
    #[test]
    fn parse_skips_headers_and_tombstones() {
        let rows = parse_user_dict_text(SAMPLE);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            DictEntryRow {
                word: "词一".into(),
                code: "ab".into(),
                commits: 12
            }
        );
        assert_eq!(
            rows[1],
            DictEntryRow {
                word: "词二".into(),
                code: "cd".into(),
                commits: 1
            }
        );
        assert_eq!(
            rows[2],
            DictEntryRow {
                word: "大词".into(),
                code: "efgh".into(),
                commits: 0
            }
        );
    }

    #[test]
    fn query_matches_word_and_code_case_insensitive() {
        let e = DictEntryRow {
            word: "Hello".into(),
            code: "AB".into(),
            commits: 1,
        };
        assert!(matches_query(&e, "hel"));
        assert!(matches_query(&e, "ab"));
        assert!(!matches_query(&e, "xyz"));
        assert!(matches_query(&e, ""));
    }

    #[test]
    fn sanitize_strips_path_chars() {
        assert_eq!(sanitize_dict_name("wubi86"), "wubi86");
        assert_eq!(sanitize_dict_name("../etc/pass"), "etcpass");
        assert_eq!(sanitize_dict_name("../../.."), "");
    }

    #[test]
    fn entries_cap_respected() {
        let text: String = (0..600).map(|i| format!("词{i}\tcode{i}\t1\n")).collect();
        let rows = parse_user_dict_text(&text);
        assert_eq!(rows.len(), 600);
        let matched: Vec<DictEntryRow> = rows
            .iter()
            .filter(|e| matches_query(e, ""))
            .cloned()
            .collect();
        assert_eq!(matched.len().min(DICT_ENTRIES_MAX), DICT_ENTRIES_MAX);
    }
}

// 一次性验证：模型下载 → 解压 → 校验链路（复用 daemon 的逻辑）
fn main() {
    let profile = xime_speech::AsrModelRegistry::default_profile();
    println!("模型: {} ({})", profile.name, profile.size);
    let dir = std::path::Path::new(&std::env::var("HOME").unwrap())
        .join(format!(".local/share/xime/models/{}", profile.id));
    let ready = [
        &profile.encoder_file,
        &profile.decoder_file,
        &profile.joiner_file,
        &profile.tokens_file,
    ]
    .iter()
    .all(|f| dir.join(f).is_file());
    if ready {
        println!("模型已就绪: {}", dir.display());
        return;
    }
    println!("开始下载到 {:?} …", dir);
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("_download.tar.bz2");
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) xime-input-method")
        .build()
        .unwrap();
    let mut resp = client.get(&profile.download_url).send().unwrap();
    assert!(resp.status().is_success(), "HTTP {}", resp.status());
    let total = resp.content_length().unwrap_or(0);
    use std::io::{Read, Write};
    let mut file = std::fs::File::create(&archive).unwrap();
    let mut downloaded = 0u64;
    let mut buf = [0u8; 256 * 1024];
    loop {
        let n = resp.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).unwrap();
        downloaded += n as u64;
        if total > 0 && downloaded % (8 * 1024 * 1024) < buf.len() as u64 {
            println!("  {:.0}%", downloaded as f32 / total as f32 * 100.0);
        }
    }
    println!("下载完成 {downloaded} 字节，解压…");
    let bz = bzip2::read::BzDecoder::new(std::fs::File::open(&archive).unwrap());
    let work = dir.join("_extract");
    tar::Archive::new(bz).unpack(&work).unwrap();
    for name in [
        &profile.encoder_file,
        &profile.decoder_file,
        &profile.joiner_file,
        &profile.tokens_file,
    ] {
        // 递归找
        fn find(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
            let mut stack = vec![root.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).unwrap() {
                    let p = e.unwrap().path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p.file_name().is_some_and(|n| n == name) {
                        return Some(p);
                    }
                }
            }
            None
        }
        let found = find(&work, name).expect(name);
        std::fs::copy(&found, dir.join(name)).unwrap();
        println!("  ✓ {name}");
    }
    std::fs::remove_dir_all(&work).unwrap();
    std::fs::remove_file(&archive).unwrap();
    println!("模型就绪: {}", dir.display());
}

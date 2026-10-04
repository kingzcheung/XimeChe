use std::path::Path;

fn main() {
    // rpath 只能由 bin 包的 build.rs 打（rlib 的 rustc-link-arg 不传播到
    // 最终二进制）。两条用途：
    // 1. $ORIGIN/../share/xime/lib —— 语音运行库（sherpa-onnx/onnxruntime）
    //    的安装位，dev（~/.local/bin）与系统包（/usr/bin）布局通用；
    // 2. librime 子模块 dist —— 开发机构建链接子模块产物（系统 librime
    //    无 lua 插件且版本旧，退出析构还会崩）；缺失时该条目被 ld 忽略。
    // 单条 -rpath 冒号分隔（lld 下多条 -rpath 互相覆盖）。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dist_lib = Path::new(&manifest_dir).join("../../../libximecore/librime/dist/lib");
    let dist = if dist_lib.join("librime.so").exists() {
        let canonical = dist_lib.canonicalize().unwrap_or(dist_lib);
        canonical.to_string_lossy().to_string()
    } else {
        String::new()
    };
    let rpath = if dist.is_empty() {
        "$ORIGIN/../share/xime/lib".to_string()
    } else {
        format!("{dist}:$ORIGIN/../share/xime/lib")
    };
    println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
    println!("cargo:rerun-if-changed=build.rs");
}

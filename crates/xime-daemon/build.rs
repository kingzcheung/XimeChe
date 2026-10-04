fn main() {
    // 语音运行库（sherpa-onnx / onnxruntime）随 dev-install / 系统包落在
    // 二进制同级结构下的 ../share/xime/lib（dev：~/.local/bin →
    // ~/.local/share/xime/lib；系统：/usr/bin → /usr/share/xime/lib），
    // $ORIGIN 相对 rpath 两种布局通用。
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../share/xime/lib");
}

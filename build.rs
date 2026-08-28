//! 嵌入 exe 图标(assets/icon.ico)

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }
    if let Err(e) = winresource::WindowsResource::new()
        .set_icon("assets/icon.ico")
        .compile()
    {
        println!("cargo::warning=图标嵌入失败: {e}");
    }
}

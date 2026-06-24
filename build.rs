// 给生成的 exe 加程序图标。仅当 assets/app.ico 存在时生效，否则跳过，
// 这样没有图标文件也能正常编译。
fn main() {
    #[cfg(windows)]
    {
        let ico = std::path::Path::new("assets/app.ico");
        if ico.exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/app.ico");
            // 编译失败不应阻断整个构建（例如缺少 rc 工具链时）。
            if let Err(e) = res.compile() {
                println!("cargo:warning=嵌入图标失败（忽略）：{e}");
            }
        } else {
            println!("cargo:warning=未找到 assets/app.ico，跳过图标嵌入");
        }
    }
}

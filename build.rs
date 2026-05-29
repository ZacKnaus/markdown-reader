fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/app-icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app-icon.ico");
        resource.set("FileDescription", "Markdown Reader");
        resource.set("ProductName", "Markdown Reader");
        resource.set("InternalName", "Markdown Reader");
        resource.set("OriginalFilename", "markdown-reader.exe");
        resource
            .compile()
            .expect("failed to compile Windows application resources");
    }
}

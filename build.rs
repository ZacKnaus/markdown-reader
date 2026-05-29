fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/app-icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/app-icon.ico");
        resource
            .compile()
            .expect("failed to compile Windows application resources");
    }
}

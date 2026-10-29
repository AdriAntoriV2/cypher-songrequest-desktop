fn main() {
    println!("cargo:rerun-if-changed=asset/icon.ico");
    println!("cargo:rerun-if-changed=ui/main.slint");

    slint_build::compile("ui/main.slint").unwrap();

    #[cfg(target_os = "windows")]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("asset/icon.ico");

        resource
            .compile()
            .expect("Failed to embed the CypherSR Desktop Windows icon");
    }
}
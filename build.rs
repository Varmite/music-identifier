fn main() {
    // Embed the exe icon on Windows; other platforms get it from the bundle / window.
    #[cfg(windows)]
    embed_resource::compile("packaging/windows/icon.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
    println!("cargo:rerun-if-changed=packaging/icon.ico");
}

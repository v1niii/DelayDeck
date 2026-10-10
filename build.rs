fn main() {
    println!("cargo:rerun-if-changed=assets/delaydeck.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/delaydeck.ico")
        .set("ProductName", "DelayDeck")
        .set("FileDescription", "DelayDeck stream delay")
        .set("OriginalFilename", "DelayDeck.exe");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed the app icon ({e}); install MinGW windres or the Windows SDK");
    }
}

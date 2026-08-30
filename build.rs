fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    winresource::WindowsResource::new()
        .set_icon("assets/icon.ico")
        .compile()
        .expect("could not embed the application icon");
}

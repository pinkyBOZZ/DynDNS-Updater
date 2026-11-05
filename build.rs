fn main() {
    let config = slint_build::CompilerConfiguration::new().with_bundled_translations("lang");
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // Icon resource id 1: used by Explorer for the .exe and by the tray (Icon::from_resource(1)).
    winresource::WindowsResource::new().set_icon_with_id("assets/icon.ico", "1").compile().unwrap();
}

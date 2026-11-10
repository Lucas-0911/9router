fn main() {
    #[cfg(feature = "desktop-app")]
    tauri_build::build();
}

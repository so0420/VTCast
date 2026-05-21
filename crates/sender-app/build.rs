fn main() {
    // Force a rebuild when any frontend asset changes — tauri::generate_context!
    // embeds them at compile time, so without this the binary stays stale.
    println!("cargo:rerun-if-changed=ui");
    tauri_build::build();
}

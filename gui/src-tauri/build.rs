fn main() {
    // The web assets are embedded at compile time. Without this, rebuilding
    // only the frontend leaves the old interface inside the binary.
    println!("cargo:rerun-if-changed=../dist");
    tauri_build::build()
}

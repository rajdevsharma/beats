fn main() {
    // Link Rubber Band (install with: brew install rubberband)
    println!("cargo:rustc-link-lib=rubberband");
    println!("cargo:rustc-link-search=native=/opt/homebrew/lib"); // Apple Silicon
    println!("cargo:rustc-link-search=native=/usr/local/lib");    // Intel Mac
    println!("cargo:rerun-if-changed=build.rs");

    tauri_build::build()
}

fn main() {
    // DLLs the exe needs are loaded from System32 only, never from the
    // exe's folder, where anyone who can write there could put a DLL of the
    // same name (LOAD_LIBRARY_SEARCH_SYSTEM32).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    tauri_build::build()
}

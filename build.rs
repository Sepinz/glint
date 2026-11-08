// Embeds a Common-Controls v6 manifest into the GUI binary (visual styles,
// double-buffered list view). Only the MSVC linker understands these flags.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=glint=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=glint=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}

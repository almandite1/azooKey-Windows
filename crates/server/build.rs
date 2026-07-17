use std::env;
use std::path::Path;

fn main() {
    // azookey-server.lib (built from server-swift) is copied to the
    // workspace root by `cargo make build_swift`. CARGO_MANIFEST_DIR is
    // crates/server, so the root is two levels up. Linking only worked
    // before because MSVC link.exe implicitly searches the cwd; point the
    // linker at the root explicitly so it keeps working under rust-lld.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("crates/server should sit two levels below the workspace root");

    println!("cargo:rustc-link-search={}", workspace_root.display());
    println!("cargo:rustc-link-lib=azookey-server");
}

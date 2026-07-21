fn main() {
    println!("cargo:rerun-if-changed=res/ui.rc");
    println!("cargo:rerun-if-changed=res/ui.manifest");

    // required, not optional: the whole point of the manifest is that the
    // DPI awareness does not depend on tao winning an initialization race,
    // so an executable that silently shipped without it would be the bug
    // this is meant to prevent (issue #30)
    if let Err(e) = embed_resource::compile("res/ui.rc", embed_resource::NONE).manifest_required() {
        panic!("failed to embed the ui.exe manifest: {e}");
    }
}

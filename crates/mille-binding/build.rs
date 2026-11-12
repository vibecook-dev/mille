fn main() {
    napi_build::setup();

    // On macOS the linker writes a dylib's own install name (LC_ID_DYLIB) as the path it linked
    // the file at, which is inside the build machine's target directory: a release build's addon
    // carried `/Users/runner/work/mille/mille/target/<triple>/release/deps/libmille_binding.dylib`.
    // Nothing loads the addon by that name, since Node dlopens the .node by its own path, so the
    // name only ships a build-machine path in every macOS binary. `--remap-path-prefix` cannot
    // reach it (it rewrites source paths, and this is a linker output path), so name it relative.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libmille_binding.dylib");
    }
}

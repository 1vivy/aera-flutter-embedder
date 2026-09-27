fn main() {
    println!("cargo:rerun-if-changed=vendor/flutter_embedder.h");
    let bindings = bindgen::Builder::default()
        .header("vendor/flutter_embedder.h")
        .allowlist_type("Flutter.*")
        .allowlist_var("FLUTTER_.*")
        .allowlist_function("Flutter.*")
        .prepend_enum_name(false)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .layout_tests(false)
        .generate_comments(false)
        .generate()
        .expect("generate flutter_embedder.h bindings");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("flutter_embedder.rs")).unwrap();
}

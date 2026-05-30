use std::path::PathBuf;

fn main() {
    let src_dir = PathBuf::from("src");

    let mut build = cc::Build::new();
    build.include(&src_dir);
    build.flag_if_supported("-Wno-unused-parameter");
    build.flag_if_supported("-Wno-unused-but-set-variable");
    build.flag_if_supported("-Wno-trigraphs");

    let parser = src_dir.join("parser.c");
    build.file(&parser);
    println!("cargo:rerun-if-changed={}", parser.display());

    let scanner = src_dir.join("scanner.c");
    if scanner.exists() {
        build.file(&scanner);
        println!("cargo:rerun-if-changed={}", scanner.display());
    }

    build.compile("tree-sitter-m1");
}

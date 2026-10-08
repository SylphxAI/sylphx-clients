mod assemble;

fn main() {
    let root =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("generated");
    println!("cargo:rerun-if-changed=generated");
    let registry = assemble::assemble(&root).expect("assemble generated apply registry");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("registry.json"), registry).expect("write assembled apply registry");
}

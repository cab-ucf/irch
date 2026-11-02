//! The app's page (app/ui), in the binary for `--web`.

use std::path::Path;

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).expect("app/ui").flatten() {
        let p = e.path();
        if p.is_dir() { walk(&p, out) } else { out.push(p) }
    }
}

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../app/ui");
    let ui = ui.canonicalize().expect("app/ui");
    println!("cargo:rerun-if-changed={}", ui.display());
    let mut all = vec![];
    walk(&ui, &mut all);
    let mut rs = String::from("&[");
    for p in all {
        let rel = p.strip_prefix(&ui).expect("inside").to_string_lossy();
        let rel = rel.replace('\\', "/");
        rs += &format!("({rel:?}, include_bytes!({:?})),", p.display());
    }
    rs += "]";
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out).join("ui.rs"), rs).expect("ui.rs");
}

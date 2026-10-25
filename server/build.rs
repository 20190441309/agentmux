//! `rust-embed` needs `../web/dist` to exist at compile time. When the
//! frontend has not been built (`npm run build` in `web/`), leave a
//! placeholder page so the server still compiles and explains itself.
use std::{fs, path::Path};

fn main() {
    let dist = Path::new("../web/dist");
    if !dist.join("index.html").exists() {
        let _ = fs::create_dir_all(dist);
        let _ = fs::write(
            dist.join("index.html"),
            "<!doctype html><meta charset=utf-8><title>agentmux</title>\
             <p>The web UI has not been built. Run <code>npm ci &amp;&amp; npm run build</code> in <code>web/</code> and rebuild the server.</p>",
        );
    }
    println!("cargo:rerun-if-changed=../web/dist");
}

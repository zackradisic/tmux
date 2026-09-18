//! plugin-index: write `index.toml` for a directory of built plugins.
//!
//! The release job runs it over the bundle directory. Each `<name>.wasm`
//! becomes one `[plugins.<name>]` table with its blake3 hash, its size,
//! its sidecar (`<name>.toml`, when present) and its crate version. The
//! `abi` field is the host ABI the plugins were built against, so a
//! server can refuse an index its tmux cannot load.
//!
//!     plugin-index --dir dist/plugins --tag v3.8-wasm.4 \
//!         --version agents=0.1.0 --version notify_toast=0.1.0
//!
//! The version of a plugin comes from `--version <stem>=<x.y.z>`, where
//! `<stem>` is the wasm file name without `.wasm` (the crate name with
//! `-` replaced by `_`). A plugin without one gets version "0.0.0" and a
//! warning. `--out` names the output (default `<dir>/index.toml`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::exit;

use serde::Serialize;

#[derive(Serialize)]
struct Index {
    abi: i32,
    tag: String,
    plugins: BTreeMap<String, Entry>,
}

#[derive(Serialize)]
struct Entry {
    file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sidecar: Option<String>,
    version: String,
    blake3: String,
    size: u64,
}

fn usage() -> ! {
    eprintln!(
        "usage: plugin-index --dir <dir> --tag <tag> [--version <stem>=<x.y.z>]... [--out <file>]"
    );
    exit(2)
}

fn main() {
    let mut dir: Option<PathBuf> = None;
    let mut tag: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--tag" => tag = Some(args.next().unwrap_or_else(|| usage())),
            "--out" => out = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--version" => {
                let v = args.next().unwrap_or_else(|| usage());
                let Some((stem, ver)) = v.split_once('=') else { usage() };
                versions.insert(stem.to_string(), ver.to_string());
            }
            _ => usage(),
        }
    }
    let (Some(dir), Some(tag)) = (dir, tag) else { usage() };
    let out = out.unwrap_or_else(|| dir.join("index.toml"));

    match build(&dir, tag, &versions) {
        Ok(index) => {
            let text = toml::to_string(&index).expect("serialize index");
            if let Err(e) = std::fs::write(&out, text) {
                eprintln!("plugin-index: {}: {e}", out.display());
                exit(1);
            }
            println!("{}: {} plugins", out.display(), index.plugins.len());
        }
        Err(e) => {
            eprintln!("plugin-index: {e}");
            exit(1);
        }
    }
}

fn build(dir: &Path, tag: String, versions: &BTreeMap<String, String>) -> Result<Index, String> {
    let mut plugins = BTreeMap::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wasm"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{}: no .wasm files", dir.display()));
    }
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("{}: bad file name", path.display()))?
            .to_string();
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file = format!("{stem}.wasm");
        let sidecar_name = format!("{stem}.toml");
        let sidecar = dir.join(&sidecar_name).is_file().then_some(sidecar_name);
        let version = match versions.get(&stem) {
            Some(v) => v.clone(),
            None => {
                eprintln!("plugin-index: warning: no --version for {stem}, using 0.0.0");
                "0.0.0".to_string()
            }
        };
        plugins.insert(
            stem,
            Entry {
                file,
                sidecar,
                version,
                blake3: blake3::hash(&bytes).to_hex().to_string(),
                size: bytes.len() as u64,
            },
        );
    }
    Ok(Index { abi: tmux_plugin_abi::ABI_VERSION, tag, plugins })
}

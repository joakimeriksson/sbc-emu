//! What snapshots leave out, all of it, and why. `snap_struct!` makes every field be saved or
//! skipped; a field that is skipped (or `name: _` in an implementation by hand) must be here.
use std::collections::{BTreeMap, BTreeSet};

const APPROVED: &[(&str, &str, &str)] = &[
    ("emu-core", "SdCard", "trace"),                        // debugging output
    ("peripherals", "Pl330", "log"),                        // debugging output
    ("rk3588", "Clock", "name"),                            // a constant of the board
    ("rk3588", "Gic", "ready"),                             // a cache, dropped by `after`
    ("rk3588", "I2sTdm", "capture out"),                    // --audio-out, what was recorded
    ("rk3588", "Machine", "breakpoints dtb mt multi"),      // the debugger's; the boot's; threads and the choice to use them
    ("rk3588", "Pe", "cached"),                             // a cache, dropped by the Gic's `after`
    ("rk3588", "Xhci", "trace"),                            // debugging output
    ("sbc-net", "VirtualNet", "log nat"),                   // debugging output; host sockets
];

fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() { sources(&p, out); } else if p.extension().is_some_and(|x| x == "rs") { out.push(p); }
    }
}

/// The text between the brackets that open at `s[0]`.
fn group(s: &str) -> &str {
    let mut depth = 0;
    for (k, c) in s.char_indices() {
        match c {
            '(' | '{' => depth += 1,
            ')' | '}' => { depth -= 1; if depth == 0 { return &s[1..k]; } }
            _ => {}
        }
    }
    s
}

fn fields(list: &str) -> impl Iterator<Item = String> + '_ {
    list.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty())
}

#[test]
fn every_skipped_field_is_approved() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut found: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for e in std::fs::read_dir(root).unwrap().flatten() {
        let src = e.path().join("src");
        let krate = e.file_name().to_string_lossy().into_owned();
        // `target` and the like have no src; this workspace's crates do
        if !src.is_dir() || !e.path().join("Cargo.toml").is_file() { continue; }
        let mut files = Vec::new();
        sources(&src, &mut files);
        for f in files {
            if f.ends_with("emu-core/src/snap.rs") { continue; } // the macros themselves
            let text = std::fs::read_to_string(&f).unwrap();
            let text: String = text.split("#[cfg(test)]\nmod tests").next().unwrap().lines().map(|l| l.split("//").next().unwrap()).collect::<Vec<_>>().join("\n");
            for (at, _) in text.match_indices("snap_struct!(") {
                let body = group(&text[at + "snap_struct!".len()..]);
                let name = body.split(|c: char| !c.is_alphanumeric() && c != '_').find(|w| !w.is_empty()).unwrap().to_string();
                if let Some(k) = body.find("skip {") {
                    found.entry((krate.clone(), name)).or_default().extend(fields(group(&body[k + "skip ".len()..])));
                }
            }
            // by hand: `let Self { .. field: _ .. } = self`, in the impl block before it
            for (at, _) in text.match_indices("let Self {") {
                let body = group(&text[at + "let Self ".len()..]);
                assert!(!body.contains(".."), "{}: `..` hides fields", f.display());
                let header = text[..at].rfind("\nimpl").map_or("", |k| text[k + 1..].lines().next().unwrap());
                let name = header.trim_end_matches(" {").rsplit([' ', ':']).next().unwrap().to_string();
                for part in fields(body) {
                    // (an attribute may come first)
                    let part = part.rsplit(']').next().unwrap().trim().to_string();
                    if let Some(x) = part.strip_suffix(": _") { found.entry((krate.clone(), name.clone())).or_default().insert(x.to_string()); }
                }
            }
        }
    }
    let approved: BTreeMap<(String, String), BTreeSet<String>> = APPROVED.iter().map(|(k, t, f)| ((k.to_string(), t.to_string()), f.split(' ').map(str::to_string).collect())).collect();
    for (k, f) in &found {
        let a = approved.get(k).cloned().unwrap_or_default();
        assert!(f.is_subset(&a), "{k:?} skips {:?}: save it, or say in APPROVED why not", f.difference(&a).collect::<Vec<_>>());
    }
    for (k, a) in &approved {
        let f = found.get(k).cloned().unwrap_or_default();
        assert!(a.is_subset(&f), "{k:?}: {:?} is saved after all: take it out of APPROVED", a.difference(&f).collect::<Vec<_>>());
    }
}

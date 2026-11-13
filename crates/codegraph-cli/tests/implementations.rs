//! `codegraphV implementations <name>` (upstream 08b187aa, #2833): the issue's Cargo workspace
//! (a trait, a real implementor, a mock in a `#[cfg(test)]` module, a crate that only depends on
//! the trait) plus a TypeScript chain for `--depth`.
use serde_json::Value;
use std::{path::Path, process::Command, sync::OnceLock};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn project() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", "[workspace]\nmembers = [\"api\", \"store-real\", \"app\"]\n");
        write(root, "api/Cargo.toml", "[package]\nname = \"api\"\nversion = \"0.1.0\"\n");
        write(root, "api/src/lib.rs", "pub trait Store {\n    fn put(&self, key: &str);\n}\n");
        write(root, "store-real/Cargo.toml", "[package]\nname = \"store-real\"\nversion = \"0.1.0\"\n[dependencies]\napi = { path = \"../api\" }\n");
        write(
            root,
            "store-real/src/lib.rs",
            &[
                "use api::Store;",
                "pub struct RealStore { pub n: u8 }",
                "impl Store for RealStore { fn put(&self, key: &str) { let _ = key; } }",
                "",
                "#[cfg(test)]",
                "mod tests {",
                "    use api::Store;",
                "    struct MockStore { n: u8 }",
                "    impl Store for MockStore { fn put(&self, _key: &str) {} }",
                "    #[test]",
                "    fn mock_puts() { MockStore { n: 0 }.put(\"k\"); }",
                "}",
                "",
            ]
            .join("\n"),
        );
        write(root, "app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\napi = { path = \"../api\" }\nstore-real = { path = \"../store-real\" }\n");
        write(root, "app/src/lib.rs", "use api::Store;\npub fn save(store: &dyn Store) { store.put(\"a\"); }\npub fn build() -> store_real::RealStore { store_real::RealStore { n: 0 } }\n");
        write(
            root,
            "ts/shapes.ts",
            "export interface Shape { area(): number; }\nexport class Square implements Shape { area() { return 1; } }\nexport class Cube extends Square { volume() { return 1; } }\nexport function measure(s: Shape) { return s.area(); }\n",
        );
        write(root, "ts/__tests__/mock-shape.ts", "import { Shape } from '../shapes';\nexport class MockShape implements Shape { area() { return 0; } }\n");
        codegraph_core::CodeGraph::init_and_index(&root.to_string_lossy(), None).unwrap().close();
        dir
    })
    .path()
}

fn run(args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraphV"))
        .arg("implementations")
        .args(args)
        .arg("--path")
        .arg(project())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("LOCALAPPDATA", home.path())
        .env("CODEGRAPHV_NO_UPDATE_CHECK", "1")
        .env("CODEGRAPHV_NO_DAEMON", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn json(args: &[&str]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    serde_json::from_str(&run(&all)).unwrap()
}

#[test]
fn lists_a_traits_implementors_and_marks_the_cfg_test_mock() {
    let out = json(&["Store"]);
    assert_eq!(out["ambiguous"], false);
    let defs = out["definitions"].as_array().unwrap();
    assert_eq!(defs.len(), 1, "{out}");
    let def = &defs[0];
    assert_eq!(def["definition"]["kind"], "trait");
    assert_eq!(def["directSubtypes"], 2);
    let mut rows: Vec<_> = def["implementations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap().to_string(), r["relation"].as_str().unwrap().to_string(), r["test"].as_bool().unwrap(), format!("{}:{}", r["filePath"].as_str().unwrap(), r["startLine"])))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("MockStore".into(), "implements".into(), true, "store-real/src/lib.rs:8".into()),
            ("RealStore".into(), "implements".into(), false, "store-real/src/lib.rs:2".into()),
        ]
    );
}

#[test]
fn prints_one_line_per_implementor_with_a_test_tag() {
    let text = run(&["Store"]);
    assert!(text.contains("Implementations of trait Store (rust)"), "{text}");
    let real = regex::Regex::new(r"struct\s+RealStore\s+implements\s+store-real/src/lib\.rs:2\n").unwrap();
    let mock = regex::Regex::new(r"struct\s+MockStore\s+implements\s+store-real/src/lib\.rs:8\s+\[test\]\n").unwrap();
    assert!(real.is_match(&text) && mock.is_match(&text), "{text}");
    assert!(!text.contains("save"), "{text}");
}

#[test]
fn lists_direct_implementors_and_follows_subclasses_with_depth() {
    let direct = json(&["Shape"]);
    let rows = |v: &Value| -> Vec<(String, u64, String, Option<String>)> {
        v["definitions"][0]["implementations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["name"].as_str().unwrap().into(), r["depth"].as_u64().unwrap(), r["relation"].as_str().unwrap().into(), r["via"].as_str().map(str::to_string)))
            .collect()
    };
    let mut first = rows(&direct);
    first.sort();
    assert_eq!(first.iter().map(|r| (r.0.as_str(), r.1)).collect::<Vec<_>>(), vec![("MockShape", 1), ("Square", 1)]);
    let mock = direct["definitions"][0]["implementations"].as_array().unwrap().iter().find(|r| r["name"] == "MockShape").unwrap();
    assert_eq!(mock["test"], true);

    let deep = rows(&json(&["Shape", "--depth", "2"]));
    let mut level_one = deep[..2].to_vec();
    level_one.sort();
    assert_eq!(level_one, vec![("MockShape".into(), 1, "implements".into(), None), ("Square".into(), 1, "implements".into(), None)]);
    assert_eq!(deep[2..].to_vec(), vec![("Cube".into(), 2, "extends".into(), Some("Square".into()))]);
    let text = run(&["Shape", "-d", "2"]);
    assert!(regex::Regex::new(r"\n {4}class\s+Cube\s+extends Square\s+ts/shapes\.ts:3\n").unwrap().is_match(&text), "{text}");
}

#[test]
fn says_so_when_the_symbol_is_not_a_type_or_unknown() {
    assert!(run(&["measure"]).contains("not a type something can implement or extend"));
    assert!(run(&["NoSuchType"]).contains("not found"));
}

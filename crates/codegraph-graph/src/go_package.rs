//! The declared Go package is authoritative when a query names a root or
//! versioned package whose name is absent from the file path.
use codegraph_jsre::re;
use codegraph_types::paths::{is_within_dir, lexical_path_within_root, resolve_path};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

pub type GoPackageOf<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn go_package_clause(source: &str) -> Option<String> {
    let code = re!(r"/\*[\s\S]*?\*/", "g").replace_all_str(source, " ");
    let code = re!(r"//[^\n]*", "g").replace_all_str(&code, "");
    re!(r"^\s*package\s+([A-Za-z_]\w*)")
        .captures(&code)
        .and_then(|c| c.text(1).map(str::to_owned))
}

/// Cache is scoped to a query/session, matching the original reader closure.
pub struct GoPackageReader {
    root: String,
    cache: RefCell<HashMap<String, Option<String>>>,
}

impl GoPackageReader {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: resolve_path(root).to_string_lossy().into_owned(),
            cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn package_of(&self, file: &str) -> Option<String> {
        if let Some(hit) = self.cache.borrow().get(file) {
            return hit.clone();
        }
        let result = self.read(file);
        self.cache
            .borrow_mut()
            .insert(file.to_owned(), result.clone());
        result
    }

    fn read(&self, file: &str) -> Option<String> {
        let lexical = lexical_path_within_root(&self.root, file)?;
        let root = std::fs::canonicalize(&self.root).ok()?;
        let real = std::fs::canonicalize(&lexical).ok()?;
        if !is_within_dir(&real.to_string_lossy(), &root.to_string_lossy()) {
            return None;
        }
        let mut head = Vec::new();
        std::fs::File::open(real)
            .ok()?
            .take(8192)
            .read_to_end(&mut head)
            .ok()?;
        go_package_clause(&String::from_utf8_lossy(&head))
    }
}

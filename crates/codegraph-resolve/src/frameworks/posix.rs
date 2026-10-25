//! `path.posix` for the framework resolvers that join and normalize project-relative paths.

pub use crate::name_matcher::line_scope::posix_dirname as dirname;

/// `path.posix.normalize`: collapse `.`/`..`/repeated slashes, keep a leading `/` and a trailing `/`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let mut result = out.join("/");
    if result.is_empty() && !absolute {
        return if trailing { "./".to_string() } else { ".".to_string() };
    }
    if !result.is_empty() && trailing {
        result.push('/');
    }
    if absolute {
        format!("/{result}")
    } else {
        result
    }
}

/// `path.posix.join`: the non-empty parts joined with `/` and normalized (`.` when nothing is left).
pub fn join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    normalize(&joined.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_node_posix() {
        assert_eq!(normalize("a/./b/../c"), "a/c");
        assert_eq!(normalize("../a"), "../a");
        assert_eq!(normalize(""), ".");
        assert_eq!(join(&["routes", "../app/x.php"]), "app/x.php");
        assert_eq!(join(&[".", "x"]), "x");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(dirname("routes/api.php"), "routes");
    }
}

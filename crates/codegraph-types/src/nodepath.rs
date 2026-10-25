//! Node's `path` module, as the TypeScript engine calls it: `path.posix.*` always
//! uses `/`; the unqualified `path.*` is `win32` on Windows and `posix` elsewhere.
//!
//! Only the string manipulation is ported — nothing here touches the filesystem
//! except [`resolve`] reading the current directory, as Node's does.

pub mod posix {
    /// `path.posix.normalize`.
    pub fn normalize(p: &str) -> String {
        if p.is_empty() {
            return ".".to_string();
        }
        let absolute = p.starts_with('/');
        let trailing = p.ends_with('/');
        let mut out: Vec<&str> = Vec::new();
        for seg in p.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    if out.last().is_some_and(|l| *l != "..") {
                        out.pop();
                    } else if !absolute {
                        out.push("..");
                    }
                }
                s => out.push(s),
            }
        }
        let mut joined = out.join("/");
        if joined.is_empty() {
            if absolute {
                return "/".to_string();
            }
            return if trailing { "./".to_string() } else { ".".to_string() };
        }
        if trailing {
            joined.push('/');
        }
        if absolute {
            format!("/{joined}")
        } else {
            joined
        }
    }

    /// `path.posix.join`.
    pub fn join(parts: &[&str]) -> String {
        let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
        if joined.is_empty() {
            return ".".to_string();
        }
        normalize(&joined.join("/"))
    }

    pub fn is_absolute(p: &str) -> bool {
        p.starts_with('/')
    }

    /// `path.posix.dirname`.
    pub fn dirname(p: &str) -> String {
        if p.is_empty() {
            return ".".to_string();
        }
        let b = p.as_bytes();
        let has_root = b[0] == b'/';
        let mut end: Option<usize> = None;
        let mut matched_slash = true;
        let mut i = b.len() - 1;
        while i >= 1 {
            if b[i] == b'/' {
                if !matched_slash {
                    end = Some(i);
                    break;
                }
            } else {
                matched_slash = false;
            }
            i -= 1;
        }
        match end {
            None => {
                if has_root {
                    "/".to_string()
                } else {
                    ".".to_string()
                }
            }
            Some(1) if has_root => "//".to_string(),
            Some(e) => p[..e].to_string(),
        }
    }

    /// `path.posix.basename(p)`.
    pub fn basename(p: &str) -> String {
        let trimmed = p.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(i) => trimmed[i + 1..].to_string(),
            None => trimmed.to_string(),
        }
    }

    /// `path.posix.basename(p, ext)`.
    pub fn basename_ext(p: &str, ext: &str) -> String {
        let base = basename(p);
        if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) {
            base[..base.len() - ext.len()].to_string()
        } else if base == ext {
            base
        } else {
            base
        }
    }

    /// `path.posix.extname`.
    pub fn extname(p: &str) -> String {
        let base = basename(p);
        match base.rfind('.') {
            Some(0) | None => String::new(),
            Some(i) => {
                if base == ".." {
                    String::new()
                } else {
                    base[i..].to_string()
                }
            }
        }
    }

    /// `path.posix.resolve`, against `cwd` for relative input.
    pub fn resolve_from(cwd: &str, parts: &[&str]) -> String {
        let mut resolved = String::new();
        let mut absolute = false;
        for p in parts.iter().rev() {
            if p.is_empty() {
                continue;
            }
            resolved = if resolved.is_empty() { (*p).to_string() } else { format!("{p}/{resolved}") };
            if p.starts_with('/') {
                absolute = true;
                break;
            }
        }
        if !absolute {
            resolved = if resolved.is_empty() { cwd.to_string() } else { format!("{cwd}/{resolved}") };
        }
        let norm = normalize(&resolved);
        if norm.len() > 1 && norm.ends_with('/') {
            norm[..norm.len() - 1].to_string()
        } else {
            norm
        }
    }

    /// `path.posix.relative`.
    pub fn relative(from: &str, to: &str) -> String {
        let f = resolve_from("/", &[from]);
        let t = resolve_from("/", &[to]);
        if f == t {
            return String::new();
        }
        let fs: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
        let ts: Vec<&str> = t.split('/').filter(|s| !s.is_empty()).collect();
        let mut i = 0;
        while i < fs.len() && i < ts.len() && fs[i] == ts[i] {
            i += 1;
        }
        let mut out: Vec<&str> = vec![".."; fs.len() - i];
        out.extend(&ts[i..]);
        out.join("/")
    }
}

pub mod win32 {
    fn is_sep(c: char) -> bool {
        c == '/' || c == '\\'
    }

    /// Split an absolute/rooted prefix off: `(root, rest, absolute)`. Roots are
    /// `C:\`, `C:` (drive-relative), `\\server\share\` (UNC) or `\`.
    fn split_root(p: &str) -> (String, &str, bool) {
        let c: Vec<char> = p.chars().take(3).collect();
        let b = p.as_bytes();
        // UNC: \\server\share
        if c.len() >= 2 && is_sep(c[0]) && is_sep(c[1]) && !(c.len() >= 3 && is_sep(c[2])) {
            let rest = &p[2..];
            let mut it = rest.splitn(3, is_sep);
            let server = it.next().unwrap_or("");
            let share = it.next().unwrap_or("");
            if !server.is_empty() && !share.is_empty() {
                let consumed = 2 + server.len() + 1 + share.len();
                let tail = &p[consumed.min(p.len())..];
                return (format!("\\\\{server}\\{share}\\"), tail.trim_start_matches(is_sep), true);
            }
        }
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            if b.len() >= 3 && (b[2] == b'/' || b[2] == b'\\') {
                return (format!("{}:\\", b[0] as char), p[3..].trim_start_matches(is_sep), true);
            }
            return (format!("{}:", b[0] as char), &p[2..], false);
        }
        if !b.is_empty() && (b[0] == b'/' || b[0] == b'\\') {
            return ("\\".to_string(), p.trim_start_matches(is_sep), true);
        }
        (String::new(), p, false)
    }

    fn normalize_segments(rest: &str, absolute: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for seg in rest.split(is_sep) {
            match seg {
                "" | "." => {}
                ".." => {
                    if out.last().is_some_and(|l| l != "..") {
                        out.pop();
                    } else if !absolute {
                        out.push("..".to_string());
                    }
                }
                s => out.push(s.to_string()),
            }
        }
        out
    }

    /// `path.win32.normalize`.
    pub fn normalize(p: &str) -> String {
        if p.is_empty() {
            return ".".to_string();
        }
        let (root, rest, absolute) = split_root(p);
        let trailing = p.ends_with(is_sep);
        let segs = normalize_segments(rest, absolute);
        let mut tail = segs.join("\\");
        if tail.is_empty() && root.is_empty() {
            tail = ".".to_string();
        }
        if !tail.is_empty() && trailing && tail != "." {
            tail.push('\\');
        }
        if tail == "." && trailing && root.is_empty() {
            tail = ".\\".to_string();
        }
        format!("{root}{tail}")
    }

    /// `path.win32.join`.
    pub fn join(parts: &[&str]) -> String {
        let non_empty: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
        if non_empty.is_empty() {
            return ".".to_string();
        }
        let mut joined = non_empty[0].to_string();
        for p in &non_empty[1..] {
            joined.push('\\');
            joined.push_str(p);
        }
        // A leading UNC-looking `\\` from joining two parts must not become UNC.
        normalize(&joined)
    }

    pub fn is_absolute(p: &str) -> bool {
        let (_, _, absolute) = split_root(p);
        absolute
    }

    /// `path.win32.dirname`.
    pub fn dirname(p: &str) -> String {
        if p.is_empty() {
            return ".".to_string();
        }
        let (root, rest, _) = split_root(p);
        let trimmed = rest.trim_end_matches(is_sep);
        match trimmed.rfind(is_sep) {
            Some(i) => {
                let head = trimmed[..i].trim_end_matches(is_sep);
                format!("{root}{head}")
            }
            None => {
                if root.is_empty() {
                    ".".to_string()
                } else if trimmed.is_empty() {
                    root
                } else {
                    root
                }
            }
        }
    }

    pub fn basename(p: &str) -> String {
        let (_, rest, _) = split_root(p);
        let trimmed = rest.trim_end_matches(is_sep);
        match trimmed.rfind(is_sep) {
            Some(i) => trimmed[i + 1..].to_string(),
            None => trimmed.to_string(),
        }
    }

    pub fn basename_ext(p: &str, ext: &str) -> String {
        let base = basename(p);
        if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) {
            base[..base.len() - ext.len()].to_string()
        } else {
            base
        }
    }

    pub fn extname(p: &str) -> String {
        let base = basename(p);
        match base.rfind('.') {
            Some(0) | None => String::new(),
            Some(i) => {
                if base == ".." {
                    String::new()
                } else {
                    base[i..].to_string()
                }
            }
        }
    }

    fn device_of(root: &str) -> String {
        if root.starts_with("\\\\") {
            root.trim_end_matches('\\').to_string()
        } else if root.len() >= 2 && root.as_bytes()[1] == b':' {
            root[..2].to_string()
        } else {
            String::new()
        }
    }

    /// `path.win32.resolve`, against `cwd` for anything not rooted at a drive.
    pub fn resolve_from(cwd: &str, parts: &[&str]) -> String {
        let mut device = String::new();
        let mut tail = String::new();
        let mut absolute = false;
        let mut items: Vec<&str> = parts.iter().rev().copied().filter(|p| !p.is_empty()).collect();
        items.push(cwd);
        for path in items {
            let (root, rest, abs) = split_root(path);
            let dev = device_of(&root);
            if !dev.is_empty() && !device.is_empty() && dev.to_lowercase() != device.to_lowercase() {
                continue;
            }
            if device.is_empty() && !dev.is_empty() {
                device = dev;
            }
            if !absolute {
                tail = if tail.is_empty() { rest.to_string() } else { format!("{rest}\\{tail}") };
                absolute = abs;
            }
            if absolute && !device.is_empty() {
                break;
            }
        }
        let segs = normalize_segments(&tail, absolute);
        let joined = segs.join("\\");
        if absolute {
            format!("{device}\\{joined}")
        } else if !device.is_empty() || !joined.is_empty() {
            format!("{device}{joined}")
        } else {
            ".".to_string()
        }
    }

    /// `path.win32.relative`.
    pub fn relative(from: &str, to: &str) -> String {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "C:\\".to_string());
        let f = resolve_from(&cwd, &[from]);
        let t = resolve_from(&cwd, &[to]);
        if f == t || f.to_lowercase() == t.to_lowercase() {
            return String::new();
        }
        let (froot, frest, _) = split_root(&f);
        let (troot, trest, _) = split_root(&t);
        if froot.to_lowercase() != troot.to_lowercase() {
            return t;
        }
        let fs: Vec<&str> = frest.split(is_sep).filter(|s| !s.is_empty()).collect();
        let ts: Vec<&str> = trest.split(is_sep).filter(|s| !s.is_empty()).collect();
        let mut i = 0;
        while i < fs.len() && i < ts.len() && fs[i].to_lowercase() == ts[i].to_lowercase() {
            i += 1;
        }
        let mut out: Vec<&str> = vec![".."; fs.len() - i];
        out.extend(&ts[i..]);
        out.join("\\")
    }
}

#[cfg(windows)]
pub use win32 as native;
#[cfg(not(windows))]
pub use posix as native;

/// The platform separator (`path.sep`).
pub const SEP: char = if cfg!(windows) { '\\' } else { '/' };

fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| if cfg!(windows) { "C:\\".into() } else { "/".into() })
}

/// `path.resolve(...parts)` on the host.
pub fn resolve(parts: &[&str]) -> String {
    native::resolve_from(&cwd(), parts)
}

/// `path.join(...parts)` on the host.
pub fn join(parts: &[&str]) -> String {
    native::join(parts)
}

/// `path.dirname(p)` on the host.
pub fn dirname(p: &str) -> String {
    native::dirname(p)
}

/// `path.basename(p)` on the host.
pub fn basename(p: &str) -> String {
    native::basename(p)
}

/// `path.extname(p)` on the host.
pub fn extname(p: &str) -> String {
    native::extname(p)
}

/// `path.relative(from, to)` on the host.
pub fn relative(from: &str, to: &str) -> String {
    native::relative(from, to)
}

/// `path.isAbsolute(p)` on the host.
pub fn is_absolute(p: &str) -> bool {
    native::is_absolute(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_basics() {
        assert_eq!(posix::normalize("a/./b/../c//d/"), "a/c/d/");
        assert_eq!(posix::normalize("../x"), "../x");
        assert_eq!(posix::normalize("/../x"), "/x");
        assert_eq!(posix::join(&["a", "", "../b", "c"]), "b/c");
        assert_eq!(posix::dirname("/a/b/c"), "/a/b");
        assert_eq!(posix::dirname("a"), ".");
        assert_eq!(posix::dirname("/a"), "/");
        assert_eq!(posix::dirname("a/b/"), "a");
        assert_eq!(posix::basename("a/b/c.ts"), "c.ts");
        assert_eq!(posix::basename_ext("a/b/c.ts", ".ts"), "c");
        assert_eq!(posix::extname("a/b.c/d"), "");
        assert_eq!(posix::extname("a/.bashrc"), "");
        assert_eq!(posix::extname("a/b.tar.gz"), ".gz");
        assert_eq!(posix::relative("/a/b", "/a/c/d"), "../c/d");
        assert_eq!(posix::relative("/a", "/a"), "");
        assert_eq!(posix::resolve_from("/w", &["x", "../y"]), "/w/y");
    }

    #[test]
    fn win32_basics() {
        assert_eq!(win32::normalize("C:/a/./b/../c"), "C:\\a\\c");
        assert_eq!(win32::join(&["C:\\proj", "src", "a.ts"]), "C:\\proj\\src\\a.ts");
        assert_eq!(win32::join(&["a", "..", "b"]), "b");
        assert_eq!(win32::dirname("C:\\a\\b\\c.ts"), "C:\\a\\b");
        assert_eq!(win32::dirname("C:\\a"), "C:\\");
        assert_eq!(win32::dirname("C:\\"), "C:\\");
        assert_eq!(win32::dirname("a\\b"), "a");
        assert_eq!(win32::basename("C:\\a\\b.ts"), "b.ts");
        assert_eq!(win32::extname("C:\\a\\b.ts"), ".ts");
        assert_eq!(win32::relative("C:\\proj", "C:\\proj\\src\\a.ts"), "src\\a.ts");
        assert_eq!(win32::relative("C:\\proj\\a", "C:\\proj\\b"), "..\\b");
        assert_eq!(win32::relative("C:\\Proj", "c:\\proj\\x"), "x");
        assert_eq!(win32::resolve_from("C:\\w", &["x", "..\\y"]), "C:\\w\\y");
        assert_eq!(win32::resolve_from("C:\\w", &["D:\\z", "q"]), "D:\\z\\q");
        assert!(win32::is_absolute("C:/x"));
        assert!(!win32::is_absolute("x/y"));
    }
}

//! The package that owns a file: the nearest manifest above it that names one (upstream
//! 17d5426d, #1871).
//!
//! `package.json` and `composer.json` are not source, so nothing about them is in the index and
//! the answer is read from disk when asked, which also keeps it current without a re-index. The
//! nearest naming manifest is the whole rule: a workspace member's manifest sits closer to its
//! files than the root's, so a monorepo needs no configuration, and a manifest that names nothing
//! (`{"type":"module"}` in a build folder, a private root) doesn't stop the walk.
//!
//! `Cargo.toml` and `pyproject.toml` are left out on purpose, as upstream does: a `name =` search
//! without a TOML parser would group files under a dependency's name.
use codegraph_graph::type_hierarchy::locale_compare;
use codegraph_types::paths::{validate_path_within_root, ValidateOptions};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

/// The manifests read, in the order that settles a directory holding both.
const PACKAGE_MANIFESTS: [&str; 2] = ["package.json", "composer.json"];

#[derive(Debug, PartialEq, Eq)]
pub struct OwningPackage {
    /// The manifest's `name`, trimmed.
    pub name: String,
    /// Project-relative POSIX directory of the manifest; `.` is the project root.
    pub dir: String,
    /// Which manifest named it.
    pub manifest: &'static str,
}

pub struct PackageGroup<'a, F> {
    /// `None` for the files no manifest above them names a package for.
    pub owner: Option<Rc<OwningPackage>>,
    pub files: Vec<&'a F>,
}

fn posix_dirname(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((dir, _)) => dir,
        None => ".",
    }
}

fn posix_join(dir: &str, name: &str) -> String {
    if dir == "." { name.to_string() } else { format!("{dir}/{name}") }
}

/// The `name` a manifest declares. `None` when it is absent, outside the project, over the source
/// size limit, not valid JSON, or names nothing: a bad manifest costs a listing one package name,
/// never the listing.
fn manifest_name(project_root: &str, manifest_path: &str) -> Option<String> {
    // Most directories hold no manifest, so this is the common exit, ahead of the containment
    // check, which resolves symlinks.
    if !Path::new(project_root).join(manifest_path).exists() {
        return None;
    }
    let file = validate_path_within_root(project_root, manifest_path, ValidateOptions::default())?;
    let bytes = codegraph_core::file_limits::read_bounded_source(Path::new(&file)).ok()?.bytes?;
    let text = String::from_utf8_lossy(&bytes);
    let manifest: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let name = manifest.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// A `project-relative path -> owner` lookup for one listing. Each directory is resolved once and
/// shares its parent's answer, so every file of a package gets the same `OwningPackage`.
pub struct OwningPackageLookup {
    root: String,
    by_dir: HashMap<String, Option<Rc<OwningPackage>>>,
}

impl OwningPackageLookup {
    pub fn new(project_root: &str) -> Self {
        Self { root: project_root.to_string(), by_dir: HashMap::new() }
    }

    pub fn owner_of(&mut self, file_path: &str) -> Option<Rc<OwningPackage>> {
        self.owner_of_dir(posix_dirname(file_path))
    }

    fn owner_of_dir(&mut self, dir: &str) -> Option<Rc<OwningPackage>> {
        if let Some(known) = self.by_dir.get(dir) {
            return known.clone();
        }
        let mut owner = PACKAGE_MANIFESTS.iter().find_map(|manifest| {
            manifest_name(&self.root, &posix_join(dir, manifest))
                .map(|name| Rc::new(OwningPackage { name, dir: dir.to_string(), manifest }))
        });
        if owner.is_none() {
            let parent = posix_dirname(dir);
            if parent != dir {
                owner = self.owner_of_dir(parent);
            }
        }
        self.by_dir.insert(dir.to_string(), owner.clone());
        owner
    }
}

/// `files` grouped by owning package: packages by name, then directory when two share a name,
/// files by path. The unowned group comes last and is never folded into a package. `project_root`
/// is the project's root even under a path filter: a file's manifest can sit above the filter.
pub fn group_files_by_package<'a, F>(
    project_root: &str,
    files: &'a [F],
    path_of: impl Fn(&F) -> &str,
) -> Vec<PackageGroup<'a, F>> {
    let mut lookup = OwningPackageLookup::new(project_root);
    let mut groups: Vec<PackageGroup<'a, F>> = Vec::new();
    let mut index: HashMap<Option<String>, usize> = HashMap::new();
    for file in files {
        let owner = lookup.owner_of(path_of(file));
        let key = owner.as_ref().map(|o| o.dir.clone());
        let slot = *index.entry(key).or_insert_with(|| {
            groups.push(PackageGroup { owner, files: Vec::new() });
            groups.len() - 1
        });
        groups[slot].files.push(file);
    }
    for group in &mut groups {
        group.files.sort_by(|a, b| locale_compare(path_of(a), path_of(b)));
    }
    groups.sort_by(|a, b| match (&a.owner, &b.owner) {
        (Some(x), Some(y)) => locale_compare(&x.name, &y.name).then_with(|| locale_compare(&x.dir, &y.dir)),
        (x, y) => x.is_none().cmp(&y.is_none()),
    });
    groups
}

fn count_of(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// The heading of a by-package listing.
pub fn package_listing_title<F>(groups: &[PackageGroup<'_, F>]) -> String {
    let files: usize = groups.iter().map(|g| g.files.len()).sum();
    let packages = groups.iter().filter(|g| g.owner.is_some()).count();
    format!("Files by Package ({}, {})", count_of(files, "file"), count_of(packages, "package"))
}

/// The heading of one package's section.
pub fn package_group_title<F>(group: &PackageGroup<'_, F>) -> String {
    let size = count_of(group.files.len(), "file");
    match &group.owner {
        None => format!("no owning package ({size})"),
        Some(owner) if owner.dir == "." => format!("{} ({size})", owner.name),
        Some(owner) => format!("{} in {} ({size})", owner.name, owner.dir),
    }
}

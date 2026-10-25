//! Terraform framework resolver. Port of `src/resolution/frameworks/terraform.ts`.
//!
//! Terraform's scoping rule is narrow and directory-shaped: `var.X`, `local.X`, `module.M`, and
//! resource/data references resolve ONLY inside the same module directory as the reference site.
//! The generic name matcher resolves by qualified name alone, so a reference to `var.project_id`
//! from `modules/net-vpc/main.tf` could bind to a `variable "project_id"` declared in an unrelated
//! module — a wrong cross-module edge that poisons impact analysis. This resolver enforces the real
//! semantics:
//!
//!   1. Same directory as the reference site → resolve (highest confidence).
//!   2. `.tfvars` files additionally walk UP to the nearest ancestor directory declaring the
//!      variable (`terraform apply -var-file=envs/prod.tfvars` sets ROOT module variables from a
//!      subdirectory).
//!   3. Otherwise: no edge. Terraform cannot reference across sibling module directories, so a
//!      non-local candidate is never a correct target.
//!
//! It also bridges the module boundary through `:`-scoped references that only this resolver
//! understands (see the extractor's emitModuleWiring):
//!
//!   - `module.M:file`       → the entry file of the module's local source directory (an `imports`
//!     edge, so a module call connects to the code it instantiates).
//!   - `module.M:var.<in>`   → the child module's `variable "<in>"` node — the module block sets
//!     that variable, so "what depends on the child's var.cidr" reaches every caller.
//!   - `module.M:output.<o>` → the child module's `output "<o>"` node — `module.M.o` uses flow
//!     through to the output's definition instead of dead-ending at the module declaration.
//!
//! The module's `source` is re-read from the declaration's file (cached lines); only local
//! `./`/`../` sources bridge. Registry/git sources stay unresolved — an out-of-repo module is a
//! visible boundary, never a guess.

use crate::ctx::Ctx;
use crate::types::{FrameworkResolver, NodeRef, ResolvedBy, ResolvedRef, UnresolvedRef};
use codegraph_jsre::{re, Re};
use codegraph_types::{Language, Node, NodeKind};

pub struct TerraformResolver;

const LANGUAGES: [Language; 1] = [Language::Terraform];

/// `module.M:file` / `module.M:var.X` / `module.M:output.X` / `module.M:remote-output.X` —
/// extractor-emitted scoped refs.
fn scoped_ref_re() -> &'static Re {
    re!(r"^module\.([^.:\s]+):(file$|var\.|output\.|remote-output\.)")
}

impl FrameworkResolver for TerraformResolver {
    fn name(&self) -> &str {
        "terraform"
    }

    fn languages(&self) -> Option<&[Language]> {
        Some(&LANGUAGES)
    }

    fn detect(&self, ctx: &Ctx) -> bool {
        ctx.get_all_files().iter().any(|f| f.ends_with(".tf") || f.ends_with(".tfvars") || f.ends_with(".tofu"))
    }

    // Scoped refs name no declared symbol; opt them through the resolver's name-exists pre-filter
    // so they reach resolve() at all.
    fn claims_reference(&self, name: &str) -> bool {
        scoped_ref_re().is_match(name)
    }

    fn resolve(&self, r: &UnresolvedRef, ctx: &Ctx) -> Option<ResolvedRef> {
        if r.language != Language::Terraform {
            return None;
        }

        let qname = r.reference_name.as_str();
        let ref_dir = dir_of(&r.file_path);

        // --- module-boundary bridge: module.M:file / module.M:var.X / module.M:output.X ---
        if let Some(scoped) = re!(r"^module\.([^.:\s]+):(.+)$").captures(qname) {
            return resolve_scoped_module_ref(r, scoped.text(1)?, scoped.text(2)?, &ref_dir, ctx);
        }

        let candidates = ctx.get_nodes_by_qualified_name(qname);
        if candidates.is_empty() {
            return None;
        }

        // 1. Same directory — the only scope Terraform can actually reference.
        if let Some(same) = candidates.iter().find(|c| dir_of(&c.file_path) == ref_dir) {
            return Some(ResolvedRef::new(r, same.id.clone(), 0.95, ResolvedBy::Framework));
        }

        // 2. `.tfvars` assignments set ROOT module variables, and var-files are routinely kept in
        //    a subdirectory (`envs/prod.tfvars`). Walk up to the nearest ancestor directory that
        //    declares the variable.
        if r.file_path.ends_with(".tfvars") && qname.starts_with("var.") {
            if let Some(up) = nearest_ancestor_match(&candidates, &ref_dir) {
                return Some(ResolvedRef::new(r, up.id.clone(), 0.9, ResolvedBy::Framework));
            }
        }

        // 2b. Provider configurations are the one construct Terraform inherits across the module
        //     tree: they're declared in the root (or a parent) module and passed down, so
        //     `provider = aws.east` inside a child module legitimately names a configuration
        //     declared above it.
        if qname.starts_with("provider.") {
            let configs: Vec<NodeRef> = candidates.iter().filter(|c| c.kind == NodeKind::Namespace).cloned().collect();
            if let Some(up) = nearest_ancestor_match(&configs, &ref_dir) {
                return Some(ResolvedRef::new(r, up.id.clone(), 0.9, ResolvedBy::Framework));
            }
            return None;
        }

        // 3. No same-directory declaration → no edge. A candidate in another module directory is
        //    never the real target (cross-module access only exists through module.M
        //    inputs/outputs, bridged above), and a wrong edge is worse than none.
        None
    }
}

/// Nearest candidate walking UP the directory tree from `ref_dir` (exclusive).
fn nearest_ancestor_match(candidates: &[NodeRef], ref_dir: &str) -> Option<NodeRef> {
    let mut dir = parent_of(ref_dir);
    while let Some(current) = dir {
        if let Some(found) = candidates.iter().find(|c| dir_of(&c.file_path) == current) {
            return Some(found.clone());
        }
        dir = parent_of(&current);
    }
    None
}

/// Resolve `module.M:<child>` by locating the `module "M"` declaration in the reference's own
/// directory, reading its `source` attribute, and looking the child symbol up inside that
/// directory.
fn resolve_scoped_module_ref(r: &UnresolvedRef, module_name: &str, child: &str, ref_dir: &str, ctx: &Ctx) -> Option<ResolvedRef> {
    let decls: Vec<NodeRef> =
        ctx.get_nodes_by_qualified_name(&format!("module.{module_name}")).iter().filter(|n| n.kind == NodeKind::Module).cloned().collect();
    if decls.is_empty() {
        return None;
    }
    // Terraform scoping: the declaration lives in the reference's directory.
    let decl = decls.iter().find(|d| dir_of(&d.file_path) == ref_dir).or(if decls.len() == 1 { decls.first() } else { None })?;

    let source = read_module_attr(decl, "source", ctx)?;

    // --- cloudposse/atmos remote-state: module.M.outputs.X where M is the stack-config
    // remote-state module reading another COMPONENT's state. The component name is static in the
    // monorepo case (`component = "vpc"` or "eks/cluster"), so bridge to that component
    // directory's own `output "X"` — but only when every gate holds: the module source is the
    // remote-state module, the component is a string literal, and exactly ONE directory in the
    // repo matches the component name and declares that output. Anything dynamic or ambiguous
    // stays a visible boundary.
    if let Some(out_name) = child.strip_prefix("remote-output.") {
        if !re!(r"\/remote-state(\/|$)").is_match(&source) {
            return None;
        }
        let mut component = read_module_attr(decl, "component", ctx);
        if component.is_none() {
            // The other half of real-world declarations indirect through a variable with a
            // literal default in the same directory (`component = var.vpc_component_name` +
            // `default = "vpc"`) — the component's declared static wiring. One hop, same
            // literal gate.
            if let Some(via_var) = read_node_span_match(decl, re!(r"^\s*component\s*=\s*var\.([A-Za-z0-9_-]+)\s*$"), ctx) {
                let declared: Vec<NodeRef> = ctx
                    .get_nodes_by_qualified_name(&format!("var.{via_var}"))
                    .iter()
                    .filter(|n| dir_of(&n.file_path) == dir_of(&decl.file_path))
                    .cloned()
                    .collect();
                if declared.len() == 1 {
                    component = read_node_span_match(&declared[0], re!(r#"^\s*default\s*=\s*"([^"]+)""#), ctx);
                }
            }
        }
        let component = component.filter(|c| !c.is_empty())?;
        let outs: Vec<NodeRef> = ctx
            .get_nodes_by_qualified_name(&format!("output.{out_name}"))
            .iter()
            .filter(|o| {
                let d = dir_of(&o.file_path);
                d == component || d.ends_with(&format!("/{component}"))
            })
            .cloned()
            .collect();
        if outs.is_empty() {
            return None;
        }
        let first_dir = dir_of(&outs[0].file_path);
        if outs.iter().any(|o| dir_of(&o.file_path) != first_dir) {
            return None; // two directories claim this component name — never guess
        }
        return Some(ResolvedRef::new(r, outs[0].id.clone(), 0.9, ResolvedBy::Framework));
    }

    if !(source.starts_with("./") || source.starts_with("../")) {
        // Registry / git / absolute sources are out-of-repo: stay unresolved.
        return None;
    }
    let target_dir = normalize_rel(&join_dirs(&dir_of(&decl.file_path), &source));

    if child == "file" {
        let mut tf_files: Vec<String> =
            ctx.get_all_files().into_iter().filter(|f| dir_of(f) == target_dir && (f.ends_with(".tf") || f.ends_with(".tofu"))).collect();
        tf_files.sort_by(|a, b| crate::import_resolver::python::js_cmp(a, b));
        if tf_files.is_empty() {
            return None;
        }
        let entry = tf_files.iter().find(|f| f.ends_with("/main.tf") || *f == "main.tf").unwrap_or(&tf_files[0]).clone();
        let file_node = ctx.get_nodes_in_file(&entry).iter().find(|n| n.kind == NodeKind::File).cloned()?;
        return Some(ResolvedRef::new(r, file_node.id.clone(), 0.95, ResolvedBy::Framework));
    }

    // child is `var.X` or `output.X` — the child module's own qualified names.
    let target = ctx.get_nodes_by_qualified_name(child).iter().find(|c| dir_of(&c.file_path) == target_dir).cloned()?;
    Some(ResolvedRef::new(r, target.id.clone(), 0.95, ResolvedBy::Framework))
}

/// A direct string-literal attribute (`source = "…"`, `component = "…"`) of a module declaration,
/// re-read from its file (project paths are stored relative; node metadata isn't persisted, so the
/// declaration's line span + cached file lines are the durable carrier). Non-literal values
/// (variables, expressions) return `None` — dynamic wiring is never guessed.
fn read_module_attr(decl: &Node, name: &str, ctx: &Ctx) -> Option<String> {
    let pattern = Re::cached(&format!(r#"^\s*{name}\s*=\s*"([^"]+)""#), "");
    read_node_span_match(decl, &pattern, ctx)
}

/// First capture of `re` across the node's line span, or `None`.
fn read_node_span_match(node: &Node, regex: &Re, ctx: &Ctx) -> Option<String> {
    let lines = ctx.get_file_lines(&node.file_path)?;
    let end = (node.end_line as usize).min(lines.len());
    let start = (node.start_line as usize).saturating_sub(1);
    for line in lines.iter().take(end).skip(start) {
        if let Some(m) = regex.captures(line) {
            return m.text(1).map(String::from);
        }
    }
    None
}

/// Directory of a stored (forward-slash, project-relative) path.
fn dir_of(p: &str) -> String {
    match p.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// Parent directory, or `None` above the project root.
fn parent_of(dir: &str) -> Option<String> {
    if dir == "." || dir.is_empty() {
        return None;
    }
    let parent = dir_of(dir);
    if parent == dir {
        None
    } else {
        Some(parent)
    }
}

/// Join a base directory with a `./`/`../` relative source path.
fn join_dirs(base: &str, rel: &str) -> String {
    let base = if base == "." { "" } else { base };
    if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Normalize to the stored path shape: forward slashes, '.' for the root.
fn normalize_rel(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let n = out.join("/");
    if n.is_empty() {
        ".".to_string()
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_directory_shaped() {
        assert_eq!(dir_of("main.tf"), ".");
        assert_eq!(dir_of("modules/vpc/main.tf"), "modules/vpc");
        assert_eq!(parent_of("envs"), Some(".".to_string()));
        assert_eq!(parent_of("a/b"), Some("a".to_string()));
        assert_eq!(parent_of("."), None);
        assert_eq!(normalize_rel(&join_dirs("envs/prod", "../../modules/vpc")), "modules/vpc");
        assert_eq!(normalize_rel(&join_dirs(".", "./modules/vpc/")), "modules/vpc");
        assert_eq!(normalize_rel(&join_dirs("a", "..")), ".");
    }

    #[test]
    fn scoped_refs_are_claimed() {
        assert!(TerraformResolver.claims_reference("module.vpc:file"));
        assert!(TerraformResolver.claims_reference("module.vpc:var.cidr"));
        assert!(TerraformResolver.claims_reference("module.vpc:remote-output.id"));
        assert!(!TerraformResolver.claims_reference("module.vpc"));
    }
}

//! The folder preference the component resolvers share (React, Svelte, Astro, Vue): when a
//! component name has several definitions, the ones in the folder of the file that uses the name,
//! or in a folder below it, come first.
//!
//! The folder is compared whole. A bare `starts_with("src/ui")` also held `src/ui-kit/Widget.tsx`,
//! so `src/ui/Card.tsx`'s `Widget` could land in the sibling folder; and a file at the project
//! root, whose folder is `""`, held every definition in the project, so its pick skipped the "only
//! one definition may resolve" rule that keeps an ambiguous name unlinked (#764).

/// The candidates in `from_file`'s folder or a folder below it, in their given order. A file at the
/// project root has no folder to prefer, so it gets none.
pub fn in_referencing_folder<'a, T: 'a>(
    from_file: &'a str,
    candidates: impl IntoIterator<Item = T> + 'a,
    path_of: impl Fn(&T) -> &str + 'a,
) -> impl Iterator<Item = T> + 'a {
    let folder = from_file.rfind('/').map(|cut| &from_file[..=cut]);
    candidates.into_iter().filter(move |candidate| folder.is_some_and(|folder| path_of(candidate).starts_with(folder)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_folders_whole_keeps_folders_below_and_gives_a_root_file_none() {
        let files = ["src/ui-kit/Widget.tsx", "src/ui/forms/Widget.tsx", "src/ui/Widget.tsx", "Widget.tsx"];
        let beside_card: Vec<&str> = in_referencing_folder("src/ui/Card.tsx", files, |f| f).collect();
        assert_eq!(beside_card, ["src/ui/forms/Widget.tsx", "src/ui/Widget.tsx"]);
        assert_eq!(in_referencing_folder("App.tsx", files, |f| f).count(), 0);
    }

    #[test]
    fn works_over_borrowed_candidates_of_any_type() {
        let nodes = vec![("a", String::from("pkg/a/Thing.vue")), ("b", String::from("pkg/ab/Thing.vue"))];
        let picked: Vec<&str> = in_referencing_folder("pkg/a/Page.vue", nodes.iter(), |n| n.1.as_str()).map(|n| n.0).collect();
        assert_eq!(picked, ["a"]);
    }
}

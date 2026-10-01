//! Cross-file label resolution: union the per-file label definitions across the
//! inclusion graph so a `\ref` can be resolved against the whole document, and
//! a key defined in two files of one document can be flagged as a duplicate.
//!
//! Layered like [`crate::project::graph`]: [`ResolvedLabels::build`] is the
//! **pure** algorithm (no salsa, no disk), and [`crate::project::resolved_labels`]
//! is a thin tracked wrapper. The CLI calls the pure builder directly (one-shot,
//! no salsa); the language server (eventually) uses the query. Both feed the same
//! data into the linter, so results match.
//!
//! **Namespace = directed document reachability.** LaTeX labels share one namespace
//! per *compiled document*, rooted at each compilation entry point (`\documentclass`
//! or `\begin{document}`). A file's namespace consists of all files reachable from
//! the roots that include it. Multiple independent documents (e.g. a paper and slides,
//! or different versions of a paper) that share a common include (e.g. `macros.tex` or
//! `appendix.tex`) remain isolated into their respective document scopes, preventing
//! spurious cross-document duplicate label warnings.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use smol_str::SmolStr;

use crate::ast::{command_name, environment_name};
use crate::incremental::{
    IncrementalDb, QueryKind, QueryLogEntry, file_is_document_root, file_labels, file_refs,
};
use crate::project::graph::{IncludeGraph, project_graph};
use crate::semantic::SemanticModel;
use crate::syntax::{SyntaxKind, SyntaxNode};

/// The distinct label names defined in `model`, sorted and deduped—the per-file
/// label input to [`ResolvedLabels::build`]. Shared by the CLI
/// (one-shot, non-salsa) and the [`crate::incremental::file_labels`] firewall so
/// both feed identical data into the resolver.
pub fn document_label_names(model: &SemanticModel) -> Vec<SmolStr> {
    let mut names: Vec<SmolStr> = model
        .labels()
        .iter()
        .map(|label| label.name.clone())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The distinct `\ref`-family key names *used* in `model`, sorted and deduped —
/// the per-file reference input to [`ResolvedLabels::build`], the mirror image of
/// [`document_label_names`]. A `\cref{a,b}` contributes both `a` and `b` (the
/// model already splits key lists). Feeds the cross-file `unreferenced-label`
/// lint, which asks whether a label definition is targeted *anywhere* in the
/// namespace.
pub fn document_ref_names(model: &SemanticModel) -> Vec<SmolStr> {
    let mut names: Vec<SmolStr> = model.refs().iter().map(|r| r.name.clone()).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The distinct glossary/acronym keys defined in `model`
/// (`\newglossaryentry`/`\newacronym`/…), sorted and deduped — the per-file input
/// to the [`crate::incremental::file_glossary_keys`] firewall, the glossary
/// analog of [`document_label_names`].
pub fn document_glossary_keys(model: &SemanticModel) -> Vec<SmolStr> {
    let mut keys: Vec<SmolStr> = model
        .glossary_defs()
        .iter()
        .map(|def| def.key.clone())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Whether `root` carries a `\documentclass` or a `\begin{document}` — the
/// document-root signal gating the `undefined-ref` lint (see [`ResolvedLabels`]).
/// Shared by the CLI and the [`crate::incremental::file_is_document_root`]
/// firewall.
pub fn is_document_root(root: &SyntaxNode) -> bool {
    root.descendants().any(|node| match node.kind() {
        SyntaxKind::COMMAND => command_name(&node).as_deref() == Some("documentclass"),
        // The `document` environment's name lives on its `\begin{document}`.
        SyntaxKind::BEGIN => environment_name(&node).as_deref() == Some("document"),
        _ => false,
    })
}

/// One label namespace: a document scope defined by compilation root reachability.
#[derive(Debug, Default)]
struct Component {
    /// Member files in this document scope, sorted.
    members: Vec<PathBuf>,
    /// Label name → the files in this component that define it, sorted & deduped.
    defs: HashMap<SmolStr, Vec<PathBuf>>,
    /// Every `\ref`-family key *used* by any file in the component. Membership
    /// only (order-free), so a `HashSet`: `unreferenced-label` asks "is this
    /// label referenced somewhere in the namespace?", the mirror of `defs`.
    refs: HashSet<SmolStr>,
    /// Whether every include in the component resolves to an analyzed member: no
    /// dynamic and no external (out-of-set) targets. Only then is "defined
    /// nowhere" trustworthy enough to drive `undefined-ref`.
    closed: bool,
    /// Whether any member is a document root (`\documentclass` /
    /// `\begin{document}`). `undefined-ref` fires only inside a rooted namespace.
    rooted: bool,
}

/// The resolved cross-file label model over a set of analyzed files.
///
/// Holds `HashMap`s/`PathBuf`s, so (like [`IncludeGraph`]) it is neither `Eq` nor
/// `salsa::SalsaValue`; the [`crate::project::resolved_labels`] query is therefore
/// `no_eq`. Built by [`ResolvedLabels::build`].
#[derive(Debug, Default)]
pub struct ResolvedLabels {
    /// File path → index into [`components`](Self::components).
    component_of: HashMap<PathBuf, usize>,
    components: Vec<Component>,
}

impl ResolvedLabels {
    /// Resolve labels for `files` — each a `(path, distinct sorted label names,
    /// distinct sorted `\ref` key names, is_document_root)` tuple — partitioned by
    /// the inclusion `graph`.
    ///
    /// Pure and deterministic: components are assigned in sorted-path order and
    /// every definer list is sorted, so the output never depends on `HashMap`
    /// iteration order. (The per-component reference set is queried by membership
    /// only, so its iteration order never reaches the output.)
    pub fn build(
        files: &[(PathBuf, Vec<SmolStr>, Vec<SmolStr>, bool)],
        graph: &IncludeGraph,
    ) -> Self {
        let file_roots: Vec<(&Path, bool)> = files
            .iter()
            .map(|(p, _, _, is_root)| (p.as_path(), *is_root))
            .collect();
        let (component_of, member_lists) = graph.document_components(&file_roots);

        let mut components: Vec<Component> = member_lists
            .into_iter()
            .map(|members| Component {
                members,
                closed: true,
                ..Component::default()
            })
            .collect();

        // Index definitions, references, and the rooted flag per component.
        let file_facts: HashMap<&Path, (&[SmolStr], &[SmolStr], bool)> = files
            .iter()
            .map(|(p, names, refs, is_root)| {
                (p.as_path(), (names.as_slice(), refs.as_slice(), *is_root))
            })
            .collect();

        for comp in &mut components {
            for member in &comp.members {
                if let Some(&(names, refs, is_root)) = file_facts.get(member.as_path()) {
                    comp.rooted |= is_root;
                    for name in names {
                        comp.defs
                            .entry(name.clone())
                            .or_default()
                            .push(member.clone());
                    }
                    comp.refs.extend(refs.iter().cloned());
                }
            }
        }

        // An unresolved include (dynamic or out-of-set) opens any component whose
        // visible set contains the including file.
        let mut unresolved_from: HashSet<&Path> = HashSet::new();
        for edge in graph.unresolved() {
            unresolved_from.insert(edge.from.as_path());
        }
        for comp in &mut components {
            if comp
                .members
                .iter()
                .any(|m| unresolved_from.contains(m.as_path()))
            {
                comp.closed = false;
            }
        }

        // Canonicalize definer lists (a file appears at most once per name —
        // `file_labels` is already deduped — but distinct files arrive unordered).
        for comp in &mut components {
            for definers in comp.defs.values_mut() {
                definers.sort_unstable();
                definers.dedup();
            }
        }

        Self {
            component_of,
            components,
        }
    }

    /// Files in `file`'s namespace that define `name`, sorted. Empty when `file`
    /// is unknown or `name` is undefined in its component. Includes `file` itself
    /// when it defines `name`; callers wanting *other* definers filter it out.
    pub fn definers(&self, file: &Path, name: &str) -> &[PathBuf] {
        self.component_of
            .get(file)
            .and_then(|&id| self.components[id].defs.get(name))
            .map_or(&[], Vec::as_slice)
    }

    /// Whether `name` is defined anywhere in `file`'s namespace.
    pub fn is_defined(&self, file: &Path, name: &str) -> bool {
        !self.definers(file, name).is_empty()
    }

    /// Whether `name` is targeted by a `\ref`-family command anywhere in `file`'s
    /// namespace. The mirror of [`is_defined`](Self::is_defined): `undefined-ref`
    /// asks whether a *reference* has a definition, `unreferenced-label` asks
    /// whether a *definition* has a reference. Both are trustworthy only over a
    /// closed, rooted namespace (see [`is_closed`](Self::is_closed) /
    /// [`is_root_component`](Self::is_root_component)).
    pub fn is_referenced(&self, file: &Path, name: &str) -> bool {
        self.component_of
            .get(file)
            .is_some_and(|&id| self.components[id].refs.contains(name))
    }

    /// All member files sharing `file`'s namespace (its document scope),
    /// sorted; empty when `file` is unknown. Includes `file` itself. Unlike
    /// [`definers`](Self::definers) (which files *define* a name) this is every
    /// file in the namespace — the search set for find-references, which must scan
    /// each member for `\ref` use sites.
    pub fn namespace_members(&self, file: &Path) -> Vec<&Path> {
        self.component_of
            .get(file)
            .map(|&id| {
                self.components[id]
                    .members
                    .iter()
                    .map(|p| p.as_path())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `file`'s namespace is closed — every include resolves to an
    /// analyzed member. Gates `undefined-ref` (an open namespace may define the
    /// key in a file we never saw).
    pub fn is_closed(&self, file: &Path) -> bool {
        self.component_of
            .get(file)
            .is_some_and(|&id| self.components[id].closed)
    }

    /// Whether `file`'s namespace contains a document root. Gates `undefined-ref`
    /// so a bare fragment opened standalone is never flagged.
    pub fn is_root_component(&self, file: &Path) -> bool {
        self.component_of
            .get(file)
            .is_some_and(|&id| self.components[id].rooted)
    }
}

/// The cross-file label resolution for `project`, built from the per-file
/// [`file_labels`] firewall and the [`project_graph`].
///
/// `no_eq` + `unsafe(non_salsa_values)` for the same reason as [`project_graph`]:
/// [`ResolvedLabels`] holds `HashMap`s (not `Eq`/`salsa::SalsaValue`) and is a pure
/// function of the backdated [`Project`] plus the backdated per-file facts, so it
/// carries no salsa references. The firewall pays off here: a prose edit leaves
/// `file_labels`, `file_refs`, `file_is_document_root`, and `include_edges` all
/// backdated, so neither [`project_graph`] nor this query re-executes. A `\ref`
/// edit *does* rebuild this query (it changes `file_refs`), because
/// `unreferenced-label` depends on the cross-file reference union — but a pure
/// prose edit still backdates both firewalls.
#[salsa::tracked(returns(ref), no_eq, unsafe(non_salsa_values))]
pub fn resolved_labels(db: &dyn IncrementalDb) -> ResolvedLabels {
    db.record_query(QueryLogEntry {
        kind: QueryKind::ResolvedLabels,
        file: None,
    });

    let project = crate::project::workspace_project(db);
    let graph = project_graph(db);
    // Labels live in LaTeX files (`.tex`/`.sty`/`.cls`); `.bib` members carry none
    // and are not part of the include-graph namespace.
    let files: Vec<(PathBuf, Vec<SmolStr>, Vec<SmolStr>, bool)> = project
        .members
        .iter()
        .filter(|member| member.kind.is_latex())
        .map(|member| {
            (
                member.path.clone(),
                file_labels(db, member.file).clone(),
                file_refs(db, member.file).clone(),
                *file_is_document_root(db, member.file),
            )
        })
        .collect();

    ResolvedLabels::build(&files, graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::graph::FileFacts;
    use crate::project::include::{IncludeEdgeKey, IncludeKind, IncludeTarget};

    /// Build an `IncludeGraph` from `(path, [(kind, target)])` tuples.
    fn graph(files: &[(&str, &[(IncludeKind, &str)])]) -> IncludeGraph {
        let facts: Vec<FileFacts> = files
            .iter()
            .map(|(path, edges)| FileFacts {
                path: PathBuf::from(path),
                include_edges: edges
                    .iter()
                    .map(|(kind, target)| IncludeEdgeKey {
                        kind: *kind,
                        target: IncludeTarget::Path(PathBuf::from(target)),
                    })
                    .collect(),
            })
            .collect();
        IncludeGraph::build(&facts, None)
    }

    fn names(list: &[&str]) -> Vec<SmolStr> {
        list.iter().map(SmolStr::new).collect()
    }

    #[test]
    fn lone_file_is_its_own_component() {
        let g = graph(&[("/p/a.tex", &[])]);
        let r = ResolvedLabels::build(
            &[(PathBuf::from("/p/a.tex"), names(&["x"]), names(&[]), false)],
            &g,
        );
        assert!(r.is_defined(Path::new("/p/a.tex"), "x"));
        assert!(!r.is_defined(Path::new("/p/a.tex"), "y"));
        // No other file defines `x`.
        assert_eq!(
            r.definers(Path::new("/p/a.tex"), "x"),
            &[PathBuf::from("/p/a.tex")]
        );
    }

    #[test]
    fn input_chain_shares_one_namespace() {
        let g = graph(&[
            ("/p/main.tex", &[(IncludeKind::Input, "/p/chap.tex")]),
            ("/p/chap.tex", &[]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/main.tex"),
                    names(&[]),
                    names(&["a"]),
                    true,
                ),
                (
                    PathBuf::from("/p/chap.tex"),
                    names(&["a"]),
                    names(&[]),
                    false,
                ),
            ],
            &g,
        );
        // A label in the chapter is visible from the main file's namespace.
        assert!(r.is_defined(Path::new("/p/main.tex"), "a"));
        assert!(r.is_root_component(Path::new("/p/chap.tex")));
        assert!(r.is_closed(Path::new("/p/main.tex")));
        // The chapter's `\label{a}` is referenced cross-file (from main), visible
        // when the whole namespace is queried from either member.
        assert!(r.is_referenced(Path::new("/p/chap.tex"), "a"));
        assert!(!r.is_referenced(Path::new("/p/chap.tex"), "b"));
    }

    #[test]
    fn diamond_merges_all_four() {
        let g = graph(&[
            (
                "/p/main.tex",
                &[
                    (IncludeKind::Input, "/p/a.tex"),
                    (IncludeKind::Input, "/p/b.tex"),
                ],
            ),
            ("/p/a.tex", &[(IncludeKind::Input, "/p/shared.tex")]),
            ("/p/b.tex", &[(IncludeKind::Input, "/p/shared.tex")]),
            ("/p/shared.tex", &[]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (PathBuf::from("/p/main.tex"), names(&[]), names(&[]), true),
                (PathBuf::from("/p/a.tex"), names(&["k"]), names(&[]), false),
                (PathBuf::from("/p/b.tex"), names(&["k"]), names(&[]), false),
                (
                    PathBuf::from("/p/shared.tex"),
                    names(&[]),
                    names(&[]),
                    false,
                ),
            ],
            &g,
        );
        // `k` defined in both a and b → both are cross-file definers, sorted.
        assert_eq!(
            r.definers(Path::new("/p/a.tex"), "k"),
            &[PathBuf::from("/p/a.tex"), PathBuf::from("/p/b.tex")]
        );
        // The whole diamond is one namespace: every member is a reference-search
        // target, regardless of whether it defines anything.
        assert_eq!(
            r.namespace_members(Path::new("/p/shared.tex")),
            &[
                Path::new("/p/a.tex"),
                Path::new("/p/b.tex"),
                Path::new("/p/main.tex"),
                Path::new("/p/shared.tex"),
            ]
        );
    }

    #[test]
    fn namespace_members_isolates_independent_documents() {
        let g = graph(&[("/p/one.tex", &[]), ("/p/two.tex", &[])]);
        let r = ResolvedLabels::build(
            &[
                (PathBuf::from("/p/one.tex"), names(&["x"]), names(&[]), true),
                (PathBuf::from("/p/two.tex"), names(&["x"]), names(&[]), true),
            ],
            &g,
        );
        assert_eq!(
            r.namespace_members(Path::new("/p/one.tex")),
            &[Path::new("/p/one.tex")]
        );
        assert!(r.namespace_members(Path::new("/p/missing.tex")).is_empty());
    }

    #[test]
    fn independent_documents_do_not_share_labels() {
        let g = graph(&[("/p/one.tex", &[]), ("/p/two.tex", &[])]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/one.tex"),
                    names(&["intro"]),
                    names(&[]),
                    true,
                ),
                (
                    PathBuf::from("/p/two.tex"),
                    names(&["intro"]),
                    names(&[]),
                    true,
                ),
            ],
            &g,
        );
        // Same key in two unrelated docs is NOT a cross-file duplicate.
        assert_eq!(
            r.definers(Path::new("/p/one.tex"), "intro"),
            &[PathBuf::from("/p/one.tex")]
        );
        assert_eq!(
            r.definers(Path::new("/p/two.tex"), "intro"),
            &[PathBuf::from("/p/two.tex")]
        );
    }

    #[test]
    fn cycle_is_one_component() {
        let g = graph(&[
            ("/p/a.tex", &[(IncludeKind::Input, "/p/b.tex")]),
            ("/p/b.tex", &[(IncludeKind::Input, "/p/a.tex")]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (PathBuf::from("/p/a.tex"), names(&["x"]), names(&[]), false),
                (PathBuf::from("/p/b.tex"), names(&[]), names(&[]), false),
            ],
            &g,
        );
        assert!(r.is_defined(Path::new("/p/b.tex"), "x"));
    }

    #[test]
    fn dynamic_include_opens_the_component() {
        let g = {
            let facts = vec![FileFacts {
                path: PathBuf::from("/p/main.tex"),
                include_edges: vec![IncludeEdgeKey {
                    kind: IncludeKind::Input,
                    target: IncludeTarget::Dynamic,
                }],
            }];
            IncludeGraph::build(&facts, None)
        };
        let r = ResolvedLabels::build(
            &[(PathBuf::from("/p/main.tex"), names(&[]), names(&[]), true)],
            &g,
        );
        assert!(!r.is_closed(Path::new("/p/main.tex")));
    }

    #[test]
    fn external_include_opens_the_component() {
        // `/p/missing.tex` is not an analyzed member → unresolved → open.
        let g = graph(&[("/p/main.tex", &[(IncludeKind::Input, "/p/missing.tex")])]);
        let r = ResolvedLabels::build(
            &[(PathBuf::from("/p/main.tex"), names(&[]), names(&[]), true)],
            &g,
        );
        assert!(!r.is_closed(Path::new("/p/main.tex")));
    }

    #[test]
    fn rootless_component_reports_no_root() {
        let g = graph(&[("/p/frag.tex", &[])]);
        let r = ResolvedLabels::build(
            &[(
                PathBuf::from("/p/frag.tex"),
                names(&["x"]),
                names(&[]),
                false,
            )],
            &g,
        );
        assert!(!r.is_root_component(Path::new("/p/frag.tex")));
        assert!(r.is_closed(Path::new("/p/frag.tex")));
    }

    #[test]
    fn is_referenced_tracks_the_component_reference_union() {
        // One namespace: `a` is defined and referenced (in-file), `b` is defined
        // but never referenced anywhere, `c` is referenced but undefined.
        let g = graph(&[("/p/a.tex", &[])]);
        let r = ResolvedLabels::build(
            &[(
                PathBuf::from("/p/a.tex"),
                names(&["a", "b"]),
                names(&["a", "c"]),
                true,
            )],
            &g,
        );
        assert!(r.is_referenced(Path::new("/p/a.tex"), "a"));
        assert!(!r.is_referenced(Path::new("/p/a.tex"), "b"));
        // A referenced-but-undefined key still reads as referenced (that is
        // `undefined-ref`'s concern, not this method's).
        assert!(r.is_referenced(Path::new("/p/a.tex"), "c"));
        // An unknown file has an empty reference set.
        assert!(!r.is_referenced(Path::new("/p/missing.tex"), "a"));
    }

    #[test]
    fn two_roots_sharing_an_include_do_not_share_labels() {
        // Two independent documents (paper & slides) both include macros.tex.
        // Both define `\label{thm:main}`. They must remain separate and NOT
        // report each other as cross-file duplicate definers.
        let g = graph(&[
            ("/p/paper.tex", &[(IncludeKind::Input, "/p/macros.tex")]),
            ("/p/slides.tex", &[(IncludeKind::Input, "/p/macros.tex")]),
            ("/p/macros.tex", &[]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/paper.tex"),
                    names(&["thm:main"]),
                    names(&[]),
                    true,
                ),
                (
                    PathBuf::from("/p/slides.tex"),
                    names(&["thm:main"]),
                    names(&[]),
                    true,
                ),
                (
                    PathBuf::from("/p/macros.tex"),
                    names(&[]),
                    names(&[]),
                    false,
                ),
            ],
            &g,
        );

        assert_eq!(
            r.definers(Path::new("/p/paper.tex"), "thm:main"),
            &[PathBuf::from("/p/paper.tex")]
        );
        assert_eq!(
            r.definers(Path::new("/p/slides.tex"), "thm:main"),
            &[PathBuf::from("/p/slides.tex")]
        );
        // macros.tex is visible from both roots, so its query sees both.
        assert_eq!(
            r.definers(Path::new("/p/macros.tex"), "thm:main"),
            &[
                PathBuf::from("/p/paper.tex"),
                PathBuf::from("/p/slides.tex")
            ]
        );

        // Namespace members for each root is only its own reachable tree.
        assert_eq!(
            r.namespace_members(Path::new("/p/paper.tex")),
            &[Path::new("/p/macros.tex"), Path::new("/p/paper.tex")]
        );
        assert_eq!(
            r.namespace_members(Path::new("/p/slides.tex")),
            &[Path::new("/p/macros.tex"), Path::new("/p/slides.tex")]
        );
    }

    #[test]
    fn shared_include_defining_label_warns_in_both_roots() {
        // An appendix defining a label is included by two documents.
        // paper.tex defines a colliding label, slides.tex does not.
        let g = graph(&[
            ("/p/paper.tex", &[(IncludeKind::Input, "/p/appendix.tex")]),
            ("/p/slides.tex", &[(IncludeKind::Input, "/p/appendix.tex")]),
            ("/p/appendix.tex", &[]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/paper.tex"),
                    names(&["thm:dup"]),
                    names(&[]),
                    true,
                ),
                (PathBuf::from("/p/slides.tex"), names(&[]), names(&[]), true),
                (
                    PathBuf::from("/p/appendix.tex"),
                    names(&["thm:dup", "sec:app"]),
                    names(&[]),
                    false,
                ),
            ],
            &g,
        );

        // In paper.tex, thm:dup collides with appendix.tex.
        assert_eq!(
            r.definers(Path::new("/p/paper.tex"), "thm:dup"),
            &[
                PathBuf::from("/p/appendix.tex"),
                PathBuf::from("/p/paper.tex")
            ]
        );
        // In slides.tex, thm:dup is only in appendix.tex (no duplicate for slides).
        assert_eq!(
            r.definers(Path::new("/p/slides.tex"), "thm:dup"),
            &[PathBuf::from("/p/appendix.tex")]
        );
        // sec:app is defined in appendix.tex and unique in both.
        assert_eq!(
            r.definers(Path::new("/p/paper.tex"), "sec:app"),
            &[PathBuf::from("/p/appendix.tex")]
        );
        assert_eq!(
            r.definers(Path::new("/p/slides.tex"), "sec:app"),
            &[PathBuf::from("/p/appendix.tex")]
        );
    }

    #[test]
    fn rootless_files_sharing_include_split_by_indegree_zero() {
        // When no file has `\documentclass`, in-degree 0 files act as roots.
        let g = graph(&[
            ("/p/ch1.tex", &[(IncludeKind::Input, "/p/defs.tex")]),
            ("/p/ch2.tex", &[(IncludeKind::Input, "/p/defs.tex")]),
            ("/p/defs.tex", &[]),
        ]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/ch1.tex"),
                    names(&["label"]),
                    names(&[]),
                    false,
                ),
                (
                    PathBuf::from("/p/ch2.tex"),
                    names(&["label"]),
                    names(&[]),
                    false,
                ),
                (PathBuf::from("/p/defs.tex"), names(&[]), names(&[]), false),
            ],
            &g,
        );
        assert_eq!(
            r.definers(Path::new("/p/ch1.tex"), "label"),
            &[PathBuf::from("/p/ch1.tex")]
        );
        assert_eq!(
            r.definers(Path::new("/p/ch2.tex"), "label"),
            &[PathBuf::from("/p/ch2.tex")]
        );
    }

    #[test]
    fn subfiles_parent_edge_treats_parent_as_root() {
        let g = graph(&[
            ("/p/main.tex", &[(IncludeKind::SubFile, "/p/ch1.tex")]),
            (
                "/p/ch1.tex",
                &[(IncludeKind::SubFilesParent, "/p/main.tex")],
            ),
        ]);
        let r = ResolvedLabels::build(
            &[
                (
                    PathBuf::from("/p/main.tex"),
                    names(&["m"]),
                    names(&[]),
                    true,
                ),
                (PathBuf::from("/p/ch1.tex"), names(&["c"]), names(&[]), true),
            ],
            &g,
        );
        // Both files share one document scope rooted at main.tex.
        assert_eq!(
            r.namespace_members(Path::new("/p/ch1.tex")),
            &[Path::new("/p/ch1.tex"), Path::new("/p/main.tex")]
        );
        assert!(r.is_defined(Path::new("/p/ch1.tex"), "m"));
        assert!(r.is_defined(Path::new("/p/main.tex"), "c"));
    }
}

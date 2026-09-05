//! The paths a Codex `apply_patch` call touches — and nothing else about it.
//!
//! Codex edits files through one tool, `apply_patch`, whose hook payload is
//! the whole patch as text. One call can add, update, move and delete several
//! files. knoot needs the *paths* and the *kind* of each operation, so it can
//! gate the writes and announce the deletions; it never needs the hunks, and
//! this module does not return them. The diff body stays in the hook process
//! and goes nowhere — see "What crosses the wire" in the README.
//!
//! The envelope, from Codex's own `apply_patch` crate:
//!
//! ```text
//! *** Begin Patch
//! *** Add File: src/new.rs
//! +line
//! *** Update File: src/old.rs
//! *** Move to: src/renamed.rs
//! @@ hunk header
//! -old
//! +new
//! *** Delete File: src/gone.rs
//! *** End Patch
//! ```
//!
//! Anything this parser does not recognise is skipped, never guessed at: an
//! operation it invents would block a write nobody is making.

/// One file-level operation in a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// A file that does not exist yet. Gated as a *creation*, which is a
    /// different collision from an edit: two agents creating one new file is
    /// something a claim on an existing path cannot see.
    Add(String),
    /// An existing file, edited in place.
    Update(String),
    /// An existing file, edited and renamed. The old path stops existing,
    /// which is what everyone who read it needs to hear; the new one is a
    /// creation.
    Move { from: String, to: String },
    /// A file removed outright.
    Delete(String),
}

impl PatchOp {
    /// Every path this operation writes, for gating. A move writes both ends:
    /// the source is emptied and the destination is created.
    pub fn writes(&self) -> Vec<(String, bool)> {
        match self {
            PatchOp::Add(p) => vec![(p.clone(), true)],
            PatchOp::Update(p) => vec![(p.clone(), false)],
            PatchOp::Move { from, to } => vec![(from.clone(), false), (to.clone(), true)],
            PatchOp::Delete(p) => vec![(p.clone(), false)],
        }
    }

    /// The path this operation makes stop existing, if any, and whether it
    /// was a move rather than a deletion.
    pub fn removal(&self) -> Option<(String, bool)> {
        match self {
            PatchOp::Move { from, .. } => Some((from.clone(), true)),
            PatchOp::Delete(p) => Some((p.clone(), false)),
            _ => None,
        }
    }
}

/// The file operations in a patch, in order. Empty for anything that is not
/// a patch envelope, including an empty string.
pub fn ops(patch: &str) -> Vec<PatchOp> {
    let mut out: Vec<PatchOp> = Vec::new();
    for line in patch.lines() {
        // Only marker lines matter; hunk content is skipped whatever it says.
        // A `+*** Add File: x` inside a hunk is content, and it has a leading
        // `+`, so this cannot be fooled by a patch that adds a patch.
        let Some(rest) = line.strip_prefix("*** ") else { continue };
        if let Some(p) = rest.strip_prefix("Add File: ") {
            push_path(&mut out, p, PatchOp::Add);
        } else if let Some(p) = rest.strip_prefix("Update File: ") {
            push_path(&mut out, p, PatchOp::Update);
        } else if let Some(p) = rest.strip_prefix("Delete File: ") {
            push_path(&mut out, p, PatchOp::Delete);
        } else if let Some(to) = rest.strip_prefix("Move to: ") {
            // Belongs to the Update immediately before it. A stray Move with
            // no Update is malformed and is ignored rather than guessed at.
            let to = to.trim();
            if to.is_empty() {
                continue;
            }
            if let Some(PatchOp::Update(from)) = out.last().cloned() {
                out.pop();
                out.push(PatchOp::Move { from, to: to.to_string() });
            }
        }
    }
    out
}

fn push_path(out: &mut Vec<PatchOp>, raw: &str, mk: fn(String) -> PatchOp) {
    let p = raw.trim();
    if !p.is_empty() {
        out.push(mk(p.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "*** Begin Patch\n\
*** Add File: src/new.rs\n\
+fn main() {}\n\
*** Update File: src/old.rs\n\
*** Move to: src/renamed.rs\n\
@@ fn a\n\
-let x = 1;\n\
+let x = 2;\n\
*** Update File: src/kept.rs\n\
@@\n\
+// note\n\
*** Delete File: src/gone.rs\n\
*** End Patch\n";

    #[test]
    fn every_kind_of_operation_is_read_and_nothing_else_is() {
        assert_eq!(
            ops(PATCH),
            vec![
                PatchOp::Add("src/new.rs".into()),
                PatchOp::Move { from: "src/old.rs".into(), to: "src/renamed.rs".into() },
                PatchOp::Update("src/kept.rs".into()),
                PatchOp::Delete("src/gone.rs".into()),
            ]
        );
    }

    #[test]
    fn a_move_writes_both_ends_and_removes_the_source() {
        let mv = PatchOp::Move { from: "a".into(), to: "b".into() };
        assert_eq!(mv.writes(), vec![("a".to_string(), false), ("b".to_string(), true)]);
        assert_eq!(mv.removal(), Some(("a".to_string(), true)));
        assert_eq!(PatchOp::Delete("c".into()).removal(), Some(("c".to_string(), false)));
        assert_eq!(PatchOp::Update("d".into()).removal(), None);
    }

    /// Hunk content that happens to look like a marker is content. A patch
    /// that adds a file *containing* a patch must not be read as touching the
    /// files that inner patch names.
    #[test]
    fn a_patch_that_contains_a_patch_touches_only_its_own_files() {
        let p = "*** Begin Patch\n*** Add File: docs/example.patch\n+*** Begin Patch\n+*** Update File: src/secret.rs\n+*** End Patch\n*** End Patch\n";
        assert_eq!(ops(p), vec![PatchOp::Add("docs/example.patch".into())]);
    }

    #[test]
    fn not_a_patch_yields_nothing() {
        assert!(ops("").is_empty());
        assert!(ops("echo hello > out.txt").is_empty());
        // A Move with no Update before it is malformed and is not guessed at.
        assert!(ops("*** Begin Patch\n*** Move to: x\n*** End Patch\n").is_empty());
    }
}

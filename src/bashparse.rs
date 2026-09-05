//! Finding the files a Bash command will write.
//!
//! This is a heuristic and says so: `audit` reports when the command could not
//! be proven read-only, which is the caller's signal to fall back to a
//! working-tree diff after the fact. False positives block legitimate work, so
//! quoting is respected and only well-understood forms yield targets.

#[derive(Debug, Default, PartialEq)]
pub struct Analysis {
    /// Paths this command is expected to write, as written on the command line.
    pub targets: Vec<String>,
    /// True when the command could not be proven read-only. Anything unknown,
    /// any interpreter, any command substitution.
    pub audit: bool,
    /// Paths this command is expected to make *stop existing*, and whether it
    /// was a move rather than a delete.
    ///
    /// These are targets too — a delete is a write and is gated as one — but
    /// they are also the half of a conflict a claim cannot express: 26.8% of
    /// real agent conflicts are modify/delete, and "someone still holds a
    /// claim on a file that is gone" is not a state anyone can act on. Kept
    /// separately so the daemon can announce them once they have happened.
    pub removals: Vec<(String, bool)>,
}

/// Commands that cannot modify the working tree.
const READ_ONLY: &[&str] = &[
    "ls", "cat", "bat", "head", "tail", "grep", "egrep", "fgrep", "rg", "ag", "ack", "find", "fd",
    "wc", "sort", "uniq", "cut", "tr", "echo", "printf", "pwd", "cd", "which", "type", "file",
    "stat", "du", "df", "date", "env", "basename", "dirname", "realpath", "readlink", "jq", "yq",
    "diff", "cmp", "shasum", "md5", "md5sum", "sha256sum", "tree", "column", "nl", "rev", "seq",
    "true", "false", "test", "sleep", "hostname", "whoami", "uname", "id", "tput", "less", "more",
];

/// Interpreters and build tools: they can write anything, so audit them.
const OPAQUE: &[&str] = &[
    "python", "python3", "node", "deno", "bun", "perl", "ruby", "php", "sh", "bash", "zsh", "make",
    "cargo", "npm", "pnpm", "yarn", "npx", "go", "gradle", "mvn", "rustc", "tsc", "eval", "exec",
    "source", ".", "xargs", "patch", "ed", "vim", "emacs", "rsync", "unzip", "tar", "brew", "pip",
    "pip3", "docker", "ansible", "terraform",
];

/// git subcommands that only read.
const GIT_READ_ONLY: &[&str] = &[
    "status", "diff", "log", "show", "ls-files", "ls-tree", "rev-parse", "rev-list", "blame",
    "describe", "cat-file", "grep", "shortlog", "remote", "branch", "tag", "config", "reflog",
    "symbolic-ref", "for-each-ref", "count-objects", "var", "help", "worktree",
];

const SINKS_TO_IGNORE: &[&str] = &[
    "/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty", "/dev/fd/1", "/dev/fd/2",
];

#[derive(Debug, PartialEq)]
enum Tok {
    Word { text: String, quoted: bool },
    Redir { append: bool },
    Sep,
}

/// Split a command line into words, redirects, and command separators,
/// honouring quotes so `echo "a > b"` yields no redirect.
///
/// Heredoc bodies are skipped wholesale. They are data, not shell: a body
/// containing `(sum, i) => sum + 1` would otherwise lex `=>` as a redirect and
/// claim a file called `sum`.
fn lex(cmd: &str) -> (Vec<Tok>, bool) {
    let c: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut quoted_word = false;
    let mut saw_subst = false;
    // Heredoc delimiters seen on this line; their bodies start at the next line.
    let mut pending_heredocs: Vec<String> = Vec::new();

    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                toks.push(Tok::Word { text: std::mem::take(&mut cur), quoted: quoted_word });
            }
            quoted_word = false;
        };
    }

    while i < c.len() {
        match c[i] {
            '\'' => {
                quoted_word = true;
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    cur.push(c[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                quoted_word = true;
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        cur.push(c[i + 1]);
                        i += 2;
                        continue;
                    }
                    if c[i] == '`' || (c[i] == '$' && c.get(i + 1) == Some(&'(')) {
                        saw_subst = true;
                    }
                    cur.push(c[i]);
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                if i + 1 < c.len() {
                    cur.push(c[i + 1]);
                }
                i += 2;
            }
            '`' => {
                saw_subst = true;
                cur.push(c[i]);
                i += 1;
            }
            '$' if c.get(i + 1) == Some(&'(') => {
                saw_subst = true;
                cur.push(c[i]);
                i += 1;
            }
            '<' if c.get(i + 1) == Some(&'<') => {
                flush!();
                i += 2;
                if c.get(i) == Some(&'-') {
                    i += 1;
                }
                while i < c.len() && c[i].is_whitespace() && c[i] != '\n' {
                    i += 1;
                }
                // Read the delimiter word, quotes and all.
                let mut delim = String::new();
                while i < c.len() && !c[i].is_whitespace() {
                    if c[i] != '\'' && c[i] != '"' {
                        delim.push(c[i]);
                    }
                    i += 1;
                }
                if !delim.is_empty() {
                    pending_heredocs.push(delim);
                }
            }
            '<' => {
                flush!();
                i += 1;
            }
            '>' => {
                // Drop a glued fd number (2>, 1>>) or `&>`.
                if cur.len() == 1 && (cur.starts_with(|ch: char| ch.is_ascii_digit()) || cur == "&") {
                    cur.clear();
                }
                // An `=` immediately before `>` is an arrow, not a redirect.
                let arrow = cur.ends_with('=');
                flush!();
                if arrow {
                    i += 1;
                    continue;
                }
                i += 1;
                let append = c.get(i) == Some(&'>');
                if append {
                    i += 1;
                }
                if c.get(i) == Some(&'|') {
                    i += 1;
                }
                if c.get(i) == Some(&'&') {
                    i += 1;
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }
                toks.push(Tok::Redir { append });
            }
            '\n' => {
                flush!();
                toks.push(Tok::Sep);
                i += 1;
                // Consume any heredoc bodies opened on the line just ended.
                while let Some(delim) = pending_heredocs.first().cloned() {
                    pending_heredocs.remove(0);
                    loop {
                        let start = i;
                        while i < c.len() && c[i] != '\n' {
                            i += 1;
                        }
                        let line: String = c[start..i].iter().collect();
                        if i < c.len() {
                            i += 1; // consume the newline
                        }
                        if line.trim() == delim || start >= c.len() {
                            break;
                        }
                    }
                }
            }
            ';' | '|' | '&' => {
                let ch = c[i];
                flush!();
                i += 1;
                while i < c.len() && c[i] == ch {
                    i += 1;
                }
                toks.push(Tok::Sep);
            }
            ch if ch.is_whitespace() => {
                flush!();
                i += 1;
            }
            ch => {
                cur.push(ch);
                i += 1;
            }
        }
    }
    flush!();
    (toks, saw_subst)
}

fn is_flag(s: &str) -> bool {
    s.starts_with('-') && s != "-"
}

fn base(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

pub fn analyze(cmd: &str) -> Analysis {
    let (toks, saw_subst) = lex(cmd);
    let mut out = Analysis { targets: Vec::new(), audit: saw_subst, removals: Vec::new() };

    // Split into command segments at separators.
    for seg in toks.split(|t| matches!(t, Tok::Sep)) {
        if seg.is_empty() {
            continue;
        }
        let mut argv: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < seg.len() {
            match &seg[i] {
                Tok::Word { text, .. } => argv.push(text),
                Tok::Redir { .. } => {
                    // The word after a redirect is a write target.
                    if let Some(Tok::Word { text, .. }) = seg.get(i + 1) {
                        if !SINKS_TO_IGNORE.contains(&text.as_str()) {
                            out.targets.push(text.clone());
                        }
                        i += 1;
                    }
                }
                Tok::Sep => {}
            }
            i += 1;
        }
        // Strip leading VAR=value assignments.
        let cmd_start = argv.iter().position(|a| !a.contains('=') || a.starts_with('='));
        let Some(start) = cmd_start else { continue };
        let name = base(argv[start]);
        let args: Vec<&str> = argv[start + 1..].to_vec();
        analyze_command(name, &args, &mut out);
    }

    out.targets.sort();
    out.targets.dedup();
    out.removals.sort();
    out.removals.dedup();
    out
}

fn analyze_command(name: &str, args: &[&str], out: &mut Analysis) {
    let files = |args: &[&str]| -> Vec<String> {
        args.iter().filter(|a| !is_flag(a)).map(|a| a.to_string()).collect()
    };

    match name {
        "tee" => out.targets.extend(files(args)),
        "sed" | "gsed" | "perl-sed" => {
            // Only -i (in place) writes. BSD sed takes -i ''.
            if args.iter().any(|a| *a == "-i" || a.starts_with("-i")) {
                // Drop the script and any empty backup suffix; the rest are files.
                let mut cands = files(args);
                if !cands.is_empty() {
                    cands.remove(0); // the sed script
                }
                cands.retain(|c| !c.is_empty());
                out.targets.extend(cands);
            }
        }
        "cp" | "mv" | "install" | "ln" | "rsync" => {
            let f = files(args);
            if let Some(dst) = f.last() {
                out.targets.push(dst.clone());
            }
            // A move empties its sources. `cp` does not, and `mv a b` where b
            // is a directory is a move of `a` all the same.
            if name == "mv" && f.len() >= 2 {
                for src in &f[..f.len() - 1] {
                    out.removals.push((src.clone(), true));
                }
            }
            if name == "rsync" {
                out.audit = true;
            }
        }
        "rm" | "unlink" => {
            let f = files(args);
            out.removals.extend(f.iter().map(|p| (p.clone(), false)));
            out.targets.extend(f);
        }
        "touch" | "truncate" | "mkdir" | "chmod" | "chown" => {
            out.targets.extend(files(args))
        }
        "dd" => {
            for a in args {
                if let Some(p) = a.strip_prefix("of=") {
                    out.targets.push(p.to_string());
                }
            }
        }
        "awk" | "gawk" => {
            // awk can redirect internally: awk '{print > "f"}'
            if args.iter().any(|a| a.contains('>')) {
                out.audit = true;
            }
        }
        "git" => {
            let sub = args.iter().find(|a| !is_flag(a)).copied().unwrap_or("");
            if !GIT_READ_ONLY.contains(&sub) {
                out.audit = true; // checkout/apply/restore/reset/stash/clean/...
            }
            // `git rm` and `git mv` are the versions an agent reaches for in a
            // repository, and they remove paths as surely as the shell ones.
            match sub {
                "rm" => {
                    let f: Vec<String> = files(args).into_iter().skip(1).collect();
                    out.removals.extend(f.iter().map(|p| (p.clone(), false)));
                    out.targets.extend(f);
                }
                "mv" => {
                    let f: Vec<String> = files(args).into_iter().skip(1).collect();
                    if f.len() >= 2 {
                        out.targets.push(f[f.len() - 1].clone());
                        for src in &f[..f.len() - 1] {
                            out.removals.push((src.clone(), true));
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {
            if OPAQUE.contains(&name) {
                out.audit = true;
            } else if !READ_ONLY.contains(&name) {
                out.audit = true; // unknown command: assume it might write
            }
        }
    }
}

#[cfg(test)]
mod tests {

    fn removals(cmd: &str) -> Vec<(String, bool)> {
        analyze(cmd).removals
    }

    /// A deletion is gated like a write — it is one — but it is also the half
    /// of a conflict a claim cannot express, so it has to be recognisable as a
    /// deletion and not merely as a target.
    #[test]
    fn a_delete_is_both_a_target_and_a_removal() {
        assert_eq!(t("rm src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(removals("rm src/auth.js"), vec![("src/auth.js".to_string(), false)]);
        assert_eq!(removals("rm -rf src/legacy"), vec![("src/legacy".to_string(), false)]);
        assert_eq!(removals("unlink src/a.js"), vec![("src/a.js".to_string(), false)]);
    }

    /// A move empties its source. `cp` does not, and mistaking one for the
    /// other would announce a deletion that never happened.
    #[test]
    fn a_move_removes_its_source_and_a_copy_does_not() {
        assert_eq!(removals("mv src/a.js src/b.js"), vec![("src/a.js".to_string(), true)]);
        assert!(t("mv src/a.js src/b.js").contains(&"src/b.js".to_string()));
        assert!(removals("cp src/a.js src/b.js").is_empty());
    }

    /// `git rm` and `git mv` are what an agent reaches for inside a
    /// repository, and the sub-command must not be mistaken for a path.
    #[test]
    fn git_rm_and_git_mv_are_removals_too() {
        assert_eq!(removals("git rm src/old.js"), vec![("src/old.js".to_string(), false)]);
        assert_eq!(removals("git mv src/a.js src/b.js"), vec![("src/a.js".to_string(), true)]);
        assert!(t("git mv src/a.js src/b.js").contains(&"src/b.js".to_string()));
        assert!(!removals("git rm src/old.js").iter().any(|(p, _)| p == "rm"));
    }

    #[test]
    fn a_command_that_removes_nothing_reports_nothing() {
        for cmd in ["cat src/a.js", "echo hi > src/b.js", "sed -i '' s/a/b/ src/c.js", "touch x"] {
            assert!(removals(cmd).is_empty(), "{cmd} removes nothing");
        }
    }
    use super::*;

    fn t(cmd: &str) -> Vec<String> {
        analyze(cmd).targets
    }
    fn audit(cmd: &str) -> bool {
        analyze(cmd).audit
    }

    #[test]
    fn redirects_are_targets() {
        assert_eq!(t("echo hi > src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("echo hi >> src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("echo hi >src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("cmd 2> log.txt"), vec!["log.txt"]);
    }

    #[test]
    fn heredoc_write_is_caught_via_its_redirect() {
        assert_eq!(t("cat > src/auth.js <<'EOF'\nhi\nEOF"), vec!["src/auth.js"]);
    }

    #[test]
    fn quoted_gt_is_not_a_redirect() {
        assert!(t("echo 'a > b'").is_empty());
        assert!(t("echo \"a > b\"").is_empty());
        assert!(t("grep -r 'x -> y' src").is_empty());
    }

    #[test]
    fn fd_dup_is_not_a_file() {
        assert!(t("cmd >&2").is_empty());
        assert!(t("cmd 2>&1").is_empty());
    }

    #[test]
    fn dev_null_is_ignored() {
        assert!(t("cmd > /dev/null").is_empty());
        assert!(t("cmd 2>/dev/null").is_empty());
    }

    #[test]
    fn tee_targets() {
        assert_eq!(t("echo x | tee src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("echo x | tee -a src/auth.js"), vec!["src/auth.js"]);
    }

    #[test]
    fn sed_in_place_only() {
        assert_eq!(t("sed -i '' 's/a/b/' src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("sed -i 's/a/b/' src/auth.js"), vec!["src/auth.js"]);
        assert!(t("sed 's/a/b/' src/auth.js").is_empty(), "read-only sed must not claim");
        assert!(!audit("sed 's/a/b/' src/auth.js"));
    }

    #[test]
    fn copy_and_move_target_the_destination() {
        assert_eq!(t("cp /tmp/x src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("mv old.js src/auth.js"), vec!["src/auth.js"]);
    }

    #[test]
    fn destructive_file_commands() {
        assert_eq!(t("rm src/auth.js"), vec!["src/auth.js"]);
        assert_eq!(t("touch src/new.js"), vec!["src/new.js"]);
        assert_eq!(t("dd if=/dev/zero of=src/auth.js"), vec!["src/auth.js"]);
    }

    #[test]
    fn read_only_pipelines_need_no_audit() {
        for cmd in [
            "ls -la src",
            "cat src/auth.js",
            "grep -rn token src | head -20",
            "rg --files-with-matches session",
            "git status --porcelain",
            "git diff HEAD -- src/auth.js",
            "wc -l src/*.js | sort -n",
            "find . -name '*.js' -type f",
        ] {
            assert!(!audit(cmd), "should be provably read-only: {cmd}");
            assert!(t(cmd).is_empty(), "should claim nothing: {cmd}");
        }
    }

    #[test]
    fn interpreters_and_unknowns_require_audit() {
        for cmd in [
            "python3 fix.py",
            "node script.js",
            "make build",
            "cargo fmt",
            "npm run lint",
            "./some-custom-tool",
            "git checkout src/auth.js",
            "git apply patch.diff",
            "xargs sed -i s/a/b/",
        ] {
            assert!(audit(cmd), "should require audit: {cmd}");
        }
    }

    #[test]
    fn command_substitution_forces_audit() {
        assert!(audit("cat $(ls src)"));
        assert!(audit("echo `date`"));
    }

    #[test]
    fn multiple_segments_are_all_analyzed() {
        let a = analyze("cat src/a.js && echo x > src/b.js; rm src/c.js");
        assert_eq!(a.targets, vec!["src/b.js", "src/c.js"]);
    }

    #[test]
    fn env_assignments_do_not_hide_the_command() {
        assert_eq!(t("FOO=bar echo hi > src/auth.js"), vec!["src/auth.js"]);
        assert!(audit("FOO=bar python3 x.py"));
    }

    #[test]
    fn heredoc_bodies_are_not_shell() {
        // Observed live: an arrow function in a heredoc body claimed a file
        // called `sum`, and a comparison claimed `SESSION_TTL_MS`.
        let a = analyze(
            "cat > /tmp/t.js <<'EOF'\n             const total = items.reduce((sum, i) => sum + i, 0);\n             if (Date.now() - s.createdAt > SESSION_TTL_MS) return null;\n             EOF",
        );
        assert_eq!(a.targets, vec!["/tmp/t.js"], "only the real redirect counts: {a:?}");
    }

    #[test]
    fn heredoc_with_unquoted_delimiter_is_also_skipped() {
        let a = analyze("cat > out.txt <<EOF\nx > bogus\nEOF");
        assert_eq!(a.targets, vec!["out.txt"]);
    }

    #[test]
    fn multiple_heredocs_are_each_skipped() {
        let a = analyze(
            "cat > a.txt <<'A'\nx > nope1\nA\ncat > b.txt <<'B'\ny > nope2\nB",
        );
        assert_eq!(a.targets, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn arrow_functions_are_never_redirects() {
        assert!(t("node -e 'const f = (a, b) => a + b'").is_empty());
        assert!(t("echo const f = x => x").is_empty());
    }

    #[test]
    fn commands_after_a_heredoc_still_parse() {
        let a = analyze("cat > a.txt <<'EOF'\nbody\nEOF\nrm src/gone.js");
        assert_eq!(a.targets, vec!["a.txt", "src/gone.js"]);
    }

    #[test]
    fn awk_internal_redirect_is_audited() {
        assert!(audit("awk '{print > \"out.txt\"}' in.txt"));
        assert!(!audit("awk '{print $1}' in.txt"));
    }
}

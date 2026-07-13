//! V17 Phase B — strict whole-file-read command parser.
//!
//! [`whole_file_read`] returns `Some(path)` only when a shell command is
//! *provably* a pure, whole-file read of exactly one file — the shell
//! equivalent of a `Read({file_path})` with no offset/limit. It lets the read
//! advisor's `PreToolUse` **Bash** hook (`read_hook.rs`) intercept a
//! `cat`/`Get-Content` of an already-read file the same way it intercepts a
//! `Read`, and lets the bypass canary (`GraphService::check_bypass`) skip a
//! command the advisor already accounted for.
//!
//! It is deliberately *strict* and *new* — NOT the V16 bypass tap's
//! `path_like_tokens` heuristic, which stays a broad tokenizer for bypass
//! matching. Interception must never be a guess: any pipe, redirection,
//! command substitution, separator, second path, glob, unknown flag, or
//! partial-read verb (`sed`/`head`/`tail`) yields `None`, and the command runs
//! untouched. When in doubt, reject.

/// `Some(path)` iff `command` is provably a pure whole-file read of ONE file.
/// The returned path is the sole argument with surrounding quotes stripped;
/// the caller resolves a relative path against the hook payload's cwd.
pub fn whole_file_read(command: &str) -> Option<String> {
    let tokens = tokenize(command)?;
    let mut it = tokens.iter();
    let verb = it.next()?;
    if !verb_ok(verb) {
        return None;
    }
    let mut path: Option<&str> = None;
    for tok in it {
        if let Some(rest) = tok.strip_prefix('-') {
            // Only `-Raw` (PowerShell, case-insensitive) is tolerated; every
            // other flag means this isn't a plain whole-file read.
            if rest.eq_ignore_ascii_case("Raw") {
                continue;
            }
            return None;
        }
        // A non-flag token is a path argument — exactly one is allowed.
        if path.is_some() {
            return None;
        }
        path = Some(tok);
    }
    let path = path?;
    // No empty path, and no glob metacharacters (an expansion would read more
    // than the one named file — not provably a single Read).
    if path.is_empty() || path.chars().any(|c| matches!(c, '*' | '?' | '[')) {
        return None;
    }
    Some(path.to_string())
}

/// The verb set: `cat`/`type` match exactly (lowercase shell builtins); the
/// PowerShell pair `Get-Content`/`gc` matches case-insensitively.
fn verb_ok(verb: &str) -> bool {
    verb == "cat"
        || verb == "type"
        || verb.eq_ignore_ascii_case("Get-Content")
        || verb.eq_ignore_ascii_case("gc")
}

/// Split `command` into whitespace-separated tokens with surrounding quotes
/// stripped, returning `None` if any shell metacharacter appears UNQUOTED — a
/// pipe/redirection/separator (`| > < ; &`), command substitution (`$` / `` ` ``),
/// grouping (`(` `)`), or a raw newline — all of which make the command more
/// than a single whole-file read. An unterminated quote is also rejected.
fn tokenize(command: &str) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut has_tok = false; // a token has begun (possibly empty via `""`)
    let mut in_single = false;
    let mut in_double = false;
    for c in command.chars() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                has_tok = true;
            }
            '"' => {
                in_double = true;
                has_tok = true;
            }
            // Unquoted metacharacters ⇒ not a single whole-file read. This is a
            // strict superset of the spec's reject list (`| > < ; && || ` $(`):
            // rejecting bare `&`, `$`, `(`, `)` too can only turn a would-be
            // interception into a harmless pass-through, never a wrong deny.
            '|' | '>' | '<' | ';' | '&' | '`' | '$' | '(' | ')' | '\n' | '\r' => return None,
            c if c.is_whitespace() => {
                if has_tok {
                    tokens.push(std::mem::take(&mut cur));
                    has_tok = false;
                }
            }
            c => {
                cur.push(c);
                has_tok = true;
            }
        }
    }
    if in_single || in_double {
        return None; // unterminated quote ⇒ malformed
    }
    if has_tok {
        tokens.push(cur);
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::whole_file_read;

    #[test]
    fn accepts_plain_whole_file_reads() {
        assert_eq!(whole_file_read("cat p").as_deref(), Some("p"));
        assert_eq!(whole_file_read("type p").as_deref(), Some("p"));
        assert_eq!(whole_file_read("Get-Content p").as_deref(), Some("p"));
        // -Raw tolerated (either arg order), case-insensitive verb + flag.
        assert_eq!(whole_file_read("gc -Raw p").as_deref(), Some("p"));
        assert_eq!(whole_file_read("gc p -raw").as_deref(), Some("p"));
        assert_eq!(whole_file_read("get-content p").as_deref(), Some("p"));
        assert_eq!(whole_file_read("cat src/a.rs").as_deref(), Some("src/a.rs"));
        // Windows-style path with backslashes stays literal.
        assert_eq!(whole_file_read("cat C:\\x\\y.txt").as_deref(), Some("C:\\x\\y.txt"));
    }

    #[test]
    fn accepts_quoted_paths_with_spaces() {
        assert_eq!(whole_file_read("cat \"my file.txt\"").as_deref(), Some("my file.txt"));
        assert_eq!(whole_file_read("Get-Content 'a b/c d.log'").as_deref(), Some("a b/c d.log"));
        assert_eq!(whole_file_read("cat \"C:\\Program Files\\x.txt\"").as_deref(), Some("C:\\Program Files\\x.txt"));
    }

    #[test]
    fn rejects_pipes_and_redirects() {
        assert_eq!(whole_file_read("cat a | grep x"), None);
        assert_eq!(whole_file_read("cat a > b"), None);
        assert_eq!(whole_file_read("cat a >> b"), None);
        assert_eq!(whole_file_read("cat a < b"), None);
        assert_eq!(whole_file_read("cat a || cat b"), None);
    }

    #[test]
    fn rejects_separators_and_substitution() {
        assert_eq!(whole_file_read("cat a && cat b"), None);
        assert_eq!(whole_file_read("cat a ; cat b"), None);
        assert_eq!(whole_file_read("cat $(which x)"), None);
        assert_eq!(whole_file_read("cat `x`"), None);
        assert_eq!(whole_file_read("cat $HOME/x"), None);
    }

    #[test]
    fn rejects_multiple_paths_and_globs() {
        assert_eq!(whole_file_read("cat a b"), None);
        assert_eq!(whole_file_read("cat *.rs"), None);
        assert_eq!(whole_file_read("cat src/*.rs"), None);
        assert_eq!(whole_file_read("cat a?.txt"), None);
        assert_eq!(whole_file_read("cat a[0].txt"), None);
    }

    #[test]
    fn rejects_partial_read_verbs_and_unknown_flags() {
        assert_eq!(whole_file_read("sed -n 5,10p f"), None);
        assert_eq!(whole_file_read("head -50 f"), None);
        assert_eq!(whole_file_read("tail -n 20 f"), None);
        // Wrong verb, and a tolerated-flag spelling on a rejected verb.
        assert_eq!(whole_file_read("grep x f"), None);
        // cat with a non-Raw flag is a partial/altered read.
        assert_eq!(whole_file_read("cat -n f"), None);
    }

    #[test]
    fn rejects_malformed_and_empty() {
        assert_eq!(whole_file_read(""), None);
        assert_eq!(whole_file_read("cat"), None); // no path
        assert_eq!(whole_file_read("cat \"unterminated"), None);
        assert_eq!(whole_file_read("cat \"\""), None); // empty path
    }
}

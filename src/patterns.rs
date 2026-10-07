//! rsync-style exclude patterns.
//!
//! * `build` : a file or directory called `build` at any depth
//! * `build/intermediates` : that relative path at any depth (`app/build/intermediates` too)
//! * `/local.properties` : leading slash anchors the pattern at the project root
//! * `*.log` : glob; `*` never crosses `/`
//! * `!build/intermediates/apk_ide_redirect_file` : keep this even though a pattern above excludes it

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

#[derive(Clone)]
pub struct Matcher {
    exclude: GlobSet,
    include: GlobSet,
    /// Every ancestor of an include (`**/build`, `**/build/intermediates`, …): the walk must
    /// descend into these even when they are excluded, so that the include can be reached.
    include_ancestors: GlobSet,
}

fn glob(g: &str, p: &str) -> Result<globset::Glob> {
    GlobBuilder::new(g)
        .literal_separator(true)
        .build()
        .with_context(|| format!("bad pattern `{p}`"))
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let mut exclude = GlobSetBuilder::new();
        let mut include = GlobSetBuilder::new();
        let mut ancestors = GlobSetBuilder::new();
        for raw in patterns {
            let raw = raw.trim();
            let (negated, p) = match raw.strip_prefix('!') {
                Some(p) => (true, p),
                None => (false, raw),
            };
            let p = p.trim().trim_end_matches('/');
            if p.is_empty() {
                continue;
            }
            let (prefix, body) = match p.strip_prefix('/') {
                Some(anchored) => ("", anchored),
                None => ("**/", p),
            };
            let set = if negated { &mut include } else { &mut exclude };
            set.add(glob(&format!("{prefix}{body}"), p)?);
            set.add(glob(&format!("{prefix}{body}/**"), p)?);
            if negated {
                let mut prefix_so_far = String::new();
                for part in body.split('/') {
                    if !prefix_so_far.is_empty() {
                        prefix_so_far.push('/');
                    }
                    prefix_so_far.push_str(part);
                    ancestors.add(glob(&format!("{prefix}{prefix_so_far}"), p)?);
                }
            }
        }
        Ok(Self {
            exclude: exclude.build()?,
            include: include.build()?,
            include_ancestors: ancestors.build()?,
        })
    }

    /// `rel` is `/`-separated and relative to the project root.
    pub fn excluded(&self, rel: &str) -> bool {
        self.exclude.is_match(rel) && !self.include.is_match(rel)
    }

    /// Excluded and nothing under it can be included: the walk can skip the whole subtree.
    pub fn skip_subtree(&self, rel: &str) -> bool {
        self.excluded(rel) && !self.include_ancestors.is_match(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_patterns_match_at_any_depth() {
        let m = Matcher::new(&["build".into(), ".git".into(), "*.log".into()]).unwrap();
        assert!(m.excluded("build"));
        assert!(m.excluded("app/build"));
        assert!(m.excluded("app/build/outputs/x.apk"));
        assert!(m.excluded(".git/HEAD"));
        assert!(m.excluded("app/src/debug.log"));
        assert!(!m.excluded("app/src/main/Build.kt"));
        assert!(!m.excluded("buildSrc/x.kt"));
        assert!(m.skip_subtree("app/build"));
    }

    #[test]
    fn path_patterns_match_at_any_depth_unless_anchored() {
        let m = Matcher::new(&["build/intermediates".into(), "/local.properties".into()]).unwrap();
        assert!(m.excluded("build/intermediates"));
        assert!(m.excluded("app/build/intermediates"));
        assert!(m.excluded("app/build/intermediates/a/b"));
        assert!(!m.excluded("app/build/outputs"));
        assert!(m.excluded("local.properties"));
        assert!(!m.excluded("app/local.properties"));
    }

    #[test]
    fn negated_pattern_keeps_a_path_inside_an_excluded_dir() {
        let m = Matcher::new(&["build/intermediates".into(), "!build/intermediates/apk_ide_redirect_file".into()]).unwrap();
        assert!(!m.excluded("app/build/intermediates/apk_ide_redirect_file"));
        assert!(!m.excluded("app/build/intermediates/apk_ide_redirect_file/debug/redirect.txt"));
        assert!(m.excluded("app/build/intermediates/dex/x.dex"));
        assert!(m.excluded("app/build/intermediates/some.txt"));
        // the walk must enter the excluded parents on the way to the include, and nothing else
        assert!(!m.skip_subtree("app/build/intermediates"));
        assert!(m.skip_subtree("app/build/intermediates/dex"));
        assert!(!m.skip_subtree("app/build/intermediates/apk_ide_redirect_file"));
    }

    #[test]
    fn negated_anchored_pattern() {
        let m = Matcher::new(&["/build".into(), "!/build/keep".into()]).unwrap();
        assert!(m.excluded("build/x"));
        assert!(!m.excluded("build/keep/y"));
        assert!(!m.skip_subtree("build"));
        assert!(!m.excluded("app/build/x")); // the anchored exclude never applied here
    }

    fn m(patterns: &[&str]) -> Matcher {
        Matcher::new(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn trailing_slash_whitespace_and_empty_patterns_are_tolerated() {
        let a = m(&["build/ ", "", "  ", " /local.properties", " ! keep/ "]);
        for p in ["build", "app/build", "app/build/x", "local.properties"] {
            assert!(a.excluded(p), "{p}");
        }
        assert!(!a.excluded("app/local.properties"));
        assert!(!a.excluded("build/keep"));
        let empty = m(&["", "  ", "/", "!"]);
        assert!(!empty.excluded("anything"));
        assert!(!empty.excluded("a/b"));
    }

    #[test]
    fn star_never_crosses_a_slash() {
        let a = m(&["*.apk", "app/*.aab"]);
        assert!(a.excluded("x.apk"));
        assert!(a.excluded("a/b/x.apk"));
        assert!(a.excluded("app/x.aab"));
        assert!(a.excluded("mod/app/x.aab"));
        assert!(!a.excluded("app/x/y.aab"));
        assert!(!a.excluded("x.apk.txt"));
    }

    #[test]
    fn explicit_double_star_and_question_mark_globs_work() {
        let a = m(&["**/generated", "?.tmp", "/src/**/*.bak"]);
        assert!(a.excluded("generated"));
        assert!(a.excluded("app/build/generated/R.java"));
        assert!(a.excluded("a.tmp"));
        assert!(a.excluded("dir/b.tmp"));
        assert!(!a.excluded("ab.tmp"));
        assert!(!a.excluded(".tmp"));
        assert!(a.excluded("src/x.bak"));
        assert!(a.excluded("src/a/b/x.bak"));
        assert!(!a.excluded("app/src/x.bak"));
    }

    #[test]
    fn a_bad_glob_is_an_error_naming_the_pattern() {
        let err = Matcher::new(&["build".into(), "[abc".into()]).err().expect("must fail");
        assert!(format!("{err:#}").contains("bad pattern `[abc`"), "{err:#}");
        let err = Matcher::new(&["!keep/[x".into()]).err().expect("must fail");
        assert!(format!("{err:#}").contains("keep/[x"), "{err:#}");
    }

    #[test]
    fn an_include_without_a_matching_exclude_excludes_nothing() {
        let a = m(&["!keep"]);
        for p in ["keep", "keep/x", "other", "a/keep"] {
            assert!(!a.excluded(p), "{p}");
            assert!(!a.skip_subtree(p), "{p}");
        }
    }

    #[test]
    fn an_excluded_directory_is_skipped_unless_an_include_lies_below_it() {
        let plain = m(&["build"]);
        assert!(plain.excluded("build"));
        assert!(plain.skip_subtree("build"));

        let deep = m(&["build", "!build/a/b/keep"]);
        assert!(deep.excluded("build"));
        for p in ["build", "build/a", "build/a/b", "app/build", "app/build/a/b"] {
            assert!(!deep.skip_subtree(p), "ancestor {p}");
        }
        for p in ["build/c", "build/a/c", "build/a/b/other", "app/build/c"] {
            assert!(deep.skip_subtree(p), "sibling {p}");
        }
        assert!(!deep.excluded("build/a/b/keep"));
        assert!(!deep.excluded("app/build/a/b/keep/x.txt"));
        assert!(deep.excluded("build/a/b/other"));
        // an ancestor is still excluded itself: only its subtree walk is kept
        assert!(deep.excluded("build/a"));
    }

    #[test]
    fn matcher_is_clone() {
        fn assert_clone<T: Clone>(_: &T) {}
        let a = m(&["build"]);
        assert_clone(&a);
        assert!(a.clone().excluded("build"));
    }
}

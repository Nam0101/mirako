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
}

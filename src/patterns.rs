//! rsync-style exclude patterns.
//!
//! * `build` : a file or directory called `build` at any depth
//! * `build/intermediates` : that relative path at any depth (`app/build/intermediates` too)
//! * `/local.properties` : leading slash anchors the pattern at the project root
//! * `*.log` : glob; `*` never crosses `/`

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

#[derive(Clone)]
pub struct Matcher {
    set: GlobSet,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            let p = p.trim().trim_end_matches('/');
            if p.is_empty() {
                continue;
            }
            let globs = match p.strip_prefix('/') {
                Some(anchored) => vec![anchored.to_string(), format!("{anchored}/**")],
                None => vec![format!("**/{p}"), format!("**/{p}/**")],
            };
            for g in globs {
                b.add(
                    GlobBuilder::new(&g)
                        .literal_separator(true)
                        .build()
                        .with_context(|| format!("bad pattern `{p}`"))?,
                );
            }
        }
        Ok(Self { set: b.build()? })
    }

    /// `rel` is `/`-separated and relative to the project root.
    pub fn excluded(&self, rel: &str) -> bool {
        self.set.is_match(rel)
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
}

//! Rewrites the remote project path to the local one in the build output, line by line,
//! so IDE error links and file paths keep pointing at this machine.

pub struct LineRewriter {
    from: Vec<u8>,
    to: Vec<u8>,
    /// `to` as it stands behind `file://`
    to_url: Vec<u8>,
    buf: Vec<u8>,
}

impl LineRewriter {
    pub fn new(from: &str, to: &str) -> Self {
        // a Windows path is `/C:/…` in a URL: kotlinc prints `file:///C:/…`, the host `file:///Users/…`
        let to_url = if to.starts_with('/') {
            to.to_string()
        } else {
            format!("/{}", to.replace('\\', "/"))
        };
        Self {
            from: from.as_bytes().to_vec(),
            to: to.as_bytes().to_vec(),
            to_url: to_url.into_bytes(),
            buf: Vec::new(),
        }
    }

    /// Returns the rewritten complete lines; a trailing partial line is kept until the next call.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(data);
        let Some(last_nl) = self.buf.iter().rposition(|&b| b == b'\n') else {
            // no newline yet: flush anyway if the buffer is big (progress bars without newlines)
            if self.buf.len() > 64 * 1024 {
                let out = self.replace(&self.buf.clone());
                self.buf.clear();
                return out;
            }
            return Vec::new();
        };
        let complete: Vec<u8> = self.buf.drain(..=last_nl).collect();
        self.replace(&complete)
    }

    pub fn flush(&mut self) -> Vec<u8> {
        let rest = std::mem::take(&mut self.buf);
        self.replace(&rest)
    }

    fn replace(&self, data: &[u8]) -> Vec<u8> {
        if self.from.is_empty() || self.from == self.to {
            return data.to_vec();
        }
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i..].starts_with(&self.from) {
                let to = if out.ends_with(b"file://") { &self.to_url } else { &self.to };
                out.extend_from_slice(to);
                i += self.from.len();
            } else {
                out.push(data[i]);
                i += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_across_chunks() {
        let mut r = LineRewriter::new("/Users/admin/mirako/app", "/Users/me/app");
        let mut out = r.feed(b"e: /Users/admin/mirako/app/src/A.kt: boom\nnext /Users/adm");
        out.extend(r.feed(b"in/mirako/app/x\n"));
        out.extend(r.flush());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "e: /Users/me/app/src/A.kt: boom\nnext /Users/me/app/x\n"
        );
    }

    fn all(r: &mut LineRewriter, chunks: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.feed(c));
        }
        out.extend(r.flush());
        out
    }

    #[test]
    fn a_chunk_without_a_newline_is_held_back_until_flush() {
        let mut r = LineRewriter::new("/remote", "/local");
        assert!(r.feed(b"at /remote/a.kt").is_empty());
        assert!(r.feed(b" still going").is_empty());
        assert_eq!(r.flush(), b"at /local/a.kt still going");
        assert!(r.flush().is_empty());
    }

    #[test]
    fn only_complete_lines_are_returned_and_the_tail_waits() {
        let mut r = LineRewriter::new("/remote", "/local");
        assert_eq!(r.feed(b"1 /remote\n2 /rem"), b"1 /local\n");
        assert_eq!(r.feed(b"ote\n3"), b"2 /local\n");
        assert_eq!(r.flush(), b"3");
    }

    #[test]
    fn more_than_64_kib_without_a_newline_is_flushed_anyway_and_rewritten() {
        let mut r = LineRewriter::new("/remote", "/local");
        let mut big = b"/remote ".to_vec();
        big.resize(64 * 1024 + 1, b'.');
        let out = r.feed(&big);
        assert!(out.starts_with(b"/local "));
        assert_eq!(out.len(), big.len() - 1);
        assert!(r.flush().is_empty());
    }

    #[test]
    fn a_path_split_by_the_64_kib_forced_flush_is_not_rewritten() {
        // current behaviour: the forced flush does not keep a possible prefix of `from` back
        let mut r = LineRewriter::new("/remote", "/local");
        let mut big = vec![b'.'; 64 * 1024 - 2];
        big.extend_from_slice(b"/rem");
        let mut out = r.feed(&big);
        out.extend(r.feed(b"ote\n"));
        assert!(out.ends_with(b"/remote\n"));
    }

    #[test]
    fn identical_or_empty_from_passes_bytes_through_unchanged() {
        let data: &[u8] = &[0xff, 0xfe, b'/', b'a', b'\n', 0x80, 0x00];
        assert_eq!(all(&mut LineRewriter::new("/a", "/a"), &[data]), data);
        assert_eq!(all(&mut LineRewriter::new("", "/x"), &[data]), data);
    }

    #[test]
    fn non_utf8_bytes_around_a_match_survive() {
        let mut r = LineRewriter::new("/remote", "/local");
        assert_eq!(
            all(&mut r, &[&[0xff, b'/', b'r'], b"emote", &[0x80, b'\n']]),
            [&[0xffu8][..], b"/local", &[0x80, b'\n']].concat()
        );
    }

    #[test]
    fn every_occurrence_on_a_line_is_replaced() {
        let mut r = LineRewriter::new("/r/p", "/l");
        assert_eq!(all(&mut r, &[b"/r/p/a /r/p/b:/r/p/r/p\n"]), b"/l/a /l/b:/l/l\n");
    }

    #[test]
    fn a_windows_root_becomes_a_slash_path_inside_a_file_url_only() {
        let mut r = LineRewriter::new("/Users/admin/mirako/app", r"C:\Users\me\app");
        let out = all(
            &mut r,
            &[b"e: file:///Users/admin/mirako/app/src/A.kt:3:1 boom\n/Users/admin/mirako/app/B.java:7: error\n"],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "e: file:///C:/Users/me/app/src/A.kt:3:1 boom\nC:\\Users\\me\\app/B.java:7: error\n"
        );
        // a Unix root reads the same in both places
        assert_eq!(
            all(&mut LineRewriter::new("/r", "/l"), &[b"file:///r/a /r/b\n"]),
            b"file:///l/a /l/b\n"
        );
    }

    #[test]
    fn a_prefix_at_a_chunk_boundary_that_does_not_match_is_kept_as_is() {
        let mut r = LineRewriter::new("/Users/admin", "/Users/me");
        assert_eq!(
            all(&mut r, &[b"x /Users/adm", b"ission /Users/admin\n"]),
            b"x /Users/admission /Users/me\n"
        );
    }

    #[test]
    fn replacement_longer_or_shorter_than_the_original() {
        assert_eq!(
            all(&mut LineRewriter::new("/a", "/much/longer/path"), &[b"/a/x\n"]),
            b"/much/longer/path/x\n"
        );
        assert_eq!(
            all(&mut LineRewriter::new("/much/longer/path", "/a"), &[b"/much/longer/path/x\n"]),
            b"/a/x\n"
        );
        assert_eq!(all(&mut LineRewriter::new("/a", ""), &[b"/a/x\n"]), b"/x\n");
    }
}

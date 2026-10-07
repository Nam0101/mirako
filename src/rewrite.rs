//! Rewrites the remote project path to the local one in the build output, line by line,
//! so IDE error links and file paths keep pointing at this machine.

pub struct LineRewriter {
    from: Vec<u8>,
    to: Vec<u8>,
    buf: Vec<u8>,
}

impl LineRewriter {
    pub fn new(from: &str, to: &str) -> Self {
        Self {
            from: from.as_bytes().to_vec(),
            to: to.as_bytes().to_vec(),
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
                out.extend_from_slice(&self.to);
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
}

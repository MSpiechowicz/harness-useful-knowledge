//! Compact plain-text replies: size caps and dates.

use crate::support::time::rfc3339;

/// Room kept for the `… (n more)` trailer.
const TRAILER_ROOM: usize = 24;

/// Blocks of text joined by newlines, never longer than a byte budget. Blocks that do not fit are
/// counted and reported as `… (n more)`.
pub struct Capped {
    text: String,
    max: usize,
    omitted: usize,
}

impl Capped {
    /// An empty reply of at most `max` bytes.
    pub fn new(max: usize) -> Self {
        Self {
            text: String::new(),
            max,
            omitted: 0,
        }
    }

    /// Adds `block` whole, or counts it as omitted once anything was omitted or it does not fit.
    /// Says whether the block was added.
    pub fn push(&mut self, block: &str) -> bool {
        if self.omitted > 0 || !self.fits(block.len()) {
            self.omitted += 1;
            return false;
        }

        if !self.text.is_empty() {
            self.text.push('\n');
        }

        self.text.push_str(block);

        true
    }

    /// The reply text.
    pub fn finish(mut self) -> String {
        if self.omitted > 0 {
            if !self.text.is_empty() {
                self.text.push('\n');
            }

            self.text.push_str(&format!("… ({} more)", self.omitted));
        }

        self.text
    }

    fn fits(&self, length: usize) -> bool {
        let separator = usize::from(!self.text.is_empty());

        self.text.len() + separator + length + TRAILER_ROOM <= self.max
    }
}

/// `YYYY-MM-DD` (UTC) of Unix seconds.
pub fn date(seconds: i64) -> String {
    rfc3339(seconds).chars().take(10).collect()
}

/// `text` cut to at most `max` bytes at a character boundary, ending with `…` when it was cut.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }

    let mut end = max.saturating_sub('…'.len_utf8());

    while !text.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_reports_what_it_left_out() {
        let mut reply = Capped::new(60);

        assert!(reply.push("first block"));
        assert!(!reply.push(&"x".repeat(80)));
        assert!(!reply.push("small"));
        assert_eq!(reply.finish(), "first block\n… (2 more)");
    }

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("żółw żółw żółw", 9), "żół…");
        assert_eq!(date(1_791_158_400), "2026-10-05");
    }
}

//! The smallest pieces the chunker packs: a text cut into [`Atom`]s of at most one budget of
//! tokens each, along the most natural boundary that is small enough.
//!
//! A text is cut into blocks (blank lines, but a fenced code block stays whole). A block over the
//! budget is cut into lines (so a long code block splits on lines), a line into sentences, a
//! sentence into runs of words, and a single word that is still over the budget into runs of
//! characters. The atoms cover the text in order, so a run of them is one slice of it.

use crate::tokens::TokenCounter;

/// A slice of the text, as byte offsets, with its token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Atom {
    pub start: usize,
    pub end: usize,
    pub tokens: usize,
}

/// Cuts `text` into atoms of at most `budget` tokens (at least 1).
pub fn split(text: &str, budget: usize, counter: &dyn TokenCounter) -> Vec<Atom> {
    let mut cutter = Cutter {
        text,
        budget: budget.max(1),
        counter,
        atoms: Vec::new(),
    };

    for (start, end) in blocks(text) {
        cutter.block(start, end);
    }

    cutter.atoms
}

struct Cutter<'a> {
    text: &'a str,
    budget: usize,
    counter: &'a dyn TokenCounter,
    atoms: Vec<Atom>,
}

impl Cutter<'_> {
    fn count(&self, start: usize, end: usize) -> usize {
        self.counter.count(&self.text[start..end])
    }

    /// Pushes `start..end` whole when it fits, else cuts it with `finer`.
    fn fit(&mut self, start: usize, end: usize, finer: fn(&mut Self, usize, usize)) {
        let tokens = self.count(start, end);

        if tokens <= self.budget {
            self.atoms.push(Atom { start, end, tokens });
        } else {
            finer(self, start, end);
        }
    }

    fn block(&mut self, start: usize, end: usize) {
        self.fit(start, end, Self::lines);
    }

    fn lines(&mut self, start: usize, end: usize) {
        let mut line_start = start;

        for line in self.text[start..end].split_inclusive('\n') {
            let line_end = line_start + line.len();
            let (from, to) = trimmed(self.text, line_start, line_end);

            if from < to {
                self.fit(from, to, Self::sentences);
            }

            line_start = line_end;
        }
    }

    fn sentences(&mut self, start: usize, end: usize) {
        let mut from = start;
        let mut chars = self.text[start..end].char_indices().peekable();

        while let Some((offset, character)) = chars.next() {
            let ends_sentence = matches!(character, '.' | '!' | '?' | '。')
                && chars.peek().is_some_and(|(_, next)| next.is_whitespace());

            if ends_sentence {
                let to = start + offset + character.len_utf8();
                self.fit(from, to, Self::words);

                // The next sentence starts after the whitespace.
                from = to + self.text[to..end].len() - self.text[to..end].trim_start().len();
            }
        }

        if from < end {
            self.fit(from, end, Self::words);
        }
    }

    /// Runs of words of at most the budget. A word is counted alone and a run by the sum of its
    /// words, then counted as a whole (the space between words counts too) and shortened until it
    /// fits.
    fn words(&mut self, start: usize, end: usize) {
        let words = word_spans(self.text, start, end);
        let alone: Vec<usize> = words.iter().map(|(from, to)| self.count(*from, *to)).collect();
        let mut first = 0;

        while first < words.len() {
            let mut last = first + 1;
            let mut sum = alone[first];

            while last < words.len() && sum + alone[last] <= self.budget {
                sum += alone[last];
                last += 1;
            }

            let mut tokens = self.count(words[first].0, words[last - 1].1);

            while tokens > self.budget && last > first + 1 {
                last -= 1;
                tokens = self.count(words[first].0, words[last - 1].1);
            }

            if tokens > self.budget {
                self.characters(words[first].0, words[first].1);
            } else {
                self.atoms.push(Atom {
                    start: words[first].0,
                    end: words[last - 1].1,
                    tokens,
                });
            }

            first = last;
        }
    }

    /// Runs of characters, for a word that has more tokens than the budget.
    fn characters(&mut self, start: usize, end: usize) {
        let mut from = start;
        let mut take = self.budget.saturating_mul(3).max(1);

        while from < end {
            let mut to = advance(self.text, from, end, take);

            // Shorten the run until it fits (a single character always does).
            while take > 1 && self.count(from, to) > self.budget {
                take = (take * 3 / 4).max(1);
                to = advance(self.text, from, end, take);
            }

            let tokens = self.count(from, to);
            self.atoms.push(Atom {
                start: from,
                end: to,
                tokens,
            });
            from = to;
        }
    }
}

/// The words of `text[start..end]` as byte ranges.
fn word_spans(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut word_start: Option<usize> = None;

    for (offset, character) in text[start..end].char_indices() {
        match (character.is_whitespace(), word_start) {
            (true, Some(from)) => {
                spans.push((start + from, start + offset));
                word_start = None;
            }
            (false, None) => word_start = Some(offset),
            _ => {}
        }
    }

    if let Some(from) = word_start {
        spans.push((start + from, end));
    }

    spans
}

/// `start + length` characters, but not past `end`.
fn advance(text: &str, start: usize, end: usize, length: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(length)
        .map_or(end, |(offset, _)| start + offset)
}

/// `start..end` without the whitespace at both ends.
pub fn trimmed(text: &str, start: usize, end: usize) -> (usize, usize) {
    let slice = &text[start..end];
    let head = slice.len() - slice.trim_start().len();
    let tail = slice.trim_end().len();

    if tail <= head {
        (start, start)
    } else {
        (start + head, start + tail)
    }
}

/// The blocks of `text`: separated by blank lines, a fenced code block (with its blank lines) one.
fn blocks(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut block_start = 0;
    let mut fence: Option<char> = None;
    let mut line_start = 0;

    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let trimmed_line = line.trim();

        match fence {
            Some(mark) => {
                if trimmed_line.starts_with(&mark.to_string().repeat(3)) && trimmed_line.chars().all(|c| c == mark) {
                    fence = None;
                }
            }
            None => {
                let mark = ['`', '~']
                    .into_iter()
                    .find(|mark| trimmed_line.starts_with(&mark.to_string().repeat(3)));

                if mark.is_some() {
                    fence = mark;
                } else if trimmed_line.is_empty() {
                    push_block(text, &mut found, block_start, line_start);
                    block_start = line_end;
                }
            }
        }

        line_start = line_end;
    }

    push_block(text, &mut found, block_start, text.len());

    found
}

fn push_block(text: &str, found: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    let (from, to) = trimmed(text, start, end);

    if from < to {
        found.push((from, to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::ApproxCounter;

    fn pieces(text: &str, budget: usize) -> Vec<String> {
        split(text, budget, &ApproxCounter)
            .iter()
            .map(|atom| text[atom.start..atom.end].to_owned())
            .collect()
    }

    #[test]
    fn blocks_split_on_blank_lines_and_fences_stay_whole() {
        let text = "one\n\ntwo\n\n```rust\nlet a = 1;\n\nlet b = 2;\n```\n\nthree";

        assert_eq!(
            pieces(text, 100),
            ["one", "two", "```rust\nlet a = 1;\n\nlet b = 2;\n```", "three"]
        );
    }

    #[test]
    fn an_oversized_code_block_splits_on_lines() {
        let code = (0..30)
            .map(|n| format!("let value_{n} = {n};"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("```\n{code}\n```");
        let atoms = pieces(&text, 20);

        assert!(atoms.len() > 10);
        assert!(atoms.iter().all(|atom| ApproxCounter.count(atom) <= 20));
        assert!(atoms.iter().any(|atom| atom == "let value_7 = 7;"));
    }

    #[test]
    fn a_long_paragraph_splits_into_sentences_then_words_then_characters() {
        let sentences = "First sentence is here. Second sentence follows it. Third one ends the paragraph.";
        assert_eq!(
            pieces(sentences, 10),
            [
                "First sentence is here.",
                "Second sentence follows it.",
                "Third one ends the paragraph."
            ]
        );

        let words = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let runs = pieces(words, 4);
        assert!(runs.len() > 2);
        assert!(runs.iter().all(|run| ApproxCounter.count(run) <= 4), "{runs:?}");
        assert_eq!(runs.join(" "), words);

        let blob = "x".repeat(200);
        let cut = pieces(&blob, 6);
        assert!(cut.len() > 5);
        assert!(cut.iter().all(|piece| ApproxCounter.count(piece) <= 6));
        assert_eq!(cut.concat(), blob);
    }

    #[test]
    fn atoms_are_ordered_and_never_overlap() {
        let text = "Intro line.\n\nSecond paragraph with a few more words in it to be cut. And one more sentence.";
        let atoms = split(text, 6, &ApproxCounter);

        assert!(atoms.windows(2).all(|pair| pair[0].end <= pair[1].start));
        assert!(atoms.iter().all(|atom| atom.tokens <= 6 && atom.start < atom.end));
    }
}

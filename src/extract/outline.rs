//! Collects the blocks an extractor reads into [`Section`]s, keeping the stack of headings.

use super::{Section, heading_text};

/// Builds the sections of one document: call [`Outline::heading`] when a heading starts and
/// [`Outline::block`] or [`Outline::item`] for the text under it.
pub struct Outline {
    /// The headings every section starts with (the breadcrumbs of a Confluence page).
    prefix: Vec<String>,
    /// The open headings: level and text.
    open: Vec<(usize, String)>,
    sections: Vec<Section>,
    current: Section,
    /// True when the last thing added was a list item or table row, which the next one continues.
    in_list: bool,
    first_title: Option<String>,
}

impl Outline {
    /// An outline whose every heading path starts with `prefix`.
    pub fn new(prefix: Vec<String>) -> Self {
        Self {
            current: Section {
                heading_path: prefix.clone(),
                level: 0,
                text: String::new(),
            },
            prefix,
            open: Vec::new(),
            sections: Vec::new(),
            in_list: false,
            first_title: None,
        }
    }

    /// Starts the section of a heading of `level` (1 is the outermost). Open headings of the same
    /// or a deeper level end. A blank heading changes nothing.
    pub fn heading(&mut self, level: usize, text: &str) {
        let text = heading_text(text);

        if text.is_empty() {
            return;
        }

        self.finish_current();

        while self.open.last().is_some_and(|(open, _)| *open >= level) {
            self.open.pop();
        }

        if level == 1 && self.first_title.is_none() {
            self.first_title = Some(text.clone());
        }

        self.open.push((level, text));

        let mut path = self.prefix.clone();
        path.extend(self.open.iter().map(|(_, heading)| heading.clone()));
        self.current = Section {
            heading_path: path,
            level,
            text: String::new(),
        };
    }

    /// Adds a paragraph (or any block) under the current heading.
    pub fn block(&mut self, text: &str) {
        let text = text.trim();

        if text.is_empty() {
            return;
        }

        self.separate();
        self.current.text.push_str(text);
        self.in_list = false;
    }

    /// Adds a line that belongs with the lines around it: a list item or a table row. Consecutive
    /// items are one block.
    pub fn item(&mut self, line: &str) {
        let line = line.trim_end();

        if line.trim().is_empty() {
            return;
        }

        if self.in_list {
            self.current.text.push('\n');
        } else {
            self.separate();
        }

        self.current.text.push_str(line);
        self.in_list = true;
    }

    /// The text of the first level 1 heading, once seen.
    pub fn first_title(&self) -> Option<&str> {
        self.first_title.as_deref()
    }

    /// The sections, those without text left out.
    pub fn finish(mut self) -> Vec<Section> {
        self.finish_current();
        self.sections
    }

    fn separate(&mut self) {
        if !self.current.text.is_empty() {
            self.current.text.push_str("\n\n");
        }
    }

    fn finish_current(&mut self) {
        self.in_list = false;

        let next = Section {
            heading_path: self.current.heading_path.clone(),
            level: self.current.level,
            text: String::new(),
        };
        let done = std::mem::replace(&mut self.current, next);

        if !done.text.trim().is_empty() {
            self.sections.push(done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_nest_and_close_by_level() {
        let mut outline = Outline::new(Vec::new());
        outline.block("intro");
        outline.heading(1, "Guide");
        outline.heading(2, "Install");
        outline.block("steps");
        outline.heading(3, "Linux");
        outline.block("apt");
        outline.heading(2, "Use");
        outline.block("run");
        outline.heading(1, "Appendix");
        outline.block("end");

        let paths: Vec<(Vec<String>, usize)> = outline
            .finish()
            .into_iter()
            .map(|section| (section.heading_path, section.level))
            .collect();
        let path = |parts: &[&str], level| (parts.iter().map(|part| (*part).to_owned()).collect::<Vec<_>>(), level);

        assert_eq!(
            paths,
            [
                path(&[], 0),
                path(&["Guide", "Install"], 2),
                path(&["Guide", "Install", "Linux"], 3),
                path(&["Guide", "Use"], 2),
                path(&["Appendix"], 1),
            ]
        );
    }

    #[test]
    fn items_group_into_one_block_and_a_prefix_leads_every_path() {
        let mut outline = Outline::new(vec!["Space".to_owned()]);
        outline.block("intro");
        outline.item("- a");
        outline.item("- b");
        outline.block("after");
        outline.heading(2, "Part");
        outline.item("x | y");

        let sections = outline.finish();

        assert_eq!(sections[0].heading_path, ["Space"]);
        assert_eq!(sections[0].text, "intro\n\n- a\n- b\n\nafter");
        assert_eq!(sections[1].heading_path, ["Space", "Part"]);
        assert_eq!(sections[1].text, "x | y");
    }

    #[test]
    fn empty_sections_and_blank_headings_vanish() {
        let mut outline = Outline::new(Vec::new());
        outline.heading(1, "Empty");
        outline.heading(1, "   ");
        outline.heading(2, "Child");
        outline.block("text");

        assert_eq!(outline.first_title(), Some("Empty"));

        let sections = outline.finish();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading_path, ["Empty", "Child"]);
    }
}

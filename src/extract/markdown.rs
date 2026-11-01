//! Markdown (CommonMark with tables, task lists and a front matter block).
//!
//! Raw HTML is dropped. Links keep their text, code blocks become fenced blocks, a list is one block
//! with a line per item and a table one block with a line per row.

use pulldown_cmark::{CodeBlockKind, Event, MetadataBlockKind, Options, Parser, Tag, TagEnd};

use super::{Extracted, Outline, heading_text};

pub fn extract(text: &str) -> Extracted {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut reader = Reader::default();

    for event in Parser::new_ext(text, options) {
        reader.event(event);
    }

    reader.finish()
}

#[derive(Default)]
struct Reader {
    outline: Option<Outline>,
    /// The inline text of the block being read.
    text: String,
    heading: Option<usize>,
    /// The open lists: the number the next item of an ordered list gets.
    lists: Vec<Option<u64>>,
    /// The marker of each open item, until a line uses it.
    markers: Vec<Option<String>>,
    /// The lines of the outermost list being read.
    lines: Vec<String>,
    code: Option<(String, String)>,
    table: Option<Vec<String>>,
    row: Vec<String>,
    metadata: Option<String>,
    front_title: Option<String>,
}

impl Reader {
    fn outline(&mut self) -> &mut Outline {
        self.outline.get_or_insert_with(|| Outline::new(Vec::new()))
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                self.text.push('`');
                self.text.push_str(&code);
                self.text.push('`');
            }
            Event::SoftBreak => self.text.push(' '),
            Event::HardBreak => self.text.push('\n'),
            Event::TaskListMarker(done) => self.text.push_str(if done { "[x] " } else { "[ ] " }),
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else if let Some(metadata) = &mut self.metadata {
            metadata.push_str(text);
        } else {
            self.text.push_str(text);
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_paragraph();
                self.heading = Some(level as usize);
            }
            Tag::Paragraph => {
                if !self.lists.is_empty() && !self.text.is_empty() {
                    self.text.push(' ');
                }
            }
            Tag::List(first) => {
                // The text of the item before a nested list is a line of its own.
                self.flush_item_text();

                if self.lists.is_empty() {
                    self.flush_paragraph();
                }

                self.lists.push(first);
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "- ".to_owned(),
                };

                self.markers.push(Some(marker));
            }
            Tag::CodeBlock(kind) => {
                self.flush_item_text();

                let language = match kind {
                    CodeBlockKind::Fenced(language) => language.split_whitespace().next().unwrap_or("").to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };

                self.code = Some((language, String::new()));
            }
            Tag::Table(_) => {
                self.flush_paragraph();
                self.table = Some(Vec::new());
            }
            Tag::TableHead | Tag::TableRow => self.row.clear(),
            Tag::TableCell => self.text.clear(),
            Tag::MetadataBlock(MetadataBlockKind::YamlStyle | MetadataBlockKind::PlusesStyle) => {
                self.metadata = Some(String::new());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                let text = std::mem::take(&mut self.text);

                if let Some(level) = self.heading.take() {
                    self.outline().heading(level, &text);
                }
            }
            TagEnd::Paragraph => {
                if self.lists.is_empty() {
                    self.flush_paragraph();
                }
            }
            TagEnd::Item => {
                self.flush_item_text();
                self.markers.pop();
            }
            TagEnd::List(_) => {
                self.lists.pop();

                if self.lists.is_empty() {
                    let block = std::mem::take(&mut self.lines).join("\n");
                    self.outline().block(&block);
                }
            }
            TagEnd::CodeBlock => self.end_code(),
            TagEnd::TableCell => {
                let cell = heading_text(&std::mem::take(&mut self.text));
                self.row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let line = self.row.join(" | ");

                if let Some(table) = &mut self.table {
                    table.push(line);
                }
            }
            TagEnd::Table => {
                let rows = self.table.take().unwrap_or_default().join("\n");
                self.outline().block(&rows);
            }
            TagEnd::MetadataBlock(_) => {
                if let Some(metadata) = self.metadata.take() {
                    self.front_title = front_matter_title(&metadata);
                }
            }
            _ => {}
        }
    }

    /// A paragraph outside a list is a block.
    fn flush_paragraph(&mut self) {
        let text = std::mem::take(&mut self.text);

        if self.lists.is_empty() {
            self.outline().block(&text);
        }
    }

    /// The text of the open item becomes a line, led by the item's marker (once).
    fn flush_item_text(&mut self) {
        if self.lists.is_empty() {
            return;
        }

        let text = std::mem::take(&mut self.text);

        if text.trim().is_empty() {
            return;
        }

        let indent = "  ".repeat(self.lists.len() - 1);
        let marker = self.markers.last_mut().and_then(Option::take);
        let lead = marker.unwrap_or_else(|| "  ".to_owned());

        self.lines.push(format!("{indent}{lead}{}", text.trim()));
    }

    fn end_code(&mut self) {
        let Some((language, body)) = self.code.take() else {
            return;
        };

        let body = body.trim_end_matches('\n');
        let fence = if body.contains("```") { "````" } else { "```" };
        let block = format!("{fence}{language}\n{body}\n{fence}");

        if self.lists.is_empty() {
            self.outline().block(&block);
        } else {
            self.lines.push(block);
        }
    }

    fn finish(mut self) -> Extracted {
        self.flush_paragraph();

        let outline = self.outline.take().unwrap_or_else(|| Outline::new(Vec::new()));
        let title = self.front_title.or_else(|| outline.first_title().map(str::to_owned));

        Extracted {
            title,
            sections: outline.finish(),
            ..Extracted::default()
        }
    }
}

/// The `title:` of a front matter block.
fn front_matter_title(metadata: &str) -> Option<String> {
    metadata.lines().find_map(|line| {
        let value = line.strip_prefix("title:")?.trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'').trim();

        (!value.is_empty()).then(|| heading_text(value))
    })
}

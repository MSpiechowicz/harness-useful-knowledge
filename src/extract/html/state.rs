//! The reading state of the HTML walker: the open elements and the text being collected (see the
//! parent module for what they make of a page).

use std::collections::HashMap;

use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{Tag, TokenSinkResult};

use crate::extract::{Extracted, Outline, heading_text};

/// Elements whose content is not documentation.
const SKIPPED: [&str; 8] = [
    "script", "style", "noscript", "template", "svg", "iframe", "nav", "footer",
];

/// Elements that end the paragraph being read.
const BLOCKS: [&str; 24] = [
    "p",
    "div",
    "section",
    "article",
    "blockquote",
    "ul",
    "ol",
    "dl",
    "dt",
    "dd",
    "figure",
    "figcaption",
    "details",
    "summary",
    "main",
    "header",
    "address",
    "fieldset",
    "hr",
    "body",
    "table",
    "aside",
    "form",
    "li",
];

/// The most elements open at once. Real pages nest a few dozen deep.
const DEPTH_MAX: usize = 512;

const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
];

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum Role {
    None,
    /// `title`: the page title in the head.
    Title,
    /// `#main-content` of a Confluence page.
    Main,
    /// `ol#breadcrumbs` of a Confluence page.
    Crumbs,
    /// `#title-text` of a Confluence page.
    PageTitle,
}

#[derive(Clone)]
struct Open {
    name: String,
    skip: bool,
    role: Role,
}

pub(super) struct State {
    confluence: bool,
    stack: Vec<Open>,
    /// How many open elements have each [`Role`] (indexed by it).
    roles: [usize; 5],
    /// How many `li` elements are open.
    items: usize,
    /// How many elements of each name are open, so an end tag that closes nothing costs nothing.
    open_names: HashMap<String, usize>,
    skipped: usize,
    main: usize,
    outline: Outline,
    /// The paragraph being read.
    paragraph: String,
    heading: Option<(usize, String)>,
    pre: Option<String>,
    row: Option<Vec<String>>,
    cell: Option<String>,
    title: String,
    page_title: String,
    crumbs: Vec<String>,
    crumb: String,
}

impl State {
    pub(super) fn new(confluence: bool) -> Self {
        Self {
            confluence,
            stack: Vec::new(),
            roles: [0; 5],
            items: 0,
            open_names: HashMap::new(),
            skipped: 0,
            main: 0,
            outline: Outline::new(Vec::new()),
            paragraph: String::new(),
            heading: None,
            pre: None,
            row: None,
            cell: None,
            title: String::new(),
            page_title: String::new(),
            crumbs: Vec::new(),
            crumb: String::new(),
        }
    }

    /// True while text is document content.
    fn accepting(&self) -> bool {
        self.skipped == 0 && (!self.confluence || self.main > 0)
    }

    fn inside(&self, role: Role) -> bool {
        self.roles[role as usize] > 0
    }

    pub(super) fn text(&mut self, text: &str) {
        if self.inside(Role::Title) {
            self.title.push_str(text);
        } else if self.inside(Role::PageTitle) {
            self.page_title.push_str(text);
        } else if self.inside(Role::Crumbs) {
            self.crumb.push_str(text);
        } else if self.accepting() {
            self.content_text(text);
        }
    }

    fn content_text(&mut self, text: &str) {
        if let Some(pre) = &mut self.pre {
            pre.push_str(text);
        } else if let Some(cell) = &mut self.cell {
            cell.push_str(text);
        } else if let Some((_, heading)) = &mut self.heading {
            heading.push_str(text);
        } else {
            self.paragraph.push_str(text);
        }
    }

    pub(super) fn start(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;

        // A body implies the end of a head that was never closed.
        if name == "body" {
            self.end("head");
        }

        if VOID.contains(&name) {
            self.void(name);
            return TokenSinkResult::Continue;
        }

        let id = attribute(tag, "id");
        let class = attribute(tag, "class");
        let role = self.role_of(name, &id);
        let skip = SKIPPED.contains(&name) || self.skipped_by_name(&id, &class);

        if tag.self_closing {
            return TokenSinkResult::Continue;
        }

        let raw = raw_data(name);

        // An element too deep is ignored. A raw one (`script`, `style`) is kept: its text must stay
        // out of the document, and it cannot nest.
        if self.stack.len() >= DEPTH_MAX && raw.is_none() {
            return TokenSinkResult::Continue;
        }

        self.stack.push(Open {
            name: name.to_owned(),
            skip,
            role,
        });
        self.roles[role as usize] += 1;
        *self.open_names.entry(name.to_owned()).or_default() += 1;

        if name == "li" {
            self.items += 1;
        }

        if skip {
            self.skipped += 1;
        }

        if role == Role::Main {
            self.main += 1;
        }

        if self.accepting() {
            self.start_content(name);
        }

        if role == Role::Crumbs {
            self.crumb.clear();
        }

        if name == "li" && self.inside(Role::Crumbs) {
            self.finish_crumb();
        }

        raw.map_or(TokenSinkResult::Continue, TokenSinkResult::RawData)
    }

    fn role_of(&self, name: &str, id: &str) -> Role {
        match (name, id) {
            ("title", _) => Role::Title,
            (_, "main-content") => Role::Main,
            ("ol", "breadcrumbs") if self.confluence => Role::Crumbs,
            (_, "title-text") if self.confluence => Role::PageTitle,
            _ => Role::None,
        }
    }

    /// Navigation clutter named by id or class (`breadcrumbs`, `navigation`, `page-metadata`).
    fn skipped_by_name(&self, id: &str, class: &str) -> bool {
        let named = |word: &str| id == word || class.split_whitespace().any(|part| part == word);

        let clutter = named("navigation") || named("page-metadata") || named("breadcrumb") || named("footer");

        // The breadcrumbs of a Confluence page are read, the others are clutter.
        clutter || (!self.confluence && named("breadcrumbs"))
    }

    fn void(&mut self, name: &str) {
        if !self.accepting() {
            return;
        }

        match name {
            "br" => self.content_text(" "),
            "hr" => self.flush_paragraph(),
            _ => {}
        }
    }

    fn start_content(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_paragraph();
                self.heading = Some((usize::from(name.as_bytes()[1] - b'0'), String::new()));
            }
            "pre" => {
                self.flush_paragraph();
                self.pre = Some(String::new());
            }
            "tr" => {
                self.close_row();
                self.flush_paragraph();
                self.row = Some(Vec::new());
            }
            "td" | "th" => {
                self.close_cell();
                self.cell = Some(String::new());
            }
            other if BLOCKS.contains(&other) => self.flush_paragraph(),
            _ => {}
        }
    }

    pub(super) fn end(&mut self, name: &str) {
        if !self.open_names.contains_key(name) {
            return;
        }

        let Some(at) = self.stack.iter().rposition(|open| open.name == name) else {
            return;
        };

        while self.stack.len() > at {
            self.pop();
        }
    }

    /// Closes the innermost open element. Its content ends while it is still on the stack, so a
    /// list item's last paragraph is still a list line.
    fn pop(&mut self) {
        let Some(open) = self.stack.last().cloned() else {
            return;
        };

        if self.accepting() && !open.skip {
            self.close_content(&open.name);
        }

        if open.role == Role::Crumbs || (open.name == "li" && self.inside(Role::Crumbs)) {
            self.finish_crumb();
        }

        self.stack.pop();
        self.roles[open.role as usize] -= 1;

        if let Some(count) = self.open_names.get_mut(&open.name) {
            *count -= 1;

            if *count == 0 {
                self.open_names.remove(&open.name);
            }
        }

        if open.name == "li" {
            self.items -= 1;
        }

        if open.skip {
            self.skipped -= 1;
        }

        if open.role == Role::Main {
            self.main -= 1;
        }
    }

    fn close_content(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if let Some((level, text)) = self.heading.take() {
                    self.outline.heading(level, &text);
                }
            }
            "pre" => self.close_pre(),
            "td" | "th" => self.close_cell(),
            "tr" | "table" => {
                self.close_row();
                self.flush_paragraph();
            }
            other if BLOCKS.contains(&other) => self.flush_paragraph(),
            _ => {}
        }
    }

    fn finish_crumb(&mut self) {
        let crumb = heading_text(&std::mem::take(&mut self.crumb));

        if !crumb.is_empty() {
            self.crumbs.push(crumb);
        }
    }

    fn close_pre(&mut self) {
        let Some(code) = self.pre.take() else {
            return;
        };

        let code = code.trim_matches('\n').trim_end();

        if code.trim().is_empty() {
            return;
        }

        let fence = if code.contains("```") { "````" } else { "```" };
        self.outline.block(&format!("{fence}\n{code}\n{fence}"));
    }

    fn close_cell(&mut self) {
        if let Some(cell) = self.cell.take() {
            let cell = heading_text(&cell);

            if let Some(row) = &mut self.row {
                row.push(cell);
            }
        }
    }

    fn close_row(&mut self) {
        self.close_cell();

        if let Some(row) = self.row.take() {
            self.outline.item(&row.join(" | "));
        }
    }

    /// The paragraph read so far goes to the outline: as a list line inside a list item.
    fn flush_paragraph(&mut self) {
        let text = std::mem::take(&mut self.paragraph);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");

        if text.is_empty() {
            return;
        }

        if self.items > 0 {
            self.outline.item(&format!("- {text}"));
        } else {
            self.outline.block(&text);
        }
    }

    pub(super) fn finish(mut self) -> Extracted {
        while !self.stack.is_empty() {
            self.pop();
        }

        self.close_row();
        self.flush_paragraph();

        let first_title = self.outline.first_title().map(str::to_owned);
        let mut sections = self.outline.finish();

        for section in &mut sections {
            section.heading_path.splice(0..0, self.crumbs.iter().cloned());
        }

        let confluence = self.confluence;
        let named = |title: &str| {
            let title = heading_text(title);

            if confluence { unspaced(&title) } else { title }
        };
        let page_title = named(&self.page_title);
        let head_title = named(&self.title);
        let title = [page_title, head_title]
            .into_iter()
            .chain(first_title)
            .find(|title| !title.is_empty());

        Extracted {
            title,
            sections,
            ..Extracted::default()
        }
    }
}

/// A Confluence title is `Space : Page`: the page.
fn unspaced(title: &str) -> String {
    match title.split_once(" : ") {
        Some((_, page)) => page.to_owned(),
        None => title.to_owned(),
    }
}

/// The tokenizer state of an element whose content is raw text, not tags.
fn raw_data(name: &str) -> Option<RawKind> {
    match name {
        "script" => Some(RawKind::ScriptData),
        "style" | "iframe" | "noembed" | "noframes" | "xmp" => Some(RawKind::Rawtext),
        "title" | "textarea" => Some(RawKind::Rcdata),
        _ => None,
    }
}

/// The value of attribute `name`, blank when there is none.
fn attribute(tag: &Tag, name: &str) -> String {
    tag.attrs
        .iter()
        .find(|attribute| &*attribute.name.local == name)
        .map(|attribute| attribute.value.to_string())
        .unwrap_or_default()
}

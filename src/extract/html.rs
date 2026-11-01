//! HTML and the Confluence HTML export.
//!
//! The page is read as a stream of tags and text (the HTML tokenizer, no DOM): the text under
//! each of `h1` to `h6` becomes a section. `script`, `style`, `nav`, `footer` and the like are
//! dropped, `pre` becomes a fenced block, a list item or table row one line.
//!
//! A Confluence page (it has `id="main-content"`) is read from that element alone. The breadcrumbs
//! (`ol#breadcrumbs`) lead every heading path, the title is the page title without its `Space : `
//! prefix, and the page id in the file name (`Title_12345.html`) is the document's source key, so
//! a re-exported page updates the document instead of adding a copy. The export's `index.html`
//! (the list of pages) is skipped.
//!
//! Every step takes time independent of how deeply the tags nest: the open elements are at most
//! 512 (a deeper start tag is ignored, like a stray one), and what the open elements mean
//! (a title, the breadcrumbs, a list item) is counted when they open and close, not searched for.

use std::cell::RefCell;

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, CharacterTokens, EndTag, StartTag, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};

use super::{Extracted, Refusal, Result};

mod state;

use state::State;

pub fn extract(text: &str, name: &str) -> Result<Extracted> {
    let confluence = text.contains("id=\"main-content\"") || text.contains("id='main-content'");

    if !confluence && is_index_page(text, name) {
        return Err(Refusal::Skipped("the index page of a Confluence export".to_owned()));
    }

    let tokenizer = Tokenizer::new(Walker::new(confluence), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(text));

    let _ = tokenizer.feed(&input);
    tokenizer.end();

    let mut extracted = tokenizer.sink.state.into_inner().finish();

    if confluence {
        extracted.source_key = page_id(name).map(|id| format!("confluence:{id}"));
    }

    Ok(extracted)
}

fn is_index_page(text: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();

    (lower == "index.html" || lower == "index.htm") && text.contains("Available Pages")
}

/// The page id of a Confluence export file name: `Page-Title_12345.html` is 12345.
fn page_id(name: &str) -> Option<&str> {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let id = stem.rsplit_once('_').map_or(stem, |(_, id)| id);

    (id.len() >= 3 && id.chars().all(|c| c.is_ascii_digit())).then_some(id)
}

struct Walker {
    state: RefCell<State>,
}

impl Walker {
    fn new(confluence: bool) -> Self {
        Self {
            state: RefCell::new(State::new(confluence)),
        }
    }
}

impl TokenSink for Walker {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut state = self.state.borrow_mut();

        match token {
            CharacterTokens(text) => state.text(&text),
            TagToken(tag) => match tag.kind {
                StartTag => return state.start(&tag),
                EndTag => state.end(&tag.name),
            },
            _ => {}
        }

        TokenSinkResult::Continue
    }
}

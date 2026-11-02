//! An import whose files replace documents of their own (the manifest's `replacing`): each file
//! is saved in the place of the document it replaces, so one import can change documents of both
//! places. The events are one stream, files of the remote database first and then those of this
//! computer.

use std::io::Write;

use super::import::author_of;
use super::{Context, Failure};
use crate::backend::Access;
use crate::config::{Kind, Layout, Place};
use crate::extract::pdf::PdfPages;
use crate::import::events::Events;
use crate::import::manifest::{self, Manifest};
use crate::import::place::write_place;
use crate::import::walk::{self, Candidate};
use crate::import::{Embedder, Importer, Progress, Report};
use crate::store::routed::ids;
use crate::store::{KnowledgeStore, clean};
use crate::tokens::counter_for;

/// The files for one place, with the document each replaces (the id its store knows).
struct Turn<'a> {
    place: Place,
    store: Box<dyn KnowledgeStore>,
    files: Vec<(&'a Candidate, Option<i64>)>,
}

/// Runs the import of `manifest`, see the module docs. Everything that can be refused (a place
/// that is not in use, a remote that cannot be reached) is before the first file.
pub(super) fn import_each_place(
    context: &Context,
    manifest: &Manifest,
    pdf: &dyn PdfPages,
    mut embedder: Option<Box<dyn Embedder>>,
    cancel: &dyn Fn() -> bool,
    out: &mut dyn Write,
) -> Result<Report, Failure> {
    let links = clean::links(&manifest.links)?;
    let counter = counter_for(context.env);
    let candidates = walk::plan(manifest).map_err(Failure::Failed)?;
    let replacing = manifest.replacing_by_path();
    let kind = Kind::of(links.len());
    let mut wanted: Vec<(Place, &Candidate, Option<i64>)> = Vec::new();

    for candidate in &candidates {
        let replaced = replacing.get(&manifest::canonical(&candidate.path)).copied();
        let place = write_place(context.routing, manifest.place, replaced, kind).map_err(Failure::Failed)?;

        wanted.push((place, candidate, replaced.map(|id| ids::split(id).1)));
    }

    let mut turns = Vec::new();

    for place in context.routing.places() {
        let files: Vec<_> = wanted
            .iter()
            .filter(|(own, ..)| *own == place)
            .map(|(_, candidate, replaces)| (*candidate, *replaces))
            .collect();

        if files.is_empty() {
            continue;
        }

        let store = context
            .open_place_to_change(place, Access::Create)?
            .ok_or_else(|| Failure::Failed("the database could not be created".to_owned()))?;

        turns.push(Turn { place, store, files });
    }

    let author = author_of(context, &links);
    // Ids are written with their place only when two places are in use.
    let both = context.routing.layout() == Layout::Both;
    let mut events = Events::new(out);
    let mut progress = Progress::new(candidates.len(), &mut events);

    for turn in &mut turns {
        events.set_place(both.then_some(turn.place));

        let mut importer = Importer::new(turn.store.as_mut(), counter.as_ref(), pdf, cancel);
        importer.embedder = embedder.as_deref_mut().map(|embedder| embedder as &mut dyn Embedder);
        importer.links.clone_from(&links);
        importer.author.clone_from(&author);

        for (candidate, replaces) in &turn.files {
            if !progress.file(&mut importer, candidate, *replaces, &mut events) {
                break;
            }
        }

        if progress.cancelled() {
            break;
        }
    }

    Ok(progress.finish(counter.name(), &mut events))
}

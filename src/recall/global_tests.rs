//! The last place of a recall in a known repository goes to the best global document.

use std::sync::atomic::Ordering;

use super::tests::{PROMPT, answering, counting, down, guide, repo_world, world};
use super::*;
use crate::store::testing::{Seed, angled, axis};

/// The degrees whose cosine with the query vector is `cosine`.
fn degrees_of(cosine: f64) -> f32 {
    cosine.acos().to_degrees() as f32
}

/// The degrees whose cosine with the query vector is exactly the floor of the global place.
fn floor_degrees() -> f32 {
    degrees_of(rank::RECALL_GLOBAL_SLOT_COSINE)
}

/// A document linked to `repo`.
fn linked(title: &str, degrees: f32, repo: &str) -> Seed {
    guide(title, degrees).link(repo, &[])
}

/// The titles of the pointers in `block`, in order.
fn titles(block: &str) -> Vec<&str> {
    block
        .lines()
        .filter(|line| line.starts_with("<knowledge-doc "))
        .filter_map(|line| line.split("title=\"").nth(1)?.split('"').next())
        .collect()
}

/// Three documents linked to the repository, close to the query, and a global one a little less so.
fn three_linked_and_a_global(repo: &str, global_degrees: f32) -> Vec<Seed> {
    vec![
        linked("Linked one", 2.0, repo),
        linked("Linked two", 3.0, repo),
        linked("Linked three", 4.0, repo),
        guide("Global guide", global_degrees),
    ]
}

#[test]
fn a_global_document_that_is_the_best_match_is_listed_among_close_linked_ones() {
    let (_dir, setup) = repo_world(
        |repo| {
            vec![
                guide("Global guide", 3.0),
                linked("Linked one", 4.0, repo),
                linked("Linked two", 5.0, repo),
                linked("Linked three", 6.0, repo),
                linked("Linked four", 7.0, repo),
            ]
        },
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    assert!(titles(&block).contains(&"Global guide"), "{block}");
}

#[test]
fn the_best_global_document_takes_the_last_place_when_three_linked_ones_fill_the_block() {
    // The global one is the fourth by cosine and under the band of the best pointer.
    let (_dir, setup) = repo_world(|repo| three_linked_and_a_global(repo, 15.0), answering(axis(0)));

    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(titles(&block), ["Linked one", "Linked two", "Global guide"], "{block}");
    assert!(block.len() <= BLOCK_MAX);
}

#[test]
fn the_global_document_is_added_when_there_is_room_and_goes_last() {
    let (_dir, setup) = repo_world(
        |repo| vec![linked("Linked one", 2.0, repo), guide("Global guide", 15.0)],
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(titles(&block), ["Linked one", "Global guide"], "{block}");
}

#[test]
fn a_recall_with_a_global_document_among_its_pointers_looks_for_no_other() {
    // The global one is the closest, and the farther one is under the band.
    let (_dir, setup) = repo_world(
        |repo| {
            vec![
                guide("Global guide", 2.0),
                linked("Linked one", 3.0, repo),
                guide("Farther global guide", 15.0),
            ]
        },
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(titles(&block), ["Linked one", "Global guide"], "{block}");
}

#[test]
fn the_query_is_embedded_once_for_both_searches() {
    let (embedder, asked) = counting(axis(0));
    let (_dir, setup) = repo_world(|repo| three_linked_and_a_global(repo, 15.0), embedder);

    let block = pointers(PROMPT, setup).expect("a block");

    assert!(titles(&block).contains(&"Global guide"), "{block}");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn a_global_document_under_the_cosine_floor_is_not_reserved() {
    let without = {
        let (_dir, setup) = repo_world(
            |repo| three_linked_and_a_global(repo, 15.0).into_iter().take(3).collect(),
            answering(axis(0)),
        );
        pointers(PROMPT, setup).expect("a block")
    };

    let (_dir, setup) = repo_world(
        |repo| three_linked_and_a_global(repo, floor_degrees() + 4.0),
        answering(axis(0)),
    );
    assert_eq!(pointers(PROMPT, setup).expect("a block"), without);

    // Above what a recall asks of any pointer, under what it asks of a guaranteed place.
    let between = (rank::RECALL_COSINE_FLOOR + rank::RECALL_GLOBAL_SLOT_COSINE) / 2.0;
    let (_dir, setup) = repo_world(
        |repo| three_linked_and_a_global(repo, degrees_of(between)),
        answering(axis(0)),
    );
    assert_eq!(pointers(PROMPT, setup).expect("a block"), without);

    let (_dir, setup) = repo_world(
        |repo| three_linked_and_a_global(repo, floor_degrees() - 2.0),
        answering(axis(0)),
    );
    let block = pointers(PROMPT, setup).expect("a block");
    assert_eq!(
        titles(&block).last(),
        Some(&"Global guide"),
        "just above the floor it is kept: {block}"
    );
}

#[test]
fn without_any_global_document_the_pointers_are_the_linked_ones() {
    let (_dir, setup) = repo_world(
        |repo| three_linked_and_a_global(repo, 15.0).into_iter().take(3).collect(),
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(titles(&block), ["Linked one", "Linked two", "Linked three"], "{block}");
}

#[test]
fn a_document_linked_to_another_repository_never_appears() {
    let (_dir, setup) = repo_world(
        |repo| {
            vec![
                linked("Other repository guide", 1.0, "git:example.com/other/repository"),
                linked("Linked one", 2.0, repo),
                linked("Linked two", 3.0, repo),
                linked("Linked three", 4.0, repo),
                guide("Global guide", 15.0),
            ]
        },
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    assert!(!block.contains("Other repository"), "{block}");
    assert_eq!(titles(&block).last(), Some(&"Global guide"), "{block}");
}

#[test]
fn with_no_repository_the_global_documents_are_all_there_is_and_one_search_is_made() {
    let (embedder, asked) = counting(axis(0));
    let (dir, mut setup) = world(
        &[
            guide("Global one", 2.0),
            guide("Global two", 3.0),
            guide("Global three", 4.0),
            guide("Global four", 5.0),
            linked("Linked guide", 1.0, "git:example.com/some/repository"),
        ],
        embedder,
    );

    // The home folder is no repository.
    setup.project = Some(dir.path().to_string_lossy().into_owned());

    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(titles(&block), ["Global one", "Global two", "Global three"], "{block}");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn without_the_embedder_the_global_document_is_reserved_by_the_keyword_floor() {
    let (_dir, setup) = repo_world(
        |repo| keyword_seeds(repo, "Restart the zebrafish gateway sometimes."),
        down(),
    );

    let block = pointers("How do I restart the zebrafish gateway?", setup).expect("keyword recall");

    assert_eq!(titles(&block).last(), Some(&"Zebrafish global notes"), "{block}");
    assert_eq!(titles(&block).len(), POINTERS_MAX, "{block}");

    // A global document that shares no word of the question is under the keyword floor.
    let (_dir, setup) = repo_world(|repo| keyword_seeds(repo, "Nothing in common."), down());
    let block = pointers("How do I restart the zebrafish gateway?", setup).expect("keyword recall");

    assert!(!block.contains("Zebrafish global notes"), "{block}");
}

/// A corpus big enough for a rare word to score above the keyword floor: three linked documents that
/// are all about it, and a global one that says `global_text`.
fn keyword_seeds(repo: &str, global_text: &str) -> Vec<Seed> {
    let mut seeds: Vec<Seed> = (0..600)
        .map(|number| {
            Seed::new(&format!("Filler {number}"))
                .chunk("Notes", &format!("filler text number {number} about something else"))
        })
        .collect();

    for number in 1..=3 {
        seeds.push(
            Seed::new(&format!("Zebrafish gateway runbook {number}"))
                .chunk(
                    "Zebrafish gateway restart",
                    "Restart the zebrafish gateway with the zebrafish command.",
                )
                .link(repo, &[]),
        );
    }

    seeds.push(Seed::new("Zebrafish global notes").chunk("Notes", global_text));
    seeds
}

/// A pointer whose title and section are the longest a block shows, every character of which grows
/// when it is written as an attribute.
fn hostile(number: i64, grown: char) -> Pointer {
    Pointer {
        doc_id: number,
        chunk_id: number * 10,
        title: std::iter::repeat_n(grown, 80).collect(),
        section: std::iter::repeat_n(grown, 60).collect(),
        snippet: "x ".repeat(100),
        score: 500.0,
    }
}

#[test]
fn the_longest_titles_and_sections_still_fit_with_the_global_document_in_the_block() {
    // Plain text of the longest size a block shows: two pointers and the global one fit.
    let long: Vec<Pointer> = (1..=3).map(|number| hostile(number, 'T')).collect();
    let global = hostile(4, 'T');
    let (block, kept) = fit_kept(&long, Some(&global)).expect("a block");

    assert!(block.len() <= BLOCK_MAX, "{}", block.len());
    assert_eq!(kept.iter().map(|pointer| pointer.doc_id).collect::<Vec<_>>(), [1, 2, 4]);
    assert_eq!(block.matches("<knowledge-doc ").count(), 3);
}

#[test]
fn a_global_document_too_long_to_fit_beside_the_best_pointer_never_takes_its_place() {
    // Each of these is about 640 bytes once escaped: one alone fits, two do not.
    let growing: Vec<Pointer> = (1..=3).map(|number| hostile(number, '<')).collect();
    let global = hostile(4, '<');

    let (block, kept) = fit_kept(&growing, Some(&global)).expect("a block");

    assert!(block.len() <= BLOCK_MAX, "{}", block.len());
    assert_eq!(kept.iter().map(|pointer| pointer.doc_id).collect::<Vec<_>>(), [1]);
    assert_eq!(block.matches("<knowledge-doc ").count(), 1, "{block}");

    // The same as the usual rule without a global document: the last pointers go.
    let (_, kept) = fit_kept(&growing, None).expect("a block");
    assert_eq!(kept.iter().map(|pointer| pointer.doc_id).collect::<Vec<_>>(), [1]);
}

#[test]
fn a_global_document_that_escaping_makes_long_leaves_the_pointers_their_room() {
    // About 480 bytes once escaped: it fits beside the best pointer, but only by taking everything else.
    let plain: Vec<Pointer> = (1..=3)
        .map(|number| Pointer {
            title: format!("Guide {number}"),
            section: String::new(),
            ..hostile(number, 'T')
        })
        .collect();
    let mut global = hostile(4, 'T');
    global.title = "\"".repeat(60);

    let (_, kept) = fit_kept(&plain, Some(&global)).expect("a block");

    assert_eq!(kept.iter().map(|pointer| pointer.doc_id).collect::<Vec<_>>(), [1, 2, 3]);
}

#[test]
fn the_pointers_after_the_best_one_go_first_to_keep_the_global_document() {
    // Two plain pointers and a global one of this size do not fit, one of them and the global one do.
    let mut wide: Vec<Pointer> = (1..=2).map(|number| hostile(number, 'T')).collect();
    let mut global = hostile(3, 'T');

    for pointer in wide.iter_mut().chain([&mut global]) {
        pointer.title = "<".repeat(60);
    }

    let (block, kept) = fit_kept(&wide, Some(&global)).expect("a block");

    assert!(block.len() <= BLOCK_MAX, "{}", block.len());
    assert_eq!(kept.iter().map(|pointer| pointer.doc_id).collect::<Vec<_>>(), [1, 3]);
}

#[test]
fn a_global_document_that_cannot_fit_even_alone_leaves_the_pointers_as_they_were() {
    let mut huge = hostile(4, '"');
    huge.title = "\"".repeat(80);
    huge.section = "\"".repeat(60);
    let pointers: Vec<Pointer> = (1..=2).map(|number| hostile(number, 'a')).collect();

    let (block, kept) = fit_kept(&pointers, Some(&huge)).expect("a block");

    assert!(block.len() <= BLOCK_MAX, "{}", block.len());
    assert!(kept.iter().all(|pointer| pointer.doc_id != 4));
    assert!(!kept.is_empty());
}

/// A document whose title and section are as long as a block shows and all `<`.
pub(super) fn hostile_seed(number: usize, degrees: f32) -> Seed {
    Seed::new(&format!("{}{number}", "<".repeat(79))).embedded(
        &"<".repeat(60),
        "Run the installer and restart the gateway afterwards.",
        angled(degrees),
    )
}

#[test]
fn hostile_titles_in_a_real_recall_stay_within_the_limit_and_keep_the_best_pointer() {
    let (_dir, setup) = repo_world(
        |repo| {
            let mut seeds: Vec<Seed> = (1..=3)
                .map(|number| hostile_seed(number, 1.0 + number as f32).link(repo, &[]))
                .collect();
            seeds.push(hostile_seed(4, 15.0));
            seeds
        },
        answering(axis(0)),
    );

    let block = pointers(PROMPT, setup).expect("a block");

    // Escaped, two of these do not fit a block: the best linked one stays, the global one cannot take its place.
    assert!(block.len() <= BLOCK_MAX, "{}", block.len());
    assert_eq!(block.matches("<knowledge-doc ").count(), 1, "{block}");
    assert!(block.contains(&format!("{}1\"", "&lt;".repeat(79))), "{block}");
}

#[test]
fn the_first_block_is_kept_when_the_second_search_overruns_the_deadline() {
    let first = || Recalled {
        block: "<block>".to_owned(),
        tally: Tally {
            env: Env::for_tests(std::path::Path::new("/nonexistent")),
            backend: Backend::Sqlite,
            routing: Routing::local(),
            docs: vec![7],
        },
    };

    let kept = within_keeping(Duration::from_millis(150), move |early| {
        early(first());
        std::thread::sleep(Duration::from_secs(3));
        None
    });
    assert_eq!(kept.map(|recalled| recalled.tally.docs), Some(vec![7]));

    // Nothing was handed over: the overrun is nothing, as before.
    let nothing = within_keeping(Duration::from_millis(150), |_| {
        std::thread::sleep(Duration::from_secs(3));
        None
    });
    assert!(nothing.is_none());

    // What finishes in time is the result, not the early block.
    let finished = within_keeping(Duration::from_secs(5), move |early| {
        early(first());
        Some(Recalled {
            block: "<final>".to_owned(),
            ..first()
        })
    });
    assert_eq!(finished.map(|recalled| recalled.block), Some("<final>".to_owned()));
}

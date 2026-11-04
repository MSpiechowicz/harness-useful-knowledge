//! A prompt's pointers never name a document that is not live.

use super::tests::{PROMPT, answering, down, world};
use super::*;
use crate::store::Status;
use crate::store::testing::{Seed, axis};

fn guide(title: &str) -> Seed {
    Seed::new(title).embedded(
        "Install > Linux",
        &format!("Run the {title} installer and restart the gateway afterwards. It writes a service file."),
        axis(0),
    )
}

#[test]
fn drafts_and_pending_versions_are_never_pointed_to() {
    // The hidden documents match the prompt as well as the live one does, by words and by vector.
    let seeds = [
        guide("Gateway live"),
        guide("Gateway draft zebrasecret").status(Status::Draft),
        guide("Gateway waiting zebrasecret").status(Status::Waiting),
        guide("Gateway changes zebrasecret").status(Status::ChangesRequested),
        guide("Gateway pending zebrasecret").pending_of(1),
    ];

    let (_dir, setup) = world(&seeds, answering(axis(0)));
    let block = pointers(PROMPT, setup).expect("a block");

    assert_eq!(block.matches("<knowledge-doc ").count(), 1, "{block}");
    assert!(block.contains("title=\"Gateway live\""), "{block}");
    assert!(!block.contains("zebrasecret"), "{block}");
}

#[test]
fn the_keyword_list_alone_never_points_to_one_either() {
    // A corpus big enough for a rare word to score above the keyword floor (see the recall tests).
    let text = "Restart the zebrafish gateway with the zebrafish command.";
    let mut seeds: Vec<Seed> = (0..600)
        .map(|number| {
            Seed::new(&format!("Filler {number}"))
                .chunk("Notes", &format!("filler text number {number} about something else"))
        })
        .collect();
    seeds.push(Seed::new("Zebrafish gateway runbook").chunk("Zebrafish gateway restart", text));
    seeds.push(
        Seed::new("Zebrafish gateway draft zebrasecret")
            .chunk("Zebrafish gateway restart", text)
            .status(Status::Draft),
    );
    seeds.push(
        Seed::new("Zebrafish gateway waiting zebrasecret")
            .chunk("Zebrafish gateway restart", text)
            .status(Status::Waiting),
    );
    seeds.push(
        Seed::new("Zebrafish gateway pending zebrasecret")
            .chunk("Zebrafish gateway restart", text)
            .pending_of(601),
    );

    let (_dir, setup) = world(&seeds, down());
    let block = pointers("How do I restart the zebrafish gateway?", setup).expect("keyword recall");

    assert_eq!(block.matches("<knowledge-doc ").count(), 1, "{block}");
    assert!(block.contains("Zebrafish gateway runbook"), "{block}");
    assert!(!block.contains("zebrasecret"), "{block}");
}

#[test]
fn nothing_is_recalled_when_every_matching_document_is_hidden() {
    let seeds = [
        guide("Gateway draft").status(Status::Draft),
        guide("Gateway waiting").status(Status::Waiting),
    ];
    let (_dir, setup) = world(&seeds, answering(axis(0)));

    assert_eq!(pointers(PROMPT, setup), None);
}

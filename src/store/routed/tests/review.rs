//! Drafts and their review through a routed store: ids carry their place, the team's approval is
//! read from the remote alone, and a draft is still shown by the search of a person who may see it.

use pretty_assertions::assert_eq;

use super::flaky::Machine;
use super::*;
use crate::store::{
    EventKind, NewComment, ReviewAction, ReviewChange, ReviewReply, Scope, Side, Status, StatusFilter, StoreError,
};

fn change(action: ReviewAction, at: i64) -> ReviewChange {
    ReviewChange::new(action, Some("Bob"), at)
}

fn read(routed: &mut RoutedStore, id: i64) -> Option<crate::store::DocumentDetail> {
    routed
        .get(
            id,
            &Detail {
                drafts: true,
                ..Detail::default()
            },
        )
        .expect("get")
}

#[test]
fn a_pending_version_is_stored_reviewed_and_promoted_in_the_place_of_its_document() {
    for place in [Where::Remote, Where::Local] {
        let mut routed = routed_over(store(), store(), Routing::new(place, place));
        let expected = if place == Where::Remote {
            Place::Remote
        } else {
            Place::Local
        };

        let (doc, chunks, links) = Seed::new("Guide")
            .source("/guide")
            .chunk("Intro", "legacy words")
            .parts();
        let live = routed.put_document(&doc, &chunks, &links).expect("live");
        assert_eq!(ids::place_of(live), expected);

        // The pending version has no links of its own and is kept where its document is.
        let (doc, chunks, links) = Seed::new("Guide v2")
            .source("/guide-2")
            .pending_of(live)
            .chunk("Intro", "modern words")
            .parts();
        let pending = routed.put_document(&doc, &chunks, &links).expect("pending");
        assert_eq!(ids::place_of(pending), expected);

        let row = read(&mut routed, pending).expect("the pending row");
        assert_eq!(row.doc.pending_of, Some(live), "tagged like every id of the place");
        assert!(
            routed.get(pending, &Detail::default()).expect("get").is_none(),
            "an agent never reads it"
        );

        let page = routed.list(&query_with(StatusFilter::All)).expect("list");
        assert_eq!(page.items.len(), 1, "a pending version is not a row of its own");
        assert_eq!(page.items[0].pending.as_ref().map(|version| version.id), Some(pending));
        assert_eq!(
            routed.pending_of(live).expect("pending of").map(|version| version.id),
            Some(pending)
        );

        // Review and promote through the tagged ids.
        let rev = read(&mut routed, pending).expect("pending").doc.review_rev;
        let asked = match routed
            .set_review(pending, rev, &change(ReviewAction::Ask, 5))
            .expect("ask")
        {
            ReviewReply::Done(done) => done,
            other => panic!("{other:?}"),
        };
        assert_eq!((asked.id, asked.status), (pending, Status::Waiting));

        routed
            .add_comments(
                pending,
                Some("Cy"),
                6,
                rev,
                &[NewComment::on_lines("Intro", Side::New, 1, 1, "nit")],
            )
            .expect("comment");
        assert!(routed.put_file(pending, "v2.md", b"bytes").expect("file"));

        let promoted = match routed
            .promote(pending, asked.revision, &change(ReviewAction::Approve, 7))
            .expect("promote")
        {
            ReviewReply::Done(done) => done,
            other => panic!("{other:?}"),
        };
        assert_eq!((promoted.id, promoted.promoted), (live, true));

        assert_eq!(
            routed
                .history(live)
                .expect("history")
                .expect("history")
                .events
                .iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>(),
            [EventKind::Approved, EventKind::Asked, EventKind::UpdateDrafted]
        );
        assert!(
            routed.comments(live).expect("comments").is_empty(),
            "the review is over"
        );
        assert_eq!(
            routed.file(live).expect("file").map(|file| file.name),
            Some("v2.md".to_owned())
        );
        assert_eq!(
            routed.section_texts(live).expect("sections").expect("sections")[0].text,
            "modern words"
        );
        assert!(read(&mut routed, pending).is_none());
        assert_eq!(routed.stats().expect("stats").pending, 0);
    }
}

fn query_with(status: StatusFilter) -> ListQuery {
    ListQuery {
        status,
        ..query(Sort::Title, false, 0, 50)
    }
}

#[test]
fn a_draft_in_a_place_that_shows_its_kind_is_found_by_a_search_that_may_see_drafts() {
    // Linked documents live remotely, global ones on this computer: a draft hit is placed by its links.
    let mut remote = store();
    insert(
        &mut remote,
        &Seed::new("Gateway draft")
            .source("/draft")
            .status(Status::Draft)
            .chunk("Intro", "gateway draft words")
            .link(REPO, &[]),
    );
    insert(
        &mut remote,
        &Seed::new("Gateway hidden")
            .source("/hidden")
            .status(Status::Draft)
            .chunk("Intro", "gateway hidden words"),
    );

    let mut routed = routed_over(store(), remote, Routing::new(Where::Remote, Where::Local));
    let titles = |hits: Vec<crate::store::ChunkHit>| {
        let mut found: Vec<String> = hits.into_iter().map(|hit| hit.title).collect();
        found.sort();
        found
    };

    assert!(
        routed
            .keyword_top_k("gateway", &Scope::everything(), 10)
            .expect("search")
            .is_empty(),
        "agents see no draft"
    );
    assert_eq!(
        titles(
            routed
                .keyword_top_k("gateway", &Scope::everything().with_drafts(), 10)
                .expect("search")
        ),
        ["Gateway draft"],
        "the global draft is on the remote, which does not show global documents"
    );
}

#[test]
fn the_approval_setting_is_the_remotes_and_never_this_computers() {
    // No remote: nothing is the team's, even when this computer's file has the key.
    let mut local = store();
    local.set_meta("team_approval", "on").expect("meta");
    let mut alone = RoutedStore::new(Some(Box::new(local)), None, Routing::new(Where::Local, Where::Local));

    assert_eq!(alone.meta("team_approval").expect("meta").as_deref(), Some("on"));
    assert_eq!(alone.remote_meta("team_approval").expect("remote meta"), None);

    // A remote's value is read as it is.
    let mut remote = store();
    remote.set_meta("team_approval", "on").expect("meta");
    let mut routed = routed_over(store(), remote, Routing::new(Where::RemoteLocal, Where::RemoteLocal));
    assert_eq!(
        routed.remote_meta("team_approval").expect("remote meta").as_deref(),
        Some("on")
    );
    assert_eq!(routed.remote_meta("nothing").expect("remote meta"), None);

    // A remote that cannot be reached is an error, while the ordinary read falls back to this computer.
    let machine = Machine::new(false);
    let mut down = machine.process();

    assert_eq!(
        down.meta("dims").expect("meta").as_deref(),
        Some("384"),
        "the fallback answers"
    );
    assert!(matches!(down.remote_meta("dims"), Err(StoreError::Unreachable(_))));
    assert!(matches!(
        down.set_review(1, 0, &change(ReviewAction::Ask, 1)),
        Err(StoreError::Unreachable(_))
    ));

    // Once the cooldown is over and the remote answers, its value is read.
    machine.net.set(true);
    std::thread::sleep(super::flaky::COOLDOWN * 2);

    let mut up = machine.process();
    assert_eq!(up.remote_meta("dims").expect("up again").as_deref(), Some("384"));
}

//! `held list --sort addedBy`: by the name a proposal is written for, unknown last.

use super::*;

/// Holds a proposal titled `title` written for `name`.
fn propose_for(world: &World, title: &str, name: Option<&str>) {
    let item = HeldItem::new(
        Source {
            client: "codex".to_owned(),
            session: None,
            project_path: None,
            repo_key: None,
        },
        Proposal {
            title: title.to_owned(),
            format: "markdown".to_owned(),
            name: "proposal.md".to_owned(),
            source_path: None,
            content: format!("# {title}\n\nText of {title}."),
            links: Vec::new(),
            replaces: None,
            replaced: None,
        },
    );
    let mut item = item;
    item.added_by = name.map(str::to_owned);

    world.held().hold(item).expect("held");
}

fn titles(listed: &Value) -> Vec<&str> {
    listed["held"]
        .as_array()
        .expect("held")
        .iter()
        .map(|item| item["title"].as_str().expect("title"))
        .collect()
}

#[test]
fn the_queue_sorts_by_the_name_it_is_written_for_with_case_ignored_and_unknown_last() {
    let world = World::new();

    propose_for(&world, "Nameless", None);
    propose_for(&world, "By bob", Some("bob"));
    propose_for(&world, "By Cara", Some("Cara"));
    // A name that is not fit to show is not one.
    propose_for(&world, "Bad name", Some("Eve\u{202e}"));
    propose_for(&world, "By Ada", Some("  Ada Lovelace "));

    let sorted = world.done(&["list", "--json", "--sort", "addedBy"]);
    let order = titles(&sorted);

    assert_eq!(order[..3], ["By Ada", "By bob", "By Cara"]);
    assert_eq!(
        order[3..].iter().copied().collect::<std::collections::BTreeSet<_>>(),
        ["Bad name", "Nameless"].into_iter().collect(),
        "the unknown ones come after every name"
    );

    // The default is still newest first, and `created` says so.
    let newest = world.done(&["list", "--json"]);
    assert_eq!(world.done(&["list", "--json", "--sort", "created"]), newest);
    assert_eq!(titles(&newest).len(), 5);

    for bad in [
        &["list", "--json", "--sort", "title"][..],
        &["list", "--json", "--sort"],
    ] {
        assert!(matches!(world.run(bad), Reply::Usage), "{bad:?}");
    }
}

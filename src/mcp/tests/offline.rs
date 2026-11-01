//! The tools while the remote database cannot be reached (a port nobody listens on): with both
//! places in use the replies come from this computer, the first starts with one line saying so,
//! and an id of the remote database is an error.

use super::*;
use crate::config::{Routing, Where, backend_for};

const NOTICE: &str = "Remote knowledge is unreachable. Showing documents on this computer only.";

fn both() -> Routing {
    Routing::new(Where::RemoteLocal, Where::RemoteLocal)
}

/// A server of a computer with one document of its own and a remote database that is down.
fn offline_world() -> World {
    let mut world = World::new();
    let mut local = world.database();
    insert(
        &mut local,
        &Seed::new("Gateway guide").embedded("Install", "Install the gateway with the installer.", angled(5.0)),
    );
    drop(local);

    let home = world.home();
    let env = Env::for_tests(&home).with_var(DIR_VAR, world.knowledge().to_string_lossy());
    let projects = ProjectResolver::new(env.clone(), Some(home));
    let backend = backend_for(Some("postgres://nobody@127.0.0.1:1/none".to_owned()), None).expect("backend");

    world.server = Server::new(env, backend, projects, Box::new(Fake(Ok(axis(0))))).with_routing(both());

    world
}

fn search(world: &mut World) -> String {
    let project = project_arg(world);

    world.ok(
        "knowledge_search",
        json!({ "query": "install gateway", "project_path": project }),
    )
}

#[test]
fn the_first_reply_says_the_remote_is_unreachable_and_the_next_ones_do_not() {
    let mut world = offline_world();

    let first = search(&mut world);
    assert!(first.starts_with(NOTICE), "{first}");
    assert_eq!(first.matches(NOTICE).count(), 1);
    assert!(first.contains("title=\"Gateway guide\""), "{first}");
    assert!(
        first.contains("id=\"local:1:"),
        "the answer is this computer's: {first}"
    );

    let second = search(&mut world);
    assert!(!second.contains("unreachable"), "{second}");
    assert!(second.contains("title=\"Gateway guide\""), "{second}");

    let project = project_arg(&world);
    let listed = world.ok("knowledge_list", json!({ "project_path": project }));
    assert!(!listed.contains("unreachable"), "once per session: {listed}");
    assert!(listed.contains("Gateway guide"), "{listed}");
}

#[test]
fn a_list_or_a_get_opens_with_the_notice_when_it_is_the_first_reply() {
    let mut world = offline_world();
    let project = project_arg(&world);

    let listed = world.ok("knowledge_list", json!({ "project_path": project }));
    assert!(listed.starts_with(&format!("{NOTICE}\n")), "{listed}");

    let mut again = offline_world();
    let opened = again.ok(
        "knowledge_get",
        json!({ "id": "local:1", "project_path": project_arg(&again) }),
    );
    assert!(opened.starts_with(&format!("{NOTICE}\n")), "{opened}");
    assert!(opened.contains("Gateway guide"), "{opened}");
}

#[test]
fn a_document_of_the_remote_is_an_error_while_it_is_unreachable() {
    let mut world = offline_world();
    let project = project_arg(&world);

    let error = world.err("knowledge_get", json!({ "id": "7", "project_path": project }));
    assert_eq!(error, "remote knowledge is unreachable right now");

    let chunk = world.err(
        "knowledge_get",
        json!({ "id": "7:70", "project_path": project_arg(&world) }),
    );
    assert_eq!(chunk, "remote knowledge is unreachable right now");
}

#[test]
fn the_notice_comes_back_when_the_remote_was_reachable_in_between() {
    let mut world = offline_world();
    let offline = backend::open(
        &world.server.env,
        &world.server.backend,
        world.server.routing,
        Access::Read,
    )
    .expect("opens")
    .expect("a store");
    let online = backend::open(&world.server.env, &Backend::Sqlite, Routing::local(), Access::Read)
        .expect("opens")
        .expect("a store");

    world.server.store = Some(offline);
    assert!(search(&mut world).starts_with(NOTICE));
    assert!(!search(&mut world).contains(NOTICE));

    world.server.store = Some(online);
    assert!(!search(&mut world).contains(NOTICE), "an online reply has none");

    let down_again = backend::open(
        &world.server.env,
        &world.server.backend,
        world.server.routing,
        Access::Read,
    )
    .expect("opens")
    .expect("a store");
    world.server.store = Some(down_again);
    assert!(search(&mut world).starts_with(NOTICE), "the remote was lost again");
}

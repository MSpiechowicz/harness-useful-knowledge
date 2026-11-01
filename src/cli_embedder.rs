//! The `harness-knowledge embedder` command: `embedder [run]` serves the embedder (JSON lines on
//! standard output, see [`crate::embed::Event`]), `embedder enable|disable` switch it on or off for
//! this computer, `embedder status --json` says where it stands, and `embedder remove-model`
//! deletes the model files while it is not running.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::embed::server::{Endpoint, Lock, LockError, Server};
use crate::embed::{
    EmbedderClient, Event, Events, JsonLines, MODEL, Options, Service, TractLoader, data_folder, enabled, manifest,
    model, set_enabled,
};
use crate::support::Env;

/// The words `embedder` takes, for the usage line.
pub const USAGE: &str = "embedder [run | enable | disable | status --json | remove-model]";

/// Names a shorter idle time in seconds, for tests and the smoke script only.
const IDLE_VAR: &str = "HARNESS_KNOWLEDGE_EMBEDDER_IDLE_SECS";

/// Runs the command; `args` are the words after `embedder`.
pub fn embedder(args: &[Option<&str>]) -> ExitCode {
    match args {
        [] | [Some("run")] => run(),
        [Some("enable")] => switch(true),
        [Some("disable")] => switch(false),
        [Some("status"), Some("--json")] => status(),
        [Some("remove-model")] => remove_model(),
        _ => {
            eprintln!("usage: harness-knowledge {USAGE}");
            ExitCode::from(2)
        }
    }
}

/// The data folder, or the reason printed.
fn root() -> Option<(Env, std::path::PathBuf)> {
    let Ok(env) = Env::from_process() else {
        eprintln!("harness-knowledge: the home folder is unknown");
        return None;
    };

    match data_folder(&env) {
        Some(root) => Some((env, root)),
        None => {
            eprintln!("harness-knowledge: the data folder is unknown");
            None
        }
    }
}

fn switch(on: bool) -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    match set_enabled(&root, on) {
        Ok(()) => {
            println!("{}", json!({ "enabled": on }));
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("harness-knowledge: {message}");
            ExitCode::FAILURE
        }
    }
}

fn status() -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    let running = Lock::is_held(&root);
    let present = model::is_present(&model::model_dir(&root), &MODEL);

    // The running embedder tells what it serves, a stopped one what is installed.
    let reported = running
        .then(|| EmbedderClient::new(&root, &std::env::temp_dir()).status().ok())
        .flatten();

    let active = match &reported {
        Some(status) => status.model.clone(),
        None => present.then(|| MODEL.id.to_owned()),
    };

    println!(
        "{}",
        json!({
            "enabled": enabled(&root),
            "running": running,
            "active_model": active,
            "model_present": present,
            "ready": reported.as_ref().is_some_and(|status| status.ready),
            "workers": reported.as_ref().map(|status| status.workers),
        })
    );

    ExitCode::SUCCESS
}

fn remove_model() -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    if Lock::is_held(&root) {
        eprintln!("harness-knowledge: the embedder is running, stop it first");
        return ExitCode::FAILURE;
    }

    if let Err(error) = model::remove(&root) {
        eprintln!("harness-knowledge: cannot delete the model files ({})", error.kind());
        return ExitCode::FAILURE;
    }

    println!("{}", json!({ "removed": true }));
    ExitCode::SUCCESS
}

fn run() -> ExitCode {
    let Some((_, root)) = root() else {
        return ExitCode::FAILURE;
    };

    let events: Arc<dyn Events> = Arc::new(JsonLines);

    let lock = match Lock::take(&root) {
        Ok(lock) => lock,
        Err(LockError::Busy) => {
            events.emit(Event::Busy);
            return ExitCode::SUCCESS;
        }
        Err(LockError::Failed(message)) => {
            events.emit(Event::Error { message });
            return ExitCode::FAILURE;
        }
    };

    let mut options = Options::new(root.clone(), MODEL);
    options.base_url = std::env::var(manifest::BASE_URL_VAR)
        .ok()
        .filter(|base| !base.trim().is_empty());

    if let Some(seconds) = std::env::var(IDLE_VAR)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        options.idle = Duration::from_secs(seconds);
    }

    if let Some(base) = &options.base_url
        && let Err(error) = model::check_base(base)
    {
        events.emit(Event::Error {
            message: error.to_string(),
        });
        return ExitCode::from(2);
    }

    let service = Arc::new(Service::new(options, Arc::new(TractLoader), Arc::clone(&events)));

    let server = match Server::start(Endpoint::of(&root, &std::env::temp_dir()), Arc::clone(&service)) {
        Ok(server) => server,
        Err(error) => {
            events.emit(Event::Error {
                message: format!("cannot listen on the socket ({error})"),
            });
            service.shutdown();
            return ExitCode::FAILURE;
        }
    };

    // The harness that started the embedder closes its end when it exits.
    let stopper = Arc::clone(&service);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
        stopper.stop().stop();
    });

    // SIGINT, SIGTERM and SIGHUP (the console events on Windows) end the run like a closed stdin.
    let signalled = Arc::clone(&service);

    if let Err(error) = ctrlc::set_handler(move || signalled.stop().stop()) {
        eprintln!("harness-knowledge: cannot watch for termination signals ({error})");
    }

    let working = Arc::clone(&service);
    std::thread::spawn(move || working.lifecycle());

    service.stop().wait_forever();
    server.shutdown();
    service.shutdown();
    drop(lock);
    ExitCode::SUCCESS
}

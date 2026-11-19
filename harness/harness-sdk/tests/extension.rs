#![cfg(feature = "test-support")]
//! The public `Extension` seam, driven through a real harness by an extension of the test's own: which
//! hooks fire and when, that the addendum an extension returns reaches the model together with the
//! caller's own, that several extensions chain in order, and that one that panics or fails to start
//! costs the application nothing. No API keys, no network beyond loopback.
#[path = "support/model_mock.rs"]
mod model_mock;
use model_mock::{system_of, Answer, MockModel};
use openworker_sdk::extension::{
    BoxFuture, DecorateContext, Extension, SessionInfo, StartContext, TurnContext,
};
use openworker_sdk::{agents::*, Credentials, Error, Model, ModelProvider, Result, TurnOptions};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

fn runner() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sdk-test-host"))
}

type Log = Arc<Mutex<Vec<String>>>;

/// Records every hook it is called by, and adds its text to each turn's addendum.
struct Recorder {
    name: &'static str,
    add: &'static str,
    log: Log,
}

impl Recorder {
    fn note(&self, what: String) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}: {what}", self.name));
    }
}

impl Extension for Recorder {
    fn name(&self) -> &str {
        self.name
    }
    fn on_start<'a>(&'a self, ctx: StartContext<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.note(format!(
                "start (state dir ends with 'state': {})",
                ctx.state_dir.ends_with("state")
            ));
            Ok(())
        })
    }
    fn decorate_agent(&self, agent: &mut Agent, ctx: &DecorateContext) {
        self.note(format!(
            "decorate {} (tools registered: {})",
            agent.id(),
            ctx.tools_registered
        ));
    }
    fn on_session_created(&self, session: &SessionInfo) {
        self.note(format!("created (agent {})", session.agent));
    }
    fn on_session_attached(&self, session: &SessionInfo) {
        self.note(format!("attached (agent {})", session.agent));
    }
    fn before_send<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            self.note(format!(
                "before_send {:?} own={:?}",
                turn.text, turn.addendum
            ));
            Some(match turn.addendum {
                Some(own) => format!("{own}\n{}", self.add),
                None => self.add.to_owned(),
            })
        })
    }
    fn before_close<'a>(&'a self, session: &'a SessionInfo) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.note(format!("before_close (agent {})", session.agent)) })
    }
    fn on_shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.note("shutdown".into()) })
    }
}

/// Fails to start.
struct FailsToStart;
impl Extension for FailsToStart {
    fn name(&self) -> &str {
        "fails-to-start"
    }
    fn on_start<'a>(&'a self, _: StartContext<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Err(Error::Setup("not today".into())) })
    }
    fn before_send<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { Some("MUST-NOT-APPEAR".into()) })
    }
}

/// Panics while starting.
struct PanicsWhileStarting;
impl Extension for PanicsWhileStarting {
    fn name(&self) -> &str {
        "panics-while-starting"
    }
    fn on_start<'a>(&'a self, _: StartContext<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("on_start blew up") })
    }
}

/// Panics when a turn is sent.
struct PanicsInBeforeSend;
impl Extension for PanicsInBeforeSend {
    fn name(&self) -> &str {
        "panics-in-before-send"
    }
    fn before_send<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { panic!("before_send blew up") })
    }
}

async fn model() -> MockModel {
    MockModel::start(|_| Answer::Text("ok".into())).await
}

fn builder(root: &std::path::Path, model: &MockModel) -> openworker_sdk::HarnessBuilder {
    Harness::builder()
        .state_dir(root.join("state"))
        .executable(runner())
        .sandbox(Sandbox::Off)
        .model_provider(
            ModelProvider::chat_completions("Fixture")
                .base_url(model.url())
                .credentials(Credentials::secure("fixture-key").memory_only()),
        )
}

fn agent(id: &str) -> Agent {
    Agent::new(id)
        .instructions("Be helpful.")
        .default_model(Model::new("Fixture", "fixture"))
}

async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_fire_in_order_and_the_addendum_reaches_the_model_beside_the_callers_own() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let model = model().await;
    let log: Log = Arc::default();
    let h = builder(&root, &model)
        .extension(Recorder {
            name: "first",
            add: "FROM-FIRST",
            log: log.clone(),
        })
        .extension(Recorder {
            name: "second",
            add: "FROM-SECOND",
            log: log.clone(),
        })
        .agent(agent("early"))
        .start()
        .await
        .unwrap();
    let a = h.create_agent(agent("late")).await.unwrap();
    let s = a.new_session().await.unwrap();
    let run = within(s.send_with_options(
        "hello there",
        TurnOptions {
            instruction_addendum: Some("MINE".into()),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    within(run.result()).await.unwrap();
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();

    let log = log.lock().unwrap().clone();
    let at = |needle: &str| {
        log.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not in {log:#?}"))
    };
    // an agent given to the builder is decorated before the host compiles it (no tools can be named yet);
    // the harness then starts the extensions; an agent created later is decorated with tools registered
    assert!(at("first: decorate early (tools registered: false)") < at("first: start"));
    assert!(at("first: start") < at("first: decorate late (tools registered: true)"));
    assert!(log
        .iter()
        .any(|l| l == "first: start (state dir ends with 'state': true)"));
    assert!(at("first: decorate late") < at("first: created (agent late)"));
    assert!(at("first: created") < at("first: before_send"));
    assert!(at("first: before_send") < at("first: before_close"));
    assert!(at("first: before_close") < at("first: shutdown"));
    // extensions are called in the order they were added, each seeing what the one before made of the addendum
    assert!(
        log.contains(&"first: before_send \"hello there\" own=Some(\"MINE\")".to_owned()),
        "{log:#?}"
    );
    assert!(
        log.contains(
            &"second: before_send \"hello there\" own=Some(\"MINE\\nFROM-FIRST\")".to_owned()
        ),
        "{log:#?}"
    );
    // and the model got the result, as trusted instructions for that turn, and not as the user's words
    let request = &model.requests()[0];
    let system = system_of(request);
    assert!(
        system.contains("Application instructions for this turn:\nMINE\nFROM-FIRST\nFROM-SECOND"),
        "{system}"
    );
    assert!(!model_mock::conversation_of(request).contains("FROM-FIRST"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extension_that_fails_or_panics_while_starting_is_left_out_and_the_harness_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let model = model().await;
    let log: Log = Arc::default();
    let h = builder(&root, &model)
        .extension(FailsToStart)
        .extension(PanicsWhileStarting)
        .extension(Recorder {
            name: "good",
            add: "FROM-GOOD",
            log: log.clone(),
        })
        .start()
        .await
        .unwrap();
    let a = h.create_agent(agent("assistant")).await.unwrap();
    let s = a.new_session().await.unwrap();
    within(s.run("hello there")).await.unwrap();
    let system = system_of(&model.requests()[0]);
    assert!(system.contains("FROM-GOOD"), "{system}");
    assert!(
        !system.contains("MUST-NOT-APPEAR"),
        "an extension that did not start gets no hooks: {system}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extension_that_panics_in_a_hook_does_not_fail_the_turn_or_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let model = model().await;
    let log: Log = Arc::default();
    let h = builder(&root, &model)
        .extension(Recorder {
            name: "before",
            add: "FROM-BEFORE",
            log: log.clone(),
        })
        .extension(PanicsInBeforeSend)
        .extension(Recorder {
            name: "after",
            add: "FROM-AFTER",
            log: log.clone(),
        })
        .start()
        .await
        .unwrap();
    let a = h.create_agent(agent("assistant")).await.unwrap();
    let s = a.new_session().await.unwrap();
    let r = within(s.run("hello there")).await.unwrap();
    assert_eq!(r.status, RunStatus::Completed);
    assert_eq!(r.final_output.as_deref(), Some("ok"));
    // the panic left the addendum as the extension before it made it, and the one after still added its own
    let system = system_of(&model.requests()[0]);
    assert!(
        system.contains("Application instructions for this turn:\nFROM-BEFORE\nFROM-AFTER"),
        "{system}"
    );
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_extension_a_turn_carries_exactly_the_callers_own_addendum() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let model = model().await;
    let h = builder(&root, &model).start().await.unwrap();
    let a = h.create_agent(agent("assistant")).await.unwrap();
    let s = a.new_session().await.unwrap();
    let run = within(s.send_with_options(
        "hello there",
        TurnOptions {
            instruction_addendum: Some("MINE".into()),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    within(run.result()).await.unwrap();
    within(s.run("and again")).await.unwrap();
    let requests = model.requests();
    assert!(
        system_of(&requests[0]).ends_with("Application instructions for this turn:\nMINE"),
        "{}",
        system_of(&requests[0])
    );
    assert!(!system_of(&requests[1]).contains("Application instructions"));
    within(s.close()).await.unwrap();
    within(h.shutdown()).await.unwrap();
}

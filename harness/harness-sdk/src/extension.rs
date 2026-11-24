//! Extension points for components that sit beside a harness: they are told when a session starts
//! or ends, may add trusted text to a turn before it is sent, and are given the chance to shape an
//! agent definition before it is registered. Nothing here changes what an application calls: with no
//! extension attached every hook is skipped and the SDK behaves as it always did.
//!
//! Hooks are advisory. A hook that panics (while it is built or while it runs) is ignored and reported on
//! stderr, an addendum that the engine would refuse is ignored, and `on_start` failing drops that extension
//! and the harness starts without it. (Panics are only caught when panics unwind: with `panic = "abort"` a
//! panicking hook ends the process.) Nothing bounds a hook's time: a slow hook delays the call it hooks
//! into, and one that never returns holds that call for ever, so a hook bounds its own time.
use crate::{Agent, Harness, HarnessClient, Result};
use futures_util::FutureExt;
use std::{
    future::Future,
    io::Write,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    pin::Pin,
    sync::Arc,
};

/// The engine accepts at most this much application addendum per turn (UTF-8 bytes), and no NUL.
pub(crate) const MAX_ADDENDUM_BYTES: usize = 32_000;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Which session a hook is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: String,
    pub agent: String,
}

/// What an extension gets when the harness is up, before the application can create a session.
pub struct StartContext<'a> {
    /// The started harness, for `register_tools`. Do not keep it: the harness owns the extension.
    pub harness: &'a Harness,
    /// The protocol client (subscribe to the journal, read history).
    pub client: &'a HarnessClient,
    /// The application's state directory.
    pub state_dir: &'a Path,
}

/// Where an agent definition is on its way.
pub struct DecorateContext {
    /// False for agents given to `HarnessBuilder::agent`: the host compiles them while it opens, before
    /// application tools can be registered, so an agent that names a tool registered later fails to compile.
    pub tools_registered: bool,
}

/// A message about to be sent.
pub struct TurnContext<'a> {
    pub session: &'a SessionInfo,
    /// Stable for a retry of the same message: the answer for one submission must not change (the engine
    /// refuses a submission id that comes back with a different turn context).
    pub submission_id: &'a str,
    /// The user's text for this turn.
    pub text: &'a str,
    /// The application addendum already set for this turn (the caller's own, or an earlier extension's).
    pub addendum: Option<&'a str>,
}

/// What the extensions add to a turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnAddition {
    /// The addendum to send with the turn (`None`: none).
    pub addendum: Option<String>,
    /// Text put in front of the user's message, inside the message itself. Unlike the addendum it is part
    /// of the conversation the engine keeps, so the next turn's prompt starts with this turn's; whatever
    /// reads the conversation back (an archive, a capture) has to take it out again. The prefixes of
    /// several extensions follow one another.
    pub user_prefix: Option<String>,
}

pub trait Extension: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// The harness is up. Register tools and start listening here.
    fn on_start<'a>(&'a self, _ctx: StartContext<'a>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// An agent definition is about to be registered (or compiled by the host).
    fn decorate_agent(&self, _agent: &mut Agent, _ctx: &DecorateContext) {}

    /// A session was just created by this process.
    fn on_session_created(&self, _session: &SessionInfo) {}

    /// A session was created or reattached: its journal is now delivered to this process.
    fn on_session_attached(&self, _session: &SessionInfo) {}

    /// The addendum to send with this turn (instead of `turn.addendum`). It is trusted application text,
    /// not stored as part of the conversation. Return `turn.addendum` unchanged to add nothing. The engine
    /// accepts at most 32,000 bytes and no NUL character: an answer that breaks either is ignored (the
    /// addendum stays as it was before this extension).
    fn before_send<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { turn.addendum.map(str::to_owned) })
    }

    /// Like [`Extension::before_send`], and the one to implement to also put text in front of the user's message
    /// (`TurnAddition::user_prefix`). Same rules: stable for a retry of the same submission; an addendum the
    /// engine would refuse (over 32,000 bytes, a NUL) or a prefix over that size or with a NUL is ignored. The
    /// default only calls `before_send`.
    fn before_send_turn<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, TurnAddition> {
        Box::pin(async move {
            TurnAddition {
                addendum: self.before_send(turn).await,
                user_prefix: None,
            }
        })
    }

    /// `Session::close` is about to close the session.
    fn before_close<'a>(&'a self, _session: &'a SessionInfo) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// `Session::close` failed after `before_close` ran (the engine refused it, say because a turn is still
    /// running): the session is still open.
    fn on_close_failed(&self, _session: &SessionInfo) {}

    /// `Harness::shutdown` is about to stop the harness.
    fn on_shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// The extensions of one harness, shared by the harness, its agents and its sessions.
#[derive(Clone, Default)]
pub(crate) struct Extensions(Arc<Vec<Arc<dyn Extension>>>);

impl Extensions {
    pub(crate) fn new(list: Vec<Arc<dyn Extension>>) -> Self {
        Self(Arc::new(list))
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn decorate(&self, agent: &mut Agent, ctx: &DecorateContext) {
        for ext in self.0.iter() {
            // (a panic must not leave a half-changed definition behind)
            let before = agent.clone();
            let done = catch_unwind(AssertUnwindSafe(|| ext.decorate_agent(agent, ctx)));
            if done.is_err() {
                *agent = before;
            }
            ignored(ext.as_ref(), "decorate_agent", done.is_err());
        }
    }
    pub(crate) fn created(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            let done = catch_unwind(AssertUnwindSafe(|| ext.on_session_created(session)));
            ignored(ext.as_ref(), "on_session_created", done.is_err());
        }
    }
    pub(crate) fn attached(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            let done = catch_unwind(AssertUnwindSafe(|| ext.on_session_attached(session)));
            ignored(ext.as_ref(), "on_session_attached", done.is_err());
        }
    }
    /// What a turn gets after every extension has had its say, in the order they were added: the addendum (the last
    /// answer that is allowed) and the text in front of the user's message (every allowed one, in order).
    pub(crate) async fn turn(
        &self,
        session: &SessionInfo,
        submission_id: &str,
        text: &str,
        addendum: Option<String>,
    ) -> TurnAddition {
        let mut current = addendum;
        let mut prefix = String::new();
        for ext in self.0.iter() {
            let turn = TurnContext {
                session,
                submission_id,
                text,
                addendum: current.as_deref(),
            };
            // a panicking extension leaves the turn as it was before it (the call sits inside the future, so a
            // panic while the hook builds its future is caught as well)
            match AssertUnwindSafe(async { ext.before_send_turn(turn).await })
                .catch_unwind()
                .await
            {
                Ok(next) => {
                    let unusable = |t: &str| t.len() > MAX_ADDENDUM_BYTES || t.contains('\0');
                    if next.addendum.as_deref().is_some_and(unusable)
                        || next.user_prefix.as_deref().is_some_and(unusable)
                    {
                        // the engine would refuse every send of this application for it
                        refused(ext.as_ref());
                    } else {
                        current = next.addendum;
                        if let Some(p) = next.user_prefix {
                            prefix.push_str(&p);
                        }
                    }
                }
                Err(_) => ignored(ext.as_ref(), "before_send", true),
            }
        }
        TurnAddition {
            addendum: current,
            user_prefix: (!prefix.is_empty()).then_some(prefix),
        }
    }
    #[cfg(test)]
    pub(crate) async fn before_send(
        &self,
        session: &SessionInfo,
        submission_id: &str,
        text: &str,
        addendum: Option<String>,
    ) -> Option<String> {
        self.turn(session, submission_id, text, addendum)
            .await
            .addendum
    }
    pub(crate) async fn before_close(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            let done = AssertUnwindSafe(async { ext.before_close(session).await })
                .catch_unwind()
                .await;
            ignored(ext.as_ref(), "before_close", done.is_err());
        }
    }
    pub(crate) fn close_failed(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            let done = catch_unwind(AssertUnwindSafe(|| ext.on_close_failed(session)));
            ignored(ext.as_ref(), "on_close_failed", done.is_err());
        }
    }
    pub(crate) async fn shutdown(&self) {
        for ext in self.0.iter() {
            let done = AssertUnwindSafe(async { ext.on_shutdown().await })
                .catch_unwind()
                .await;
            ignored(ext.as_ref(), "on_shutdown", done.is_err());
        }
    }
}

/// Says so on stderr when an extension's hook panicked (the panic is otherwise swallowed).
fn ignored(ext: &dyn Extension, hook: &str, panicked: bool) {
    if panicked {
        say(&format!(
            "openworker: extension {} panicked in {hook}; ignored",
            ext.name()
        ));
    }
}

fn refused(ext: &dyn Extension) {
    say(&format!(
        "openworker: extension {} returned a turn addendum the engine would refuse (over {MAX_ADDENDUM_BYTES} bytes, or with a NUL); ignored",
        ext.name()
    ));
}

/// A line on stderr that cannot itself panic (a closed stderr is not worth stopping a turn for).
pub(crate) fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Panicky;
    impl Extension for Panicky {
        fn name(&self) -> &str {
            "panicky"
        }
        fn decorate_agent(&self, _: &mut Agent, _: &DecorateContext) {
            panic!("decorate");
        }
        fn on_session_created(&self, _: &SessionInfo) {
            panic!("created");
        }
        fn before_send<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
            Box::pin(async { panic!("before_send") })
        }
        fn before_close<'a>(&'a self, _: &'a SessionInfo) -> BoxFuture<'a, ()> {
            Box::pin(async { panic!("before_close") })
        }
    }

    struct Adds(&'static str);
    impl Extension for Adds {
        fn name(&self) -> &str {
            "adds"
        }
        fn before_send<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
            Box::pin(async move {
                Some(match turn.addendum {
                    Some(own) => format!("{own}+{}", self.0),
                    None => self.0.to_owned(),
                })
            })
        }
    }

    /// Puts text in front of the user's message and leaves the addendum as it found it.
    struct Prefixes(&'static str);
    impl Extension for Prefixes {
        fn name(&self) -> &str {
            "prefixes"
        }
        fn before_send_turn<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, TurnAddition> {
            Box::pin(async move {
                TurnAddition {
                    addendum: turn.addendum.map(str::to_owned),
                    user_prefix: Some(self.0.to_owned()),
                }
            })
        }
    }

    fn session() -> SessionInfo {
        SessionInfo {
            session_id: "s".into(),
            agent: "a".into(),
        }
    }

    #[tokio::test]
    async fn the_prefixes_of_the_extensions_follow_one_another_and_the_addendum_is_not_touched() {
        let set = Extensions::new(vec![
            Arc::new(Prefixes("one\n\n")),
            Arc::new(Adds("a")),
            Arc::new(Prefixes("two\n\n")),
        ]);
        let got = set.turn(&session(), "sub", "hi", Some("own".into())).await;
        assert_eq!(got.user_prefix.as_deref(), Some("one\n\ntwo\n\n"));
        assert_eq!(got.addendum.as_deref(), Some("own+a"));
    }

    #[tokio::test]
    async fn an_extension_that_only_has_before_send_adds_no_prefix() {
        let set = Extensions::new(vec![Arc::new(Adds("a"))]);
        let got = set.turn(&session(), "sub", "hi", None).await;
        assert_eq!(
            got,
            TurnAddition {
                addendum: Some("a".into()),
                user_prefix: None
            }
        );
    }

    #[tokio::test]
    async fn a_prefix_the_engine_would_refuse_is_ignored_with_its_addendum() {
        for bad in ["a\0b".to_owned(), "x".repeat(MAX_ADDENDUM_BYTES + 1)] {
            let bad: &'static str = Box::leak(bad.into_boxed_str());
            let set = Extensions::new(vec![Arc::new(Prefixes("ok\n\n")), Arc::new(Prefixes(bad))]);
            let got = set.turn(&session(), "sub", "hi", Some("own".into())).await;
            assert_eq!(got.user_prefix.as_deref(), Some("ok\n\n"));
            assert_eq!(got.addendum.as_deref(), Some("own"));
        }
    }

    #[tokio::test]
    async fn a_panicking_extension_leaves_the_prefix_as_it_was() {
        let set = Extensions::new(vec![Arc::new(Prefixes("one\n\n")), Arc::new(Panicky)]);
        let got = set.turn(&session(), "sub", "hi", None).await;
        assert_eq!(got.user_prefix.as_deref(), Some("one\n\n"));
    }

    #[tokio::test]
    async fn a_panicking_extension_is_ignored_and_the_others_still_run_in_order() {
        let set = Extensions::new(vec![
            Arc::new(Adds("a")),
            Arc::new(Panicky),
            Arc::new(Adds("b")),
        ]);
        let s = session();
        assert_eq!(
            set.before_send(&s, "sub", "hi", Some("own".into()))
                .await
                .as_deref(),
            Some("own+a+b")
        );
        set.created(&s);
        set.attached(&s);
        set.before_close(&s).await;
        let mut agent = Agent::new("x").instructions("i");
        set.decorate(
            &mut agent,
            &DecorateContext {
                tools_registered: true,
            },
        );
        assert_eq!(agent.instructions_text(), "i");
        assert!(Extensions::default().is_empty());
    }

    /// Panics before it returns a future (the part of a hook the tests above do not reach).
    struct PanicsBuildingTheFuture;
    impl Extension for PanicsBuildingTheFuture {
        fn name(&self) -> &str {
            "panics-early"
        }
        fn before_send<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
            panic!("before the future exists")
        }
        fn before_close<'a>(&'a self, _: &'a SessionInfo) -> BoxFuture<'a, ()> {
            panic!("before the future exists")
        }
        fn on_shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            panic!("before the future exists")
        }
    }

    #[tokio::test]
    async fn a_hook_that_panics_before_it_returns_its_future_is_ignored_too() {
        let set = Extensions::new(vec![Arc::new(PanicsBuildingTheFuture), Arc::new(Adds("b"))]);
        let s = session();
        assert_eq!(
            set.before_send(&s, "sub", "hi", Some("own".into()))
                .await
                .as_deref(),
            Some("own+b")
        );
        set.before_close(&s).await;
        set.shutdown().await;
    }

    struct EditsThenPanics;
    impl Extension for EditsThenPanics {
        fn name(&self) -> &str {
            "edits-then-panics"
        }
        fn decorate_agent(&self, agent: &mut Agent, _: &DecorateContext) {
            *agent = std::mem::replace(agent, Agent::new("x")).instructions("half done");
            panic!("after a change");
        }
    }

    #[test]
    fn a_decorate_that_panics_leaves_the_agent_as_it_was() {
        let set = Extensions::new(vec![Arc::new(EditsThenPanics)]);
        let mut agent = Agent::new("x").instructions("i");
        set.decorate(
            &mut agent,
            &DecorateContext {
                tools_registered: true,
            },
        );
        assert_eq!(agent.instructions_text(), "i");
    }

    struct Returns(Option<String>);
    impl Extension for Returns {
        fn name(&self) -> &str {
            "returns"
        }
        fn before_send<'a>(&'a self, _: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
            let answer = self.0.clone();
            Box::pin(async move { answer })
        }
    }

    #[tokio::test]
    async fn an_addendum_the_engine_would_refuse_is_ignored_and_the_one_before_it_stands() {
        let s = session();
        for refused in [
            "x".repeat(MAX_ADDENDUM_BYTES + 1),
            "a null \0 inside".to_owned(),
        ] {
            let set = Extensions::new(vec![
                Arc::new(Adds("a")),
                Arc::new(Returns(Some(refused))),
                Arc::new(Adds("b")),
            ]);
            assert_eq!(
                set.before_send(&s, "sub", "hi", Some("own".into()))
                    .await
                    .as_deref(),
                Some("own+a+b"),
                "the refused answer is skipped, the others still run"
            );
        }
        // exactly the limit is fine
        let set = Extensions::new(vec![Arc::new(Returns(Some(
            "y".repeat(MAX_ADDENDUM_BYTES),
        )))]);
        assert_eq!(
            set.before_send(&s, "sub", "hi", None)
                .await
                .map(|t| t.len()),
            Some(MAX_ADDENDUM_BYTES)
        );
    }
}

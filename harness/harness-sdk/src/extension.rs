//! Extension points for components that sit beside a harness: they are told when a session starts
//! or ends, may add trusted text to a turn before it is sent, and are given the chance to shape an
//! agent definition before it is registered. Nothing here changes what an application calls: with no
//! extension attached every hook is skipped and the SDK behaves as it always did.
//!
//! Hooks are advisory. A hook that fails or is slow must not fail a turn, so the SDK never turns an
//! extension's problem into an application error (`on_start` is the one exception: a failing start
//! drops that extension and the harness starts without it).
use crate::{Agent, Harness, HarnessClient, Result};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

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
    /// Stable for a retry of the same message: the answer for one submission must not change.
    pub submission_id: &'a str,
    /// The user's text for this turn.
    pub text: &'a str,
    /// The application addendum already set for this turn (the caller's own, or an earlier extension's).
    pub addendum: Option<&'a str>,
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
    /// not stored as part of the conversation. Return `turn.addendum` unchanged to add nothing.
    fn before_send<'a>(&'a self, turn: TurnContext<'a>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { turn.addendum.map(str::to_owned) })
    }

    /// `Session::close` is about to close the session.
    fn before_close<'a>(&'a self, _session: &'a SessionInfo) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

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
            ext.decorate_agent(agent, ctx);
        }
    }
    pub(crate) fn created(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            ext.on_session_created(session);
        }
    }
    pub(crate) fn attached(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            ext.on_session_attached(session);
        }
    }
    /// The addendum for a turn after every extension has had its say, in the order they were added.
    pub(crate) async fn before_send(
        &self,
        session: &SessionInfo,
        submission_id: &str,
        text: &str,
        addendum: Option<String>,
    ) -> Option<String> {
        let mut current = addendum;
        for ext in self.0.iter() {
            current = ext
                .before_send(TurnContext {
                    session,
                    submission_id,
                    text,
                    addendum: current.as_deref(),
                })
                .await;
        }
        current
    }
    pub(crate) async fn before_close(&self, session: &SessionInfo) {
        for ext in self.0.iter() {
            ext.before_close(session).await;
        }
    }
    pub(crate) async fn shutdown(&self) {
        for ext in self.0.iter() {
            ext.on_shutdown().await;
        }
    }
}

//! Long-term memory with HeliconSearch: a small agent, plus one configuration file.
//!
//! Session 1 tells the agent a fact and closes (which waits for HeliconSearch's answer to the extraction
//! request); session 2 asks about it. The recall happens inside the SDK: this program mentions memory only in
//! the configuration it writes and in one tool-approval line. Guide: docs/harness/guides/helicon-memory-rust.md
//!
//! ```sh
//! cd harness                                               # the directory with Cargo.toml
//! cargo build -p openworker-sdk --bin openworker-harness   # the helper that agents with tools need
//! export HELICON_TENANT_TOKEN=hs_...            # your HeliconSearch tenant token
//! export OPENAI_API_KEY=...                     # any OpenAI-compatible chat-completions endpoint
//! export OPENWORKER_MODEL=gpt-4o-mini           # an exact model ID of that endpoint
//! # optional: HELICON_URL (default http://127.0.0.1:5001), OPENWORKER_MODEL_URL (default OpenAI)
//! cargo run -p openworker-sdk --example helicon_memory
//! ```
//!
//! State, configuration and log live in `./.helicon-example-state/` (the configuration is written once: edit
//! it there to change a setting, or delete the directory).
use openworker_sdk::agents::RunStatus;
use openworker_sdk::{Agent, Credentials, Harness, Model, ModelProvider, Sandbox};

/// The configuration file the extension reads from the state directory. The token stays in the
/// environment (`${HELICON_TENANT_TOKEN}` is read when the harness starts). `closeWaitMs` makes
/// `session.close()` wait for HeliconSearch to answer the extraction request of this program's own session
/// (it answers when the facts are extracted), so the second session can already use them; a long-running
/// application leaves it at its default, 0. `debug` writes the log the guide's "check that it works" reads.
fn write_config(state_dir: &std::path::Path) -> anyhow::Result<()> {
    let file = state_dir.join("helicon-memory.json");
    if file.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)?;
    let config = serde_json::json!({
        "baseUrl": std::env::var("HELICON_URL").unwrap_or_else(|_| "http://127.0.0.1:5001".into()),
        "apiKey": "${HELICON_TENANT_TOKEN}",
        "agentId": "helicon-memory-example",
        "closeWaitMs": 120_000,
        "debug": true,
    });
    std::fs::write(file, serde_json::to_string_pretty(&config)?)?;
    Ok(())
}

async fn ask(agent: &openworker_sdk::AgentHandle, question: &str) -> anyhow::Result<()> {
    let session = agent.new_session().await?;
    println!("you:   {question}");
    let result = session.run(question).await?;
    match result.status {
        RunStatus::Completed => println!("agent: {}", result.final_output.unwrap_or_default()),
        other => println!("agent: (the run ended as {other:?})"),
    }
    // ends the session on HeliconSearch, which then writes its summary
    session.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("HELICON_TENANT_TOKEN").is_ok(),
        "set HELICON_TENANT_TOKEN to your HeliconSearch tenant token"
    );
    let model = std::env::var("OPENWORKER_MODEL").map_err(|_| {
        anyhow::anyhow!("set OPENWORKER_MODEL to an exact model ID of your endpoint")
    })?;
    let state_dir = std::env::current_dir()?.join(".helicon-example-state");
    write_config(&state_dir)?;

    // An agent with tools (the memory tools are some) needs the `openworker-harness` helper: set
    // OPENWORKER_EXECUTABLE to it, or build it next to this example (see above) and it is found.
    let helper = std::env::current_exe()?
        .parent()
        .and_then(|examples| examples.parent())
        .map(|target| {
            target.join(format!(
                "openworker-harness{}",
                std::env::consts::EXE_SUFFIX
            ))
        })
        .filter(|path| path.exists());
    let mut builder = Harness::builder()
        .state_dir(&state_dir)
        .sandbox(Sandbox::Off);
    if let (Some(helper), true) = (helper, std::env::var_os("OPENWORKER_EXECUTABLE").is_none()) {
        builder = builder.executable(helper);
    }
    let harness = builder.start().await?;
    let outcome = async {
        harness
            .set_model_provider(
                ModelProvider::chat_completions("Example")
                    .base_url(
                        std::env::var("OPENWORKER_MODEL_URL")
                            .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
                    )
                    .credentials(Credentials::from_env("OPENAI_API_KEY").memory_only()),
            )
            .await?;
        // Register the agent with `create_agent` (not `HarnessBuilder::agent`): that is what gives it the
        // memory tools. Searching memory needs no approval here; forgetting does (`helicon.memory_forget` is
        // the tool's name for the application, `helicon__memory_forget` the model's).
        let agent = harness
            .create_agent(
                Agent::new("memory-example-v1")
                    .instructions("Give short, helpful answers.")
                    .default_model(Model::new("Example", model))
                    .planning(false)
                    .scheduling(false)
                    .interrupt_on(["helicon.memory_forget"]),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "{e} (this agent names a memory tool: if memory is off, stderr says why, \
                     `openworker: helicon-memory is off: ...`)"
                )
            })?;
        ask(
            &agent,
            "My favorite tea is jasmine green tea. Please remember that.",
        )
        .await?;
        ask(&agent, "What is my favorite tea?").await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    // sends what is pending and ends the sessions still open
    harness.shutdown().await?;
    outcome
}

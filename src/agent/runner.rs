use compact_str::CompactString;
use futures::StreamExt;
use rig::agent::{Agent, MultiTurnStreamItem};
use rig::completion::{CompletionModel, Message};
use rig::message::ToolResultContent;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use tokio::sync::mpsc;

use crate::agent::recovery::{self, ErrorKind, RecoveryPolicy};
use crate::agent::tools::ToolCache;
use crate::event::AgentEvent;
use crate::session::{MessageRole, Session};

pub struct AgentRunner {
    pub event_rx: mpsc::Receiver<AgentEvent>,
}

pub fn convert_history(session: &Session) -> Vec<Message> {
    let (summary, first_kept) = session.compacted_context();
    let mut messages = Vec::new();

    if let Some(summary) = summary {
        messages.push(Message::system(format!(
            "[Previous conversation summary]\n{}",
            summary
        )));
    }

    for msg in &session.messages[first_kept..] {
        match msg.role {
            MessageRole::User => messages.push(Message::user(msg.content.to_string())),
            MessageRole::Assistant => messages.push(Message::assistant(msg.content.to_string())),
            MessageRole::System => messages.push(Message::system(msg.content.to_string())),
        }
    }

    messages
}

#[derive(Default)]
struct StreamBuffer {
    events: Vec<AgentEvent>,
    had_tool_calls: bool,
}

impl StreamBuffer {
    fn push(&mut self, event: AgentEvent) {
        if matches!(
            event,
            AgentEvent::ToolCall { .. } | AgentEvent::ToolResult { .. }
        ) {
            self.had_tool_calls = true;
        }
        self.events.push(event);
    }

    fn flush(&self, tx: &mpsc::Sender<AgentEvent>) {
        for event in &self.events {
            let _ = tx.try_send(event.clone());
        }
    }
}

async fn run_stream<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    history: Vec<Message>,
) -> (StreamBuffer, Result<(), String>)
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut buf = StreamBuffer::default();
    let mut stream = agent.stream_chat(prompt.to_string(), history).await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                buf.push(AgentEvent::Token(CompactString::from(text.text)));
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                buf.push(AgentEvent::Reasoning(CompactString::new(r.display_text())));
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                buf.push(AgentEvent::ToolCall {
                    name: CompactString::from(tool_call.function.name),
                    args: tool_call.function.arguments,
                });
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                let mut output = String::new();
                for c in tool_result.content.iter() {
                    if let ToolResultContent::Text(t) = c {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&t.text);
                    }
                }
                buf.push(AgentEvent::ToolResult {
                    output: CompactString::from(output),
                });
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let response_text = res.response();
                let estimated_tokens = Session::estimate_tokens(response_text);
                buf.push(AgentEvent::Done {
                    response: CompactString::from(response_text),
                    tokens: estimated_tokens,
                    cost: 0.0,
                });
                return (buf, Ok(()));
            }
            Err(e) => {
                return (buf, Err(e.to_string()));
            }
            _ => {}
        }
    }
    (buf, Ok(()))
}

pub fn spawn_agent<M, P>(
    agent: Agent<M, P>,
    prompt: String,
    history: Vec<Message>,
    cache: ToolCache,
) -> AgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    cache.clear();
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);

    tokio::spawn(async move {
        let policy = RecoveryPolicy::default();
        let mut attempts = 0;

        loop {
            let (buf, result) = run_stream(&agent, &prompt, history.clone()).await;

            match result {
                Ok(()) => {
                    buf.flush(&event_tx);
                    break;
                }
                Err(msg) => {
                    let kind = recovery::classify_error(&msg);

                    // Auth and unknown errors surface immediately
                    if kind == ErrorKind::Auth || kind == ErrorKind::Other {
                        buf.flush(&event_tx);
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(msg)))
                            .await;
                        break;
                    }

                    // Context-length errors: not retryable without compaction
                    // Surface a helpful error hinting at /compress
                    if kind == ErrorKind::ContextLength {
                        buf.flush(&event_tx);
                        let hint = format!(
                            "{} — try /compress to compact the conversation, then retry",
                            msg
                        );
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(hint)))
                            .await;
                        break;
                    }

                    // If any tool calls were dispatched, their side effects already
                    // executed. Retrying would re-run them. Flush the partial
                    // buffer and surface the error instead.
                    if buf.had_tool_calls {
                        buf.flush(&event_tx);
                        let err =
                            format!("{} (tool side effects already applied, not retrying)", msg);
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(err)))
                            .await;
                        break;
                    }

                    if !policy.should_retry(attempts, kind) {
                        let retry_msg = format!("{} (retries exhausted)", msg);
                        let _ = event_tx
                            .send(AgentEvent::Error(CompactString::new(retry_msg)))
                            .await;
                        break;
                    }

                    // Emit retry notification as reasoning
                    let retry_msg = format!(
                        "retrying ({kind:?} error, attempt {attempt}/{max})...",
                        kind = kind,
                        attempt = attempts + 1,
                        max = policy.max_retries(),
                    );
                    let _ = event_tx
                        .send(AgentEvent::Reasoning(CompactString::new(retry_msg)))
                        .await;

                    let delay = policy.backoff_duration(attempts);
                    tokio::time::sleep(delay).await;
                    attempts += 1;
                }
            }
        }
    });

    AgentRunner { event_rx }
}

pub async fn run_print<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    max_turns: usize,
) -> anyhow::Result<String>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut stream = agent
        .stream_chat(prompt.to_string(), Vec::<Message>::new())
        .multi_turn(max_turns)
        .await;

    let mut full_response = String::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                full_response.push_str(&text.text);
                print!("{}", text.text);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                eprint!("{}", r.display_text());
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    println!();
    Ok(full_response)
}

//! Host-side `logging` and `types` implementations for all three plugin worlds.
//!
//! The `log-record` host import must never block the calling executor thread:
//! the wall-clock deadline around every guest export is cooperative and can
//! only fire while the Wasmtime future yields, so a stalled stderr consumer or
//! JSONL writer inside this import would let an export overrun
//! `plugins.limits.call_timeout_ms` and skip the store-discard path. Records
//! are therefore handed to a bounded queue and written by a dedicated host
//! thread; when the queue is full the newest record is dropped and the drop is
//! counted and later reported from the drain side. This matches the WIT
//! contract that `log-record` is fire-and-forget.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use zeroclaw_log::{Action, Event, EventOutcome, record};

use crate::component::PluginState;
use crate::component::bindings;
use crate::instance::PluginInstanceId;

fn plugin_log_attrs(
    instance: &PluginInstanceId,
    fn_name: String,
    raw_attrs: Option<String>,
) -> serde_json::Value {
    let mut attrs = serde_json::json!({
        "plugin": instance.package(),
        "plugin_capability": instance.capability(),
        "plugin_binding": instance.binding(),
        "plugin_fn": fn_name,
    });
    if let Some(raw) = raw_attrs {
        attrs["raw"] = serde_json::Value::String(raw);
    }
    attrs
}

/// One guest-emitted log record, owned so it can cross to the drain thread.
struct QueuedPluginLog {
    instance: PluginInstanceId,
    level_idx: u8,
    fn_name: String,
    action: Action,
    outcome: EventOutcome,
    duration_ms: Option<u64>,
    raw_attrs: Option<String>,
    msg: String,
}

/// Bound chosen so a chatty guest can burst without loss while a wedged
/// consumer caps host memory at roughly one queue of records.
const PLUGIN_LOG_QUEUE_CAPACITY: usize = 1024;

/// Records dropped because the queue was full (or its drain thread never
/// started). Incremented on the enqueue side, reported from the drain side,
/// where blocking on the writer is allowed.
static DROPPED_PLUGIN_LOGS: AtomicU64 = AtomicU64::new(0);

fn plugin_log_queue() -> &'static SyncSender<QueuedPluginLog> {
    static QUEUE: OnceLock<SyncSender<QueuedPluginLog>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = sync_channel(PLUGIN_LOG_QUEUE_CAPACITY);
        // If the drain thread cannot spawn, the receiver is dropped and every
        // enqueue lands in the drop counter: logging degrades but a guest
        // export still cannot block, which is the invariant that matters.
        let _ = std::thread::Builder::new()
            .name("zc-plugin-log".to_string())
            .spawn(move || drain_plugin_logs(&rx));
        tx
    })
}

/// Enqueue without ever blocking the caller. Full queue drops the newest
/// record; the drain thread reports the accumulated drop count on its own
/// schedule so the loss is observable without re-entering the blocked path.
fn enqueue_plugin_log(log: QueuedPluginLog) {
    if plugin_log_queue().try_send(log).is_err() {
        DROPPED_PLUGIN_LOGS.fetch_add(1, Ordering::Relaxed);
    }
}

fn drain_plugin_logs(rx: &Receiver<QueuedPluginLog>) {
    let mut reported_drops = 0_u64;
    while let Ok(log) = rx.recv() {
        do_log_record(&log);
        let dropped = DROPPED_PLUGIN_LOGS.load(Ordering::Relaxed);
        if dropped > reported_drops {
            record!(
                WARN,
                Event::new(module_path!(), Action::Skip)
                    .with_outcome(EventOutcome::Failure)
                    .with_attrs(serde_json::json!({
                        "newly_dropped": dropped - reported_drops,
                        "total_dropped": dropped,
                    })),
                "plugin log queue overflowed; newest records were dropped"
            );
            reported_drops = dropped;
        }
    }
}

fn do_log_record(log: &QueuedPluginLog) {
    let mut ev = Event::new(module_path!(), log.action).with_outcome(log.outcome);
    if let Some(ms) = log.duration_ms {
        ev = ev.with_duration(ms);
    }
    ev = ev.with_attrs(plugin_log_attrs(
        &log.instance,
        log.fn_name.clone(),
        log.raw_attrs.clone(),
    ));
    let msg = log.msg.clone();
    match log.level_idx {
        0 => record!(TRACE, ev, msg),
        1 => record!(DEBUG, ev, msg),
        2 => record!(INFO, ev, msg),
        3 => record!(WARN, ev, msg),
        _ => record!(ERROR, ev, msg),
    }
}

macro_rules! impl_host {
    ($world:ident) => {
        impl bindings::$world::zeroclaw::plugin::types::Host for PluginState {}

        impl bindings::$world::zeroclaw::plugin::logging::Host for PluginState {
            async fn log_record(
                &mut self,
                level: bindings::$world::zeroclaw::plugin::logging::LogLevel,
                event: bindings::$world::zeroclaw::plugin::logging::PluginEvent,
            ) {
                use bindings::$world::zeroclaw::plugin::logging::{
                    LogLevel, PluginAction, PluginOutcome,
                };
                let action = match event.action {
                    PluginAction::Start => Action::Start,
                    PluginAction::Complete => Action::Complete,
                    PluginAction::Fail => Action::Fail,
                    PluginAction::Cancel => Action::Cancel,
                    PluginAction::Skip => Action::Skip,
                    PluginAction::Timeout => Action::Timeout,
                    PluginAction::Retry => Action::Retry,
                    PluginAction::Inbound => Action::Inbound,
                    PluginAction::Outbound => Action::Outbound,
                    PluginAction::Send => Action::Send,
                    PluginAction::Receive => Action::Receive,
                    PluginAction::Connect => Action::Connect,
                    PluginAction::Disconnect => Action::Disconnect,
                    PluginAction::Reconnect => Action::Reconnect,
                    PluginAction::Spawn => Action::Spawn,
                    PluginAction::Kill => Action::Kill,
                    PluginAction::Tick => Action::Tick,
                    PluginAction::Trigger => Action::Trigger,
                    PluginAction::Schedule => Action::Schedule,
                    PluginAction::Approve => Action::Approve,
                    PluginAction::Reject => Action::Reject,
                    PluginAction::Defer => Action::Defer,
                    PluginAction::Read => Action::Read,
                    PluginAction::Write => Action::Write,
                    PluginAction::Delete => Action::Delete,
                    PluginAction::ListAction => Action::List,
                    PluginAction::Query => Action::Query,
                    PluginAction::Invoke => Action::Invoke,
                    PluginAction::Dispatch => Action::Dispatch,
                    PluginAction::Resolve => Action::Resolve,
                    PluginAction::Register => Action::Register,
                    PluginAction::Unregister => Action::Unregister,
                    PluginAction::Load => Action::Load,
                    PluginAction::Save => Action::Save,
                    PluginAction::Migrate => Action::Migrate,
                    PluginAction::Validate => Action::Validate,
                    PluginAction::MemoryAudit => Action::MemoryAudit,
                    PluginAction::Note => Action::Note,
                };
                let outcome = match event.outcome {
                    Some(PluginOutcome::Success) => EventOutcome::Success,
                    Some(PluginOutcome::Failure) => EventOutcome::Failure,
                    None => EventOutcome::Unknown,
                };
                let level_idx = match level {
                    LogLevel::Trace => 0,
                    LogLevel::Debug => 1,
                    LogLevel::Info => 2,
                    LogLevel::Warn => 3,
                    LogLevel::Error => 4,
                };
                // Hand off instead of writing inline: see the module docs for
                // why this import must never block the executor thread.
                enqueue_plugin_log(QueuedPluginLog {
                    instance: self.scope().id().clone(),
                    level_idx,
                    fn_name: event.function_name,
                    action,
                    outcome,
                    duration_ms: event.duration_ms,
                    raw_attrs: event.attrs,
                    msg: event.message,
                });
            }
        }
    };
}

impl_host!(tool);
impl_host!(channel);
impl_host!(memory);

impl bindings::channel::zeroclaw::plugin::inbound::Host for PluginState {
    async fn inbound_poll(
        &mut self,
    ) -> Option<bindings::channel::zeroclaw::plugin::inbound::HostInboundMessage> {
        self.inbound().poll().map(|m| {
            bindings::channel::zeroclaw::plugin::inbound::HostInboundMessage {
                id: m.id,
                sender: m.sender,
                reply_target: m.reply_target,
                content: m.content,
                channel: m.channel,
                channel_alias: m.channel_alias,
                timestamp: m.timestamp,
                thread_ts: m.thread_ts,
                interruption_scope_id: m.interruption_scope_id,
                subject: m.subject,
            }
        })
    }

    async fn inbound_pending(&mut self) -> u32 {
        self.inbound().pending()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginCapability;

    const LOGGING_WIT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../wit/v0/logging.wit"
    ));

    #[test]
    fn wit_plugin_actions_cover_log_action_taxonomy() {
        let (_, after_enum) = LOGGING_WIT
            .split_once("enum plugin-action {")
            .expect("logging WIT must define plugin-action");
        let (action_body, _) = after_enum
            .split_once('}')
            .expect("plugin-action must have a closing brace");

        macro_rules! assert_actions {
            ($( $variant:ident => $wit_name:literal ),+ $(,)?) => {
                fn wit_name(action: Action) -> &'static str {
                    match action {
                        $(Action::$variant => $wit_name),+
                    }
                }

                $(
                    let name = wit_name(Action::$variant);
                    assert!(
                        action_body
                            .lines()
                            .any(|line| line.trim() == concat!($wit_name, ",")),
                        "plugin-action is missing {name}"
                    );
                )+
            };
        }

        assert_actions!(
            Start => "start",
            Complete => "complete",
            Fail => "fail",
            Cancel => "cancel",
            Skip => "skip",
            Timeout => "timeout",
            Retry => "retry",
            Inbound => "inbound",
            Outbound => "outbound",
            Send => "send",
            Receive => "receive",
            Connect => "connect",
            Disconnect => "disconnect",
            Reconnect => "reconnect",
            Spawn => "spawn",
            Kill => "kill",
            Tick => "tick",
            Trigger => "trigger",
            Schedule => "schedule",
            Approve => "approve",
            Reject => "reject",
            Defer => "defer",
            Read => "read",
            Write => "write",
            Delete => "delete",
            List => "list-action",
            Query => "query",
            Invoke => "invoke",
            Dispatch => "dispatch",
            Resolve => "resolve",
            Register => "register",
            Unregister => "unregister",
            Load => "load",
            Save => "save",
            Migrate => "migrate",
            Validate => "validate",
            MemoryAudit => "memory-audit",
            Note => "note",
        );
    }

    #[test]
    fn host_log_attributes_are_issued_from_the_instance_identity() {
        let scope = crate::instance::test_scope(PluginCapability::Channel, "support", []);
        let attrs = plugin_log_attrs(scope.id(), "poll".to_string(), Some("guest".to_string()));

        assert_eq!(attrs["plugin"], "fixture");
        assert_eq!(attrs["plugin_capability"], "channel");
        assert_eq!(attrs["plugin_binding"], "support");
        assert_eq!(attrs["plugin_fn"], "poll");
        assert_eq!(attrs["raw"], "guest");
    }
}

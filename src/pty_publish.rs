use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::context::Store;

use super::Captured;

pub(super) fn output_capture_limit(config: &crate::config::ContextConfig) -> usize {
    if !config.capture_output {
        return 0;
    }
    config
        .max_output_bytes
        .min(config.ignore_output_over_bytes)
        .min(super::MAX_CAPTURE_OUTPUT_BYTES)
}

pub(super) fn publish(store: &Arc<Mutex<Store>>, cap: Captured, forward: bool) {
    if cap.command.trim().is_empty() {
        return;
    }
    let config = if forward {
        match crate::config::Config::load() {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::warn!(%error, "could not load configuration before publishing captured context");
                return;
            }
        }
    } else {
        None
    };
    publish_with_config(
        store,
        cap,
        forward,
        config.as_ref(),
        |request, timeout, autostart| crate::client::ask(request, timeout, autostart).map(|_| ()),
    );
}

fn publish_with_config(
    store: &Arc<Mutex<Store>>,
    cap: Captured,
    forward: bool,
    config: Option<&crate::config::Config>,
    ask: impl FnOnce(crate::protocol::Request, Duration, bool) -> crate::Result<()>,
) {
    if cap.command.trim().is_empty() {
        return;
    }
    let local_config = (!forward).then(|| store.lock().config().clone());
    let current_config = if forward {
        let Some(config) = config else { return };
        if !config.terminal_enabled() {
            return;
        }
        &config.context
    } else {
        local_config.as_ref().expect("local config is present")
    };
    let output = output_for_publish(&cap.output, current_config);
    store.lock().record_with_output_and_cwd(
        &cap.command,
        cap.exit_code,
        output.as_deref().unwrap_or_default(),
        cap.cwd.as_deref().map(std::path::Path::new),
    );
    if forward {
        let config = config.expect("forwarding requires a loaded config");
        let request = record_request(&cap, output);
        let timeout = Duration::from_millis(config.daemon.connect_timeout_ms.max(500));
        if let Err(error) = ask(request, timeout, config.daemon.autostart) {
            tracing::warn!(%error, "could not publish captured terminal context");
        }
    }
}

fn output_for_publish(output: &str, config: &crate::config::ContextConfig) -> Option<String> {
    if !config.capture_output {
        return None;
    }
    let redacted = crate::redact::scrub(output);
    Some(super::stream::truncate_output_tail(
        &redacted,
        output_capture_limit(config),
    ))
}

fn record_request(cap: &Captured, output: Option<String>) -> crate::protocol::Request {
    crate::protocol::Request::Record {
        id: 0,
        command: cap.command.clone(),
        cwd: cap.cwd.clone(),
        exit_code: cap.exit_code,
        output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(enabled: bool, context: crate::config::ContextConfig) -> crate::config::Config {
        crate::config::Config {
            enabled: Some(enabled),
            disabled_terminals: Vec::new(),
            context,
            ..crate::config::Config::default()
        }
    }

    fn captured(output: &str) -> Captured {
        Captured {
            command: "tool output".into(),
            exit_code: Some(0),
            output: output.into(),
            ..Captured::default()
        }
    }

    #[test]
    fn disabled_config_drops_capture_without_recording_or_contacting_daemon() {
        let config = config(false, crate::config::ContextConfig::default());
        let store = Arc::new(Mutex::new(Store::new(config.context.clone())));
        let attempts = std::cell::Cell::new(0);

        publish_with_config(
            &store,
            captured("secret output"),
            true,
            Some(&config),
            |_, _, _| {
                attempts.set(attempts.get() + 1);
                Ok(())
            },
        );

        assert!(store.lock().is_empty());
        assert_eq!(attempts.get(), 0);
    }

    #[test]
    fn configuration_load_failure_drops_capture_without_recording_or_contacting_daemon() {
        let store = Arc::new(Mutex::new(Store::new(
            crate::config::ContextConfig::default(),
        )));
        let attempts = std::cell::Cell::new(0);

        publish_with_config(&store, captured("secret output"), true, None, |_, _, _| {
            attempts.set(attempts.get() + 1);
            Ok(())
        });

        assert!(store.lock().is_empty());
        assert_eq!(attempts.get(), 0);
    }

    #[test]
    fn enabled_config_records_and_forwards_using_the_current_output_limit() {
        let context = crate::config::ContextConfig {
            max_output_bytes: 16,
            ignore_output_over_bytes: 32,
            ..crate::config::ContextConfig::default()
        };
        let config = config(true, context.clone());
        let store = Arc::new(Mutex::new(Store::new(context)));
        let mut forwarded = None;
        let source = "discarded prefix OMP_SESSION_UUID=123456789";
        let expected = output_for_publish(source, &config.context).unwrap();

        publish_with_config(
            &store,
            captured(source),
            true,
            Some(&config),
            |request, timeout, autostart| {
                assert!(timeout >= Duration::from_millis(500));
                assert_eq!(autostart, config.daemon.autostart);
                forwarded = Some(request);
                Ok(())
            },
        );

        assert_eq!(expected.len(), 16);
        assert!(expected.ends_with("UUID=123456789"));
        let entry = store.lock().entries().next_back().unwrap().clone();
        assert_eq!(entry.output.join("\n"), expected);
        let crate::protocol::Request::Record { output, .. } = forwarded.unwrap() else {
            panic!("publish must forward a record request");
        };
        assert_eq!(output.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn current_capture_output_setting_overrides_the_store_snapshot() {
        let cached = crate::config::ContextConfig::default();
        let current = crate::config::ContextConfig {
            capture_output: false,
            ..cached.clone()
        };
        let config = config(true, current);
        let store = Arc::new(Mutex::new(Store::new(cached)));
        let mut forwarded = None;

        publish_with_config(
            &store,
            captured("old private output"),
            true,
            Some(&config),
            |request, _, _| {
                forwarded = Some(request);
                Ok(())
            },
        );

        let entry = store.lock().entries().next_back().unwrap().clone();
        assert!(entry.output.is_empty());
        let crate::protocol::Request::Record { output, .. } = forwarded.unwrap() else {
            panic!("publish must forward a record request");
        };
        assert!(output.is_none());
    }

    #[test]
    fn local_non_forwarding_recording_does_not_need_saved_config() {
        let context = crate::config::ContextConfig::default();
        let store = Arc::new(Mutex::new(Store::new(context)));

        publish_with_config(&store, captured("local output"), false, None, |_, _, _| {
            panic!("local recording must not contact the daemon")
        });

        assert_eq!(
            store.lock().entries().next_back().unwrap().command,
            "tool output"
        );
    }

    #[test]
    fn output_tail_honors_ignore_limit_and_capture_off() {
        let ignore_limited = crate::config::ContextConfig {
            max_output_bytes: 32,
            ignore_output_over_bytes: 8,
            ..crate::config::ContextConfig::default()
        };
        assert_eq!(
            output_for_publish("long old output and final", &ignore_limited).unwrap(),
            "nd final"
        );
        assert_eq!(output_capture_limit(&ignore_limited), 8);

        let disabled = crate::config::ContextConfig {
            capture_output: false,
            ..crate::config::ContextConfig::default()
        };
        assert_eq!(output_capture_limit(&disabled), 0);
        assert!(output_for_publish("private terminal output", &disabled).is_none());
    }
}

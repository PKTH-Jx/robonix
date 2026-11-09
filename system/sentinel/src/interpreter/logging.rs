//! Best-effort check logging without synchronous writes on the evaluation path.

use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{SyncSender, sync_channel},
};

use robonix_scribe::Level;

use super::adapter::InputValue;

const QUEUE_CAPACITY: usize = 1024;
static DROPPED: AtomicU64 = AtomicU64::new(0);
static RECORDS: LazyLock<Option<SyncSender<(Level, String)>>> = LazyLock::new(|| {
    let (send, receive) = sync_channel::<(Level, String)>(QUEUE_CAPACITY);
    match std::thread::Builder::new()
        .name("sentinel-log".into())
        .spawn(move || {
            for (level, message) in receive {
                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    robonix_scribe::warn!("[sentinel] dropped {dropped} log records: queue full");
                }
                robonix_scribe::log(level, robonix_scribe::default_tag(), &message);
            }
        }) {
        Ok(_) => Some(send),
        Err(error) => {
            robonix_scribe::warn!("[sentinel] could not start log writer: {error}");
            None
        }
    }
});

pub(super) fn log(level: Level, message: String) {
    if let Some(send) = RECORDS.as_ref()
        && send.try_send((level, message)).is_err()
    {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

// Bound strings may contain file contents, credentials, or personal paths.
pub(super) fn value(value: &InputValue) -> &dyn std::fmt::Debug {
    match value {
        InputValue::String(_) => &"<redacted>",
        value => value,
    }
}

pub(super) fn error(error: &super::evaluator::Error) -> String {
    match error {
        // Transport failures may contain arbitrary provider response text.
        super::evaluator::Error::Input(input) => format!(
            "input acquisition failed: input={:?} source={:?}",
            input.input, input.capability
        ),
        error => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_string_contents_and_preserves_motion_measurements() {
        let secret = InputValue::String("test-credential-value".into());
        assert_eq!(format!("{:?}", value(&secret)), "\"<redacted>\"");
        assert_eq!(
            format!("{:?}", value(&InputValue::Number(0.02))),
            "Number(0.02)"
        );
        assert_eq!(
            format!("{:?}", value(&InputValue::Boolean(false))),
            "Boolean(false)"
        );
    }

    #[test]
    fn omits_provider_error_payloads_but_keeps_the_failed_input() {
        use super::super::adapter::{InputError, InputErrorKind, transport};

        let failure = super::super::evaluator::Error::Input(InputError {
            rule: "guard".into(),
            input: "ready".into(),
            capability: "sensor/state".into(),
            kind: InputErrorKind::Acquisition(transport::Error::Mcp(anyhow::anyhow!(
                "test-provider-credential-value"
            ))),
        });
        let message = error(&failure);
        assert!(!message.contains("test-provider-credential-value"));
        assert!(message.contains("ready"));
        assert!(message.contains("sensor/state"));
    }
}

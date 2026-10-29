//! Panic hook: keeps the previous hook's output (the crash reporter and the default hook) and
//! queues one `panic` report.

use std::sync::OnceLock;

use serde_json::json;

use super::Trigger;

const BACKTRACE_CHARS: usize = 6000;

pub fn install() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The crash hook logs the panic at ERROR: suppress our layer while it runs so the
        // panic is reported once (as `panic`, not also as a log trigger).
        super::PANIC_HOOK.with(|flag| flag.set(true));
        previous(info);
        super::PANIC_HOOK.with(|flag| flag.set(false));

        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<dyn Any>".into());
        let location = info
            .location()
            .map(ToString::to_string)
            .unwrap_or_else(|| "<unknown>".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let backtrace = truncate(backtrace, BACKTRACE_CHARS);
        let _ = super::report_observed(
            Trigger::Panic {
                location,
                message,
                thread,
            },
            Vec::new(),
            json!({ "backtrace": backtrace }),
        );
    }));
}

fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

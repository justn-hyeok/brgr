use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(crate) struct Context {
    pub(crate) session: Option<String>,
    pub(crate) pane: Option<String>,
    pub(crate) headless: bool,
}

static CURRENT: OnceLock<Context> = OnceLock::new();
static HOOK: OnceLock<Context> = OnceLock::new();

pub(crate) fn initialize(session: Option<String>, pane: Option<String>, headless: bool) {
    let _ = CURRENT.set(Context {
        session,
        pane,
        headless,
    });
}

pub(crate) fn current() -> &'static Context {
    if let Some(context) = HOOK.get() {
        return context;
    }
    CURRENT.get_or_init(Context::default)
}

pub(crate) fn capture_hook(session: String, pane: Option<String>) {
    let _ = HOOK.set(Context {
        session: Some(session),
        pane,
        headless: false,
    });
}

pub(crate) fn is_hook() -> bool {
    IS_HOOK.load(Ordering::Relaxed)
}
static IS_HOOK: AtomicBool = AtomicBool::new(false);
pub(crate) fn mark_hook() {
    IS_HOOK.store(true, Ordering::Relaxed);
}

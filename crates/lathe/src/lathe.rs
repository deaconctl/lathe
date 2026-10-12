//! Umbrella initialization for Lathe's feature crates.
//!
//! Each application entry point (`main.rs`, the `zed.rs` test harness, the
//! visual test runner) previously listed the individual Lathe crate `init`
//! calls inline. Those lists drifted out of sync (production omitted
//! `mobile_dev::init`, so mobile-dev actions never registered outside tests;
//! the test paths omitted `git_graph::init`). Routing every entry point through
//! [`init`] keeps the set consistent and collapses each upstream call site to a
//! single Lathe line.
//!
//! This crate sits *above* the feature crates: it depends on them so it can call
//! their `init`s. It deliberately is not part of [`lathe_hooks`], which stays a
//! `gpui`-only leaf so feature crates can depend on it to register hooks without
//! forming a dependency cycle.

mod module_tripwire;

use gpui::{App, AppContext as _, DismissEvent};
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

/// Initialize every Lathe feature. Call once, early in application startup,
/// wherever upstream initializes its own subsystems.
///
/// Ordering among these is not significant: each `init` only registers actions,
/// observers, hooks, or serializable items. All are safe to run in headless and
/// test contexts.
pub fn init(cx: &mut App) {
    lathe_hooks::init(cx);
    pr_ui::init(cx);
    git_graph::init(cx);
    mobile_dev::init(cx);
    report_data_import(cx);
}

/// The import from Zed's folders runs before logging starts, so its messages
/// are logged here, and any account that needs a fresh sign-in is named in a
/// notification.
fn report_data_import(cx: &mut App) {
    let Some(report) = lathe_data_import::take_report() else {
        return;
    };
    for (level, message) in report.messages {
        log::log!(level, "{message}");
    }
    if report.accounts_needing_sign_in.is_empty() {
        return;
    }

    struct AccountsNeedSignIn;
    let message = format!(
        "Sign in again to {}. Lathe now keeps its data separate from Zed, and this \
         sign-in couldn't be carried over.",
        report.accounts_needing_sign_in.join(", ")
    );
    show_app_notification(
        NotificationId::unique::<AccountsNeedSignIn>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(move |cx| {
                MessageNotification::new(message, cx)
                    .primary_message("Manage AI Accounts")
                    .primary_on_click(|window, cx| {
                        match cx.build_action("agent::ManageAiAccounts", None) {
                            Ok(action) => window.dispatch_action(action, cx),
                            Err(error) => log::error!("opening Manage AI Accounts: {error}"),
                        }
                        cx.emit(DismissEvent);
                    })
            })
        },
    );
}

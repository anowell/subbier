//! subbier's macOS menu bar app: a status item, no dock icon, no window —
//! `setActivationPolicy(Accessory)` is `LSUIElement = 1` at runtime, so it needs no bundle.
//!
//! muda and tray-icon handles are `!Send`: the main thread owns every AppKit handle, and
//! tokio hands it plain data across `DispatchQueue::main()`.

#![cfg_attr(not(target_os = "macos"), allow(unused))]

#[cfg(target_os = "macos")]
mod env;
#[cfg(target_os = "macos")]
mod icon;
#[cfg(target_os = "macos")]
mod login_item;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod pasteboard;
#[cfg(target_os = "macos")]
mod ui;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Subbier's menu bar app is macOS-only; use the `subbier` CLI instead.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    use dispatch2::DispatchQueue;
    use libsubby::{Command, Engine};
    use muda::MenuEvent;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use tray_icon::TrayIconEvent;

    // EnvFilter matches targets exactly, so this crate's own target has to be named.
    let filter = std::env::var(libsubby::logging::FILTER_ENV).unwrap_or_else(|_| {
        format!(
            "{},{}=info",
            libsubby::logging::DEFAULT_FILTER,
            env!("CARGO_CRATE_NAME")
        )
    });
    // NAMED variable: `let _ = ...` drops the guard and discards every file log.
    let _guard = libsubby::logging::init(
        Some(&libsubby::store::home().join("logs")),
        Some(&filter),
        libsubby::logging::Console::Stderr,
    );

    let runtime = tokio::runtime::Runtime::new().expect("could not start the tokio runtime");

    // A second engine would poll both providers and write the same `state.db` behind a
    // menu bar item that looks healthy — so refuse, loudly enough for launchd to log it.
    if let Some(other) = runtime.block_on(async {
        let config = libsubby::Config::load_from(&libsubby::store::home().join("config.kdl"))
            .unwrap_or_default();
        libsubby::instance::probe(&config)
            .await
            .map(|snap| (config.proxy.bind.clone(), snap))
    }) {
        let (bind, snap) = other;
        let who = snap.proxy.pid.map_or_else(
            || "a subbier".to_owned(),
            |pid| format!("subbier (pid {pid})"),
        );
        tracing::error!(%bind, "{who} already owns the proxy port");
        eprintln!(
            "subbier: {who} is already running on {bind}.\n\
             Stop it first, or manage the background one with `subbier service`."
        );
        std::process::exit(1);
    }

    let handle = match runtime.block_on(Engine::new()) {
        Ok((engine, handle)) => {
            runtime.spawn(async move {
                if let Err(error) = engine.run().await {
                    tracing::error!(%error, "engine stopped");
                }
            });
            handle
        }
        Err(error) => {
            tracing::error!(%error, "could not start the engine");
            eprintln!("subbier: could not start: {error}");
            std::process::exit(1);
        }
    };

    let mut snapshots = handle.subscribe();
    runtime.spawn(async move {
        while snapshots.changed().await.is_ok() {
            let snapshot = snapshots.borrow_and_update().clone();
            DispatchQueue::main().exec_async(move || ui::apply(&snapshot));
        }
    });

    // The bounce through the main queue lets the handler stay `Send + Sync`.
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        DispatchQueue::main().exec_async(move || ui::on_menu_event(&event.id));
    }));

    // Hovering is the moment before a click, so refresh then — unforced, which respects
    // the poller's own cadence.
    let hover_handle = handle.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(event, TrayIconEvent::Enter { .. }) {
            hover_handle.send(Command::RefreshUsage { force: false });
        }
    }));

    let mtm = MainThreadMarker::new().expect("main() is not running on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // tray-icon needs the run loop *running*, not merely created, before the status item
    // is built — so this fires on its first turn.
    let ui_handle = handle;
    let rt = runtime.handle().clone();
    DispatchQueue::main().exec_async(move || ui::install(ui_handle, rt));

    // `app.run()` never returns, so the runtime must not be dropped out from under it.
    std::mem::forget(runtime);
    app.run();
}

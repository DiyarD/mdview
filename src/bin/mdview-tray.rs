// mdview-tray — ~1MB helper that owns a REAL global hotkey via RegisterHotKey
// and a tray icon. Zero webview, zero async runtime, zero CPU while idle
// (all waits are blocking). Launches mdview.exe on hotpress.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpListener;
use std::process::Command;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

#[path = "../appicon.rs"]
mod appicon;

const TRAY_PORT: &str = "127.0.0.1:17843";

#[derive(Debug)]
enum TE {
    Hotkey,
    Open,
    Quit,
}

fn combos() -> Vec<(Modifiers, Code, &'static str)> {
    vec![
        (Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyM, "Ctrl+Shift+M"),
        (Modifiers::CONTROL | Modifiers::ALT, Code::KeyM, "Ctrl+Alt+M"),
        (Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyJ, "Ctrl+Shift+J"),
    ]
}

fn main() {
    // Single tray instance.
    let _guard = match TcpListener::bind(TRAY_PORT) {
        Ok(l) => l,
        Err(_) => return,
    };

    let event_loop = EventLoopBuilder::<TE>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Real OS-level global hotkey (fails loudly if another app owns the combo).
    let manager = GlobalHotKeyManager::new().expect("hotkey manager");
    let mut active_label = "none";
    let mut active_id: Option<u32> = None;
    for (mods, code, label) in combos() {
        let hk = HotKey::new(Some(mods), code);
        if manager.register(hk).is_ok() {
            active_label = label;
            active_id = Some(hk.id());
            break;
        }
    }
    let active_id = active_id.expect("no free hotkey combo (all taken?)");

    // Forward hotkey presses (blocking recv => 0% CPU).
    let p2 = proxy.clone();
    std::thread::spawn(move || {
        let rx = GlobalHotKeyEvent::receiver();
        while let Ok(ev) = rx.recv() {
            if ev.id == active_id {
                let _ = p2.send_event(TE::Hotkey);
            }
        }
    });

    // Tray icon + menu.
    let menu = Menu::new();
    let open = MenuItem::new("Open mdview", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::new("Quit tray helper", true, None);
    menu.append_items(&[&open, &sep, &quit]).expect("menu");
    let tooltip = format!("mdview — press {} anywhere", active_label);
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .with_icon(make_icon())
        .build()
        .expect("tray");

    let p3 = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |ev| match ev {
        TrayIconEvent::Click { button, button_state, .. } => {
            if button == MouseButton::Left && button_state == MouseButtonState::Up {
                let _ = p3.send_event(TE::Open);
            }
        }
        _ => {}
    }));
    let p4 = proxy.clone();
    let open_id = open.id().clone();
    let quit_id = quit.id().clone();
    MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
        if ev.id == open_id {
            let _ = p4.send_event(TE::Open);
        } else if ev.id == quit_id {
            let _ = p4.send_event(TE::Quit);
        }
    }));

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {}
            Event::UserEvent(te) => match te {
                TE::Hotkey | TE::Open => launch_mdview(),
                TE::Quit => *control_flow = ControlFlow::Exit,
            },
            _ => {}
        }
    });
}

fn launch_mdview() {
    let mut cmd = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("mdview.exe")))
        .filter(|c| c.exists())
    {
        Some(path) => Command::new(path),
        None => Command::new("mdview.exe"), // PATH fallback
    };
    if let Ok(child) = cmd.spawn() {
        // We own the hotkey press, so lend mdview our foreground rights:
        // its window opens ACTIVE, and the first click lands on the button
        // instead of merely activating the window ("press twice" bug).
        #[cfg(target_os = "windows")]
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
        }
    }
}

/// Tray glyph: same M mark as the main window, on accent blue so the two
/// stay distinguishable at a glance. Built in code, no assets.
fn make_icon() -> Icon {
    Icon::from_rgba(appicon::m_rgba((9, 105, 218), (255, 255, 255), 32), 32, 32).expect("icon")
}

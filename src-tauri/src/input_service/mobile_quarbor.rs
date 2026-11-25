#![cfg_attr(test, allow(dead_code))]
//! Hardware-grade virtual HID output client for Axonkey Mobile on Windows.
//!
//! Connects `SystemMobileBackend` to the standalone Root-enumerated Quarbor
//! output node (`ROOT\QUARBORUNIVERSALKEYBOARDOUTPUT\AXONKEYMOBILE`) over its
//! duplex named pipe (`\\.\pipe\Quarbor.UniversalKeyboard.v1.<FNV1A_HASH>`).
//! Keystrokes and mouse buttons emitted through this channel traverse
//! `mshidumdf.sys` -> `kbdhid.sys` / `mouhid.sys` (`LLKHF_INJECTED == 0`),
//! reliably waking anti-injection global hotkeys such as Doubao's bare-`Alt`
//! hold-to-talk, while seamlessly falling back to Win32 `SendInput` whenever
//! the virtual HID node is not yet installed.

use super::{
    cursor_delta, remote_actions::HidKey, windows::behavior_hid_chord, windows_quarbor::Client,
    MouseButton, NativeBehavior, WheelDirection,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const MOBILE_VHID_INSTANCE_ID: &str =
    r"ROOT\QUARBORUNIVERSALKEYBOARDOUTPUT\AXONKEYMOBILE";

const PUMP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(600);
const HANDSHAKE_BUDGET: Duration = Duration::from_millis(180);

struct OutputChannelState {
    client: Option<Client>,
    held_driver_keys: HashSet<HidKey>,
    last_connect_attempt: Option<Instant>,
}

impl Default for OutputChannelState {
    fn default() -> Self {
        Self {
            client: None,
            held_driver_keys: HashSet::new(),
            last_connect_attempt: None,
        }
    }
}

struct MobileQuarborHub {
    state: Mutex<OutputChannelState>,
    paused: AtomicBool,
    connect_enabled: AtomicBool,
    connected_flag: AtomicBool,
}

static HUB: OnceLock<MobileQuarborHub> = OnceLock::new();

fn hub() -> &'static MobileQuarborHub {
    HUB.get_or_init(|| {
        let hub = MobileQuarborHub {
            state: Mutex::new(OutputChannelState::default()),
            paused: AtomicBool::new(false),
            connect_enabled: AtomicBool::new(false),
            connected_flag: AtomicBool::new(false),
        };
        let _ = thread::Builder::new()
            .name("axonkey-mobile-quarbor".into())
            .spawn(pump_worker);
        hub
    })
}

fn pump_worker() {
    loop {
        let h = hub();
        if h.paused.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        let mut sleep_dur = Duration::from_millis(100);
        if let Ok(mut guard) = h.state.lock() {
            if guard.client.is_some() {
                drain_client(&mut guard, &h.connected_flag);
                if guard.client.is_some() {
                    sleep_dur = PUMP_POLL_INTERVAL;
                }
            } else if h.connect_enabled.load(Ordering::Acquire) {
                let can_try = guard
                    .last_connect_attempt
                    .is_none_or(|last| last.elapsed() >= RECONNECT_BACKOFF);
                if can_try {
                    try_connect_locked(&mut guard, &h.connected_flag);
                    if guard.client.is_some() {
                        sleep_dur = PUMP_POLL_INTERVAL;
                    } else {
                        sleep_dur = Duration::from_millis(250);
                    }
                }
            }
        }
        thread::sleep(sleep_dur);
    }
}

fn drop_client_locked(state: &mut OutputChannelState, connected_flag: &AtomicBool) {
    state.client = None;
    state.held_driver_keys.clear();
    connected_flag.store(false, Ordering::Release);
}

fn drain_client(state: &mut OutputChannelState, connected_flag: &AtomicBool) {
    for _ in 0..32 {
        let Some(client) = state.client.as_mut() else {
            connected_flag.store(false, Ordering::Release);
            return;
        };
        match client.poll() {
            Ok(Some(_)) => {
                connected_flag.store(client.ready(), Ordering::Release);
            }
            Ok(None) => {
                connected_flag.store(client.ready(), Ordering::Release);
                return;
            }
            Err(err) => {
                log::debug!(
                    target: "axonkey::mobile",
                    "Mobile Quarbor virtual HID pipe disconnected: {err}"
                );
                drop_client_locked(state, connected_flag);
                return;
            }
        }
    }
}

fn try_connect_locked(state: &mut OutputChannelState, connected_flag: &AtomicBool) {
    state.last_connect_attempt = Some(Instant::now());
    let mut client = match Client::connect(MOBILE_VHID_INSTANCE_ID) {
        Ok(c) => c,
        Err(_) => {
            connected_flag.store(false, Ordering::Release);
            return;
        }
    };
    let deadline = Instant::now() + HANDSHAKE_BUDGET;
    while !client.ready() && Instant::now() < deadline {
        match client.poll() {
            Ok(Some(_)) => {
                if client.ready() {
                    break;
                }
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(err) => {
                log::warn!(
                    target: "axonkey::mobile",
                    "Mobile Quarbor virtual HID handshake failed: {err}"
                );
                connected_flag.store(false, Ordering::Release);
                return;
            }
        }
    }
    if client.ready() {
        let _ = client.reset_input();
        let _ = client.poll();
        connected_flag.store(true, Ordering::Release);
        log::info!(
            target: "axonkey::mobile",
            "Mobile Quarbor virtual HID output ready: {MOBILE_VHID_INSTANCE_ID}"
        );
        state.held_driver_keys.clear();
        state.client = Some(client);
    } else {
        connected_flag.store(false, Ordering::Release);
    }
}

fn with_ready_client<R>(
    f: impl FnOnce(&mut Client, &mut HashSet<HidKey>) -> std::io::Result<R>,
) -> Option<R> {
    let h = hub();
    if h.paused.load(Ordering::Acquire) {
        return None;
    }
    let mut guard = h.state.lock().unwrap_or_else(|e| e.into_inner());
    if guard.client.is_none() && h.connect_enabled.load(Ordering::Acquire) {
        let can_try = guard
            .last_connect_attempt
            .is_none_or(|last| last.elapsed() >= RECONNECT_BACKOFF);
        if can_try {
            try_connect_locked(&mut guard, &h.connected_flag);
        }
    }
    drain_client(&mut guard, &h.connected_flag);
    let OutputChannelState {
        ref mut client,
        ref mut held_driver_keys,
        ..
    } = *guard;
    let c = client.as_mut().filter(|c| c.ready())?;
    match f(c, held_driver_keys) {
        Ok(val) => {
            let _ = c.poll();
            Some(val)
        }
        Err(err) => {
            log::warn!(
                target: "axonkey::mobile",
                "Mobile Quarbor virtual HID command failed, falling back: {err}"
            );
            drop_client_locked(&mut guard, &h.connected_flag);
            None
        }
    }
}

pub(crate) fn set_connect_enabled(enabled: bool) {
    let h = hub();
    let prev = h.connect_enabled.swap(enabled, Ordering::AcqRel);
    if enabled && !prev && !h.paused.load(Ordering::Acquire) {
        if let Ok(mut guard) = h.state.lock() {
            guard.last_connect_attempt = None;
        }
    }
}

pub(crate) fn is_connected() -> bool {
    hub().connected_flag.load(Ordering::Acquire)
}

pub(crate) fn pause() {
    let h = hub();
    h.paused.store(true, Ordering::Release);
    if let Ok(mut guard) = h.state.lock() {
        if let Some(client) = guard.client.as_mut().filter(|c| c.ready()) {
            let _ = client.reset_input();
            let _ = client.finish_pending(Duration::from_millis(40));
        }
        drop_client_locked(&mut guard, &h.connected_flag);
    }
}

pub(crate) fn resume() {
    let h = hub();
    if let Ok(mut guard) = h.state.lock() {
        guard.last_connect_attempt = None;
    }
    h.paused.store(false, Ordering::Release);
}

pub(crate) fn reset_all_inputs() {
    let _ = with_ready_client(|client, held| {
        held.clear();
        client.reset_input().map(|_| ())
    });
}

fn mouse_button_hid_key(button: MouseButton) -> Option<HidKey> {
    let usage = match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::Back | MouseButton::Forward => return None,
    };
    Some(HidKey { page: 0x09, usage })
}

fn driver_wheel_steps(direction: WheelDirection) -> (i32, i32) {
    match direction {
        WheelDirection::Up => (1, 0),
        WheelDirection::Down => (-1, 0),
        WheelDirection::Left => (0, -1),
        WheelDirection::Right => (0, 1),
    }
}

fn clamp_motion_steps(value: i32) -> impl Iterator<Item = i32> {
    let mut remaining = value;
    std::iter::from_fn(move || {
        if remaining == 0 {
            return None;
        }
        let step = remaining.clamp(-127, 127);
        remaining -= step;
        Some(step)
    })
}

fn send_chunked_mouse_move(
    client: &mut Client,
    dx: i32,
    dy: i32,
    wheel: i32,
    pan: i32,
) -> std::io::Result<()> {
    let mut xs = clamp_motion_steps(dx);
    let mut ys = clamp_motion_steps(dy);
    let mut ws = clamp_motion_steps(wheel);
    let mut ps = clamp_motion_steps(pan);
    loop {
        let step = (
            xs.next().unwrap_or(0),
            ys.next().unwrap_or(0),
            ws.next().unwrap_or(0),
            ps.next().unwrap_or(0),
        );
        if step == (0, 0, 0, 0) {
            return Ok(());
        }
        client.move_mouse(step.0, step.1, step.2, step.3)?;
    }
}

pub(super) fn try_key_edge(key: &str, down: bool) -> Result<bool, String> {
    let behavior = NativeBehavior::Key {
        enabled: true,
        key: key.to_owned(),
    };
    let Some(chord) = behavior_hid_chord(&behavior) else {
        return Ok(false);
    };
    if !down {
        let h = hub();
        let mut guard = h.state.lock().unwrap_or_else(|e| e.into_inner());
        let any_held_in_driver = chord.iter().any(|k| guard.held_driver_keys.contains(k));
        if !any_held_in_driver {
            return Ok(false);
        }
        drain_client(&mut guard, &h.connected_flag);
        let OutputChannelState {
            ref mut client,
            ref mut held_driver_keys,
            ..
        } = *guard;
        let Some(c) = client.as_mut().filter(|c| c.ready()) else {
            // Closing the Quarbor pipe already forces the kernel driver to emit neutral all-keys-up reports.
            drop_client_locked(&mut guard, &h.connected_flag);
            return Ok(true);
        };
        for hid in chord.iter().rev() {
            if held_driver_keys.remove(hid) {
                if let Err(err) = c.send_key(hid.page, hid.usage, false) {
                    log::warn!(target: "axonkey::mobile", "Mobile Quarbor key release failed: {err}");
                    drop_client_locked(&mut guard, &h.connected_flag);
                    return Ok(true);
                }
            }
        }
        let _ = c.poll();
        return Ok(true);
    }

    let handled = with_ready_client(|client, held| {
        if !chord.iter().all(|k| client.supports_key(k.page, k.usage)) {
            return Ok(false);
        }
        for hid in &chord {
            if held.insert(*hid) {
                client.send_key(hid.page, hid.usage, true)?;
            }
        }
        Ok(true)
    });
    Ok(handled.unwrap_or(false))
}

pub(super) fn try_mouse_button_edge(button: MouseButton, down: bool) -> Result<bool, String> {
    let Some(hid) = mouse_button_hid_key(button) else {
        return Ok(false);
    };
    if !down {
        let h = hub();
        let mut guard = h.state.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.held_driver_keys.contains(&hid) {
            return Ok(false);
        }
        drain_client(&mut guard, &h.connected_flag);
        let OutputChannelState {
            ref mut client,
            ref mut held_driver_keys,
            ..
        } = *guard;
        held_driver_keys.remove(&hid);
        let Some(c) = client.as_mut().filter(|c| c.ready()) else {
            drop_client_locked(&mut guard, &h.connected_flag);
            return Ok(true);
        };
        if let Err(err) = c.send_key(hid.page, hid.usage, false) {
            log::warn!(target: "axonkey::mobile", "Mobile Quarbor mouse button release failed: {err}");
            drop_client_locked(&mut guard, &h.connected_flag);
        } else {
            let _ = c.poll();
        }
        return Ok(true);
    }

    let handled = with_ready_client(|client, held| {
        if !client.supports_key(hid.page, hid.usage) {
            return Ok(false);
        }
        if held.insert(hid) {
            client.send_key(hid.page, hid.usage, true)?;
        }
        Ok(true)
    });
    Ok(handled.unwrap_or(false))
}

pub(super) fn try_relative_motion(dx: i32, dy: i32) -> bool {
    if dx == 0 && dy == 0 {
        return true;
    }
    with_ready_client(|client, _| {
        if !client.supports_mouse_move() {
            return Ok(false);
        }
        send_chunked_mouse_move(client, dx, dy, 0, 0)?;
        Ok(true)
    })
    .unwrap_or(false)
}

pub(super) fn try_execute_behavior(behavior: &NativeBehavior) -> bool {
    match behavior {
        NativeBehavior::Key { .. } | NativeBehavior::Shortcut { .. } => {
            let Some(chord) = behavior_hid_chord(behavior) else {
                return false;
            };
            if chord.is_empty() {
                return false;
            }
            with_ready_client(|client, _| {
                if !chord.iter().all(|k| client.supports_key(k.page, k.usage)) {
                    return Ok(false);
                }
                if chord.len() == 1 {
                    client.tap_key(chord[0].page, chord[0].usage)?;
                } else {
                    for k in &chord {
                        client.send_key(k.page, k.usage, true)?;
                    }
                    for k in chord.iter().rev() {
                        client.send_key(k.page, k.usage, false)?;
                    }
                }
                Ok(true)
            })
            .unwrap_or(false)
        }
        NativeBehavior::Mouse { button, .. } => {
            let Some(hid) = mouse_button_hid_key(*button) else {
                return false;
            };
            with_ready_client(|client, _| {
                if !client.supports_key(hid.page, hid.usage) {
                    return Ok(false);
                }
                client.tap_key(hid.page, hid.usage)?;
                Ok(true)
            })
            .unwrap_or(false)
        }
        NativeBehavior::CursorMove {
            direction,
            distance,
            ..
        } => {
            let (dx, dy) = cursor_delta(*direction, *distance);
            try_relative_motion(dx, dy)
        }
        NativeBehavior::Wheel { direction, .. } => {
            let (wheel, pan) = driver_wheel_steps(*direction);
            with_ready_client(|client, _| {
                if !client.supports_mouse_move() {
                    return Ok(false);
                }
                send_chunked_mouse_move(client, 0, 0, wheel, pan)?;
                Ok(true)
            })
            .unwrap_or(false)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_doubao_alt_and_shortcuts_to_hid_usages() {
        let alt = behavior_hid_chord(&NativeBehavior::Key {
            enabled: true,
            key: "Alt".into(),
        })
        .expect("Alt HID chord");
        assert_eq!(
            alt,
            vec![HidKey {
                page: 0x07,
                usage: 0xE2
            }]
        );

        let win_h = behavior_hid_chord(&NativeBehavior::Shortcut {
            enabled: true,
            keys: vec!["Win".into(), "H".into()],
        })
        .expect("Win+H HID chord");
        assert_eq!(
            win_h,
            vec![
                HidKey {
                    page: 0x07,
                    usage: 0xE3
                },
                HidKey {
                    page: 0x07,
                    usage: 0x0B
                },
            ]
        );
    }

    #[test]
    fn chunks_large_relative_motion_into_hid_i8_range() {
        let pos: Vec<i32> = clamp_motion_steps(300).collect();
        assert_eq!(pos, vec![127, 127, 46]);

        let neg: Vec<i32> = clamp_motion_steps(-260).collect();
        assert_eq!(neg, vec![-127, -127, -6]);

        let zero: Vec<i32> = clamp_motion_steps(0).collect();
        assert!(zero.is_empty());
    }

    #[test]
    fn maps_primary_mouse_buttons_and_leaves_xbuttons_to_sendinput() {
        assert_eq!(
            mouse_button_hid_key(MouseButton::Left),
            Some(HidKey {
                page: 0x09,
                usage: 1
            })
        );
        assert_eq!(
            mouse_button_hid_key(MouseButton::Right),
            Some(HidKey {
                page: 0x09,
                usage: 2
            })
        );
        assert_eq!(
            mouse_button_hid_key(MouseButton::Middle),
            Some(HidKey {
                page: 0x09,
                usage: 3
            })
        );
        assert_eq!(mouse_button_hid_key(MouseButton::Back), None);
        assert_eq!(mouse_button_hid_key(MouseButton::Forward), None);
    }
}

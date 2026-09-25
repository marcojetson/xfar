//! Input event routing.
//!
//! Backend input events are translated into seat input here. Keyboard events go
//! through the seat's keyboard (xkb/modifier handling); pressed chords are
//! matched against the configured keybinding table ([`crate::keybinding`]) and
//! dispatched to compositor actions, with everything else forwarded to the
//! focused client. Pointer events update the tracked cursor, are delivered to
//! the surface under the cursor, and drive click-to-focus.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::keyboard::{keysyms, FilterResult};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point, Size, SERIAL_COUNTER};

use crate::state::State;

/// Route a single backend input event into the compositor.
pub fn process_input_event<B: InputBackend>(state: &mut State, event: InputEvent<B>) {
    // `B` is passed explicitly: it cannot be inferred from an associated-type
    // argument like `B::KeyboardKeyEvent`.
    match event {
        InputEvent::Keyboard { event } => on_keyboard_key::<B>(state, event),
        InputEvent::PointerMotion { event } => on_pointer_motion::<B>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            on_pointer_motion_absolute::<B>(state, event)
        }
        InputEvent::PointerButton { event } => on_pointer_button::<B>(state, event),
        InputEvent::PointerAxis { event } => on_pointer_axis::<B>(state, event),
        _ => {}
    }
}

fn on_keyboard_key<B: InputBackend>(state: &mut State, event: B::KeyboardKeyEvent) {
    let keycode = event.key_code();
    let key_state = event.state();
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();

    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };

    // Resolve shortcuts with the current modifier state, then forward unmatched
    // keys to the focused client.
    keyboard.input::<(), _>(
        state,
        keycode,
        key_state,
        serial,
        time,
        |state, modifiers, keysym| {
            // Use the layout base keysym so shortcuts are shift-independent
            // (e.g. Shift+2 still resolves to `1`..`N` as `2`, not `@`).
            let bind = keysym
                .raw_latin_sym_or_raw_current_sym()
                .map(|k| k.raw())
                .unwrap_or(0);

            // The launcher captures keyboard input while open.
            if state.launcher.open {
                if key_state == KeyState::Pressed {
                    match bind {
                        keysyms::KEY_Escape => state.launcher.close(),
                        keysyms::KEY_Return | keysyms::KEY_KP_Enter => state.launcher_activate(),
                        keysyms::KEY_BackSpace => state.launcher_backspace(),
                        keysyms::KEY_Up => state.launcher.select_prev(),
                        keysyms::KEY_Down => state.launcher.select_next(),
                        _ => {
                            // Don't insert a character when a command modifier is
                            // held — e.g. pressing Super+D again while searching
                            // must not type a literal 'd'.
                            if !modifiers.logo && !modifiers.ctrl && !modifiers.alt {
                                if let Some(ch) = keysym.modified_sym().key_char() {
                                    if !ch.is_control() {
                                        state.launcher_type(ch);
                                    }
                                }
                            }
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // The settings panel captures keyboard input while open.
            if state.settings.open {
                if key_state == KeyState::Pressed {
                    let out_h = state.output.current_mode().map(|m| m.size.h).unwrap_or(0);
                    let view_rows = crate::layout::settings_view_rows(out_h, state.panel_bottom);
                    if modifiers.ctrl && bind == keysyms::KEY_s {
                        state.settings_save(); // ctrl+s saves (also while editing)
                    } else if state.settings.editing_row().is_some() {
                        match bind {
                            keysyms::KEY_Escape => state.settings.edit_cancel(),
                            keysyms::KEY_BackSpace => state.settings.edit_backspace(),
                            keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                                state.settings.edit_commit()
                            }
                            _ => {
                                // Ignore command-modified keys.
                                if !modifiers.logo && !modifiers.ctrl && !modifiers.alt {
                                    if let Some(ch) = keysym.modified_sym().key_char() {
                                        if !ch.is_control() {
                                            state.settings.edit_char(ch);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        match bind {
                            keysyms::KEY_Escape => state.settings_close(),
                            keysyms::KEY_Up => state.settings.move_selection(-1, view_rows),
                            keysyms::KEY_Down => state.settings.move_selection(1, view_rows),
                            keysyms::KEY_Page_Up => state
                                .settings
                                .move_selection(-(view_rows as i32), view_rows),
                            keysyms::KEY_Page_Down => {
                                state.settings.move_selection(view_rows as i32, view_rows)
                            }
                            keysyms::KEY_Home => state.settings.goto_start(),
                            keysyms::KEY_End => state.settings.goto_end(view_rows),
                            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                                state.settings.activate_selected()
                            }
                            keysyms::KEY_Left => state.settings.apply_counter(-1),
                            keysyms::KEY_Right => state.settings.apply_counter(1),
                            _ => {}
                        }
                    }
                }
                return FilterResult::Intercept(());
            }

            // Match the pressed chord against the configured bindings.
            if key_state == KeyState::Pressed {
                let chord = crate::keybinding::Chord {
                    logo: modifiers.logo,
                    shift: modifiers.shift,
                    alt: modifiers.alt,
                    ctrl: modifiers.ctrl,
                    keysym: bind,
                };
                if let Some(action) = state
                    .keybindings
                    .iter()
                    .find(|(c, _)| *c == chord)
                    .map(|(_, a)| *a)
                {
                    dispatch_action(state, action);
                    return FilterResult::Intercept(());
                }
            }

            // Alt+Tab application switcher: cycle on Tab, commit when Alt is
            // released.
            if key_state == KeyState::Pressed && modifiers.alt && bind == keysyms::KEY_Tab {
                state.switcher_cycle();
                return FilterResult::Intercept(());
            }
            if key_state == KeyState::Released
                && matches!(bind, keysyms::KEY_Alt_L | keysyms::KEY_Alt_R)
            {
                state.switcher_finish();
            }
            tracing::debug!(
                keycode = keycode.raw(),
                state = ?key_state,
                keysym = ?keysym.modified_sym(),
                "keyboard key",
            );
            FilterResult::Forward
        },
    );
}

/// Perform the compositor command bound to a triggered keyboard shortcut.
fn dispatch_action(state: &mut State, command: crate::keybinding::Command) {
    use crate::keybinding::Command;
    match command {
        Command::Launcher => state.launcher_open(),
        Command::GrowWidth => state.resize_focused(true, true),
        Command::ShrinkWidth => state.resize_focused(true, false),
        Command::GrowHeight => state.resize_focused(false, true),
        Command::ShrinkHeight => state.resize_focused(false, false),
        Command::Cycle => state.cycle_windows(),
        Command::Close => state.close_focused_window(),
        Command::Quit => state.running = false,
        Command::NewWorkspace => state.add_workspace(),
        Command::NextWorkspace => state.cycle_workspace(1),
        Command::PrevWorkspace => state.cycle_workspace(-1),
        Command::MoveToNextWorkspace => state.move_focused_to_workspace_rel(1),
        Command::MoveToPrevWorkspace => state.move_focused_to_workspace_rel(-1),
    }
}

/// Accumulate relative pointer motion and clamp the cursor to the output.
fn on_pointer_motion<B: InputBackend>(state: &mut State, event: B::PointerMotionEvent) {
    let mode_size = state
        .output
        .current_mode()
        .map(|m| m.size)
        .unwrap_or_default();
    let mut location = state.pointer_location + event.delta();
    location.x = location.x.clamp(0.0, mode_size.w.max(1) as f64 - 1.0);
    location.y = location.y.clamp(0.0, mode_size.h.max(1) as f64 - 1.0);
    state.pointer_location = location;

    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let focus = state.surface_under(location);
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

fn on_pointer_motion_absolute<B: InputBackend>(
    state: &mut State,
    event: B::PointerMotionAbsoluteEvent,
) {
    let mode_size = state
        .output
        .current_mode()
        .map(|m| m.size)
        .unwrap_or_default();
    let output_size: Size<i32, Logical> = (mode_size.w, mode_size.h).into();

    let location = event.position_transformed(output_size);
    state.pointer_location = location;
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();

    // Deliver motion to the surface under the cursor (if any).
    let focus = state.surface_under(location);
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location,
            serial,
            time,
        },
    );
    pointer.frame(state);
    tracing::debug!(x = location.x, y = location.y, "pointer motion");
}

fn on_pointer_button<B: InputBackend>(state: &mut State, event: B::PointerButtonEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let button_state = event.state();
    let time = event.time_msec();

    // Presses inside the panel's hit-test rectangle are consumed.
    let px = state.pointer_location.x as i32;
    let py = state.pointer_location.y as i32;
    let (width, height) = state
        .output
        .current_mode()
        .map(|m| (m.size.w, m.size.h))
        .unwrap_or((0, 0));
    let bar_top = crate::layout::bar_offset(height, state.panel_bottom).y;
    let in_bar = py >= bar_top && py < bar_top + crate::layout::BAR_HEIGHT;
    if button_state == ButtonState::Pressed && in_bar {
        let point: Point<i32, Logical> = (px, py - bar_top).into();
        if crate::layout::bar_search_rect(width).contains(point) {
            state.launcher_open();
        } else if state.workspaces.len() > 1 {
            // Pips are only clickable when another workspace exists.
            if let Some(i) = crate::layout::bar_pip_rects(width, state.workspaces.len())
                .iter()
                .position(|r| r.contains(point))
            {
                state.switch_workspace(i);
            }
        }
        return;
    }

    // A click while the settings panel is open selects the row under the cursor
    // and closes the panel when clicking anywhere outside it.
    if button_state == ButtonState::Pressed && state.settings.open {
        let point: Point<i32, Logical> = (px, py).into();
        let rect = crate::layout::settings_rect(width, height, state.panel_bottom);
        if rect.contains(point) {
            let rel = (py - rect.loc.y) / crate::settings::SETTINGS_ROW_H;
            let view_rows = crate::layout::settings_view_rows(height, state.panel_bottom);
            if (0..view_rows as i32).contains(&rel) {
                state.settings.select(state.settings.top + rel as usize);
            }
        } else {
            state.settings_close();
        }
        return;
    }

    // A click on a launcher result row selects and activates it (the dropdown
    // sits above any window, so it must not fall through to focus one).
    if button_state == ButtonState::Pressed && state.launcher.open {
        let point: Point<i32, Logical> = (px, py).into();
        if let Some(row) = crate::layout::launcher_row_at(
            point,
            state.launcher.results.len(),
            height,
            state.panel_bottom,
        ) {
            state.launcher.selection = row;
            state.launcher_activate();
            return;
        }
    }

    // Click-to-focus: pressing over a window focuses it. A click also closes
    // the launcher — leaving it open keeps capturing the keyboard, so the
    // clicked window would not receive key input.
    if button_state == ButtonState::Pressed {
        if let Some(window) = state
            .space
            .element_under(state.pointer_location)
            .map(|(w, _)| w.clone())
        {
            if state.launcher.open {
                state.launcher.close();
            }
            state.focus_window(&window, serial);
        }
    }

    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    pointer.button(
        state,
        &ButtonEvent {
            serial,
            time,
            button,
            state: button_state,
        },
    );
    pointer.frame(state);
    tracing::info!(button, state = ?button_state, "pointer button");
}

fn on_pointer_axis<B: InputBackend>(state: &mut State, event: B::PointerAxisEvent) {
    // Vertical scrolling changes the selection while settings are open.
    if state.settings.open {
        if let Some(amount) = event.amount(Axis::Vertical) {
            let out_h = state.output.current_mode().map(|m| m.size.h).unwrap_or(0);
            let view_rows = crate::layout::settings_view_rows(out_h, state.panel_bottom);
            let delta = if amount >= 0.5 {
                1
            } else if amount <= -0.5 {
                -1
            } else {
                0
            };
            state.settings.move_selection(delta, view_rows);
        }
        return;
    }

    let source = event.source();
    let mut frame = AxisFrame::new(event.time_msec()).source(source);
    for axis in [Axis::Horizontal, Axis::Vertical] {
        if let Some(amount) = event.amount(axis) {
            frame = frame.value(axis, amount);
            if let Some(v120) = event.amount_v120(axis) {
                frame = frame.v120(axis, v120 as i32);
            }
        } else if source == AxisSource::Finger {
            frame = frame.stop(axis);
        }
    }

    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    pointer.axis(state, frame);
    pointer.frame(state);
    tracing::info!(?source, "pointer axis");
}

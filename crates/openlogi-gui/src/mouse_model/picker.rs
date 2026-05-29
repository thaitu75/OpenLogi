//! Popover content for binding mouse buttons (and the gesture button's five
//! sub-directions) to an [`Action`].
//!
//! Three entry points, all generic over the entity that should be notified
//! after a binding changes so the trigger re-renders with the new label:
//!
//! - [`action_picker`] — one button → one action.
//! - [`gesture_picker`] — the gesture button's two-page flow (directions list
//!   → per-direction action picker), gated on [`AppState::gesture_edit`].
//!
//! The gpui-component [`Popover`](gpui_component::popover::Popover) already
//! wraps this content in a styled surface (background, border, shadow, and
//! `p_3` padding), so the layout here stays flat: no extra card background,
//! no extra outer padding. Rows are transparent until hovered; the active
//! binding is marked with accent text plus a check glyph rather than a filled
//! box, which is what made the old list read as a stack of buttons.

use std::rc::Rc;

use gpui::{
    AnyElement, App, BorrowAppContext as _, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, StatefulInteractiveElement as _, Styled, Window, div, hsla,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{h_flex, popover::PopoverState, v_flex};

use crate::data::mouse_buttons::{
    Action, ButtonId, Category, GestureDirection, default_gesture_binding,
};
use crate::state::AppState;
use crate::theme::{ACCENT_BLUE, BORDER, SURFACE_HOVER, TEXT_MUTED, TEXT_PRIMARY};

/// Minimum popover width. Wide enough for the gesture rows' two columns
/// (direction name on the left, bound action on the right).
const POPOVER_W: f32 = 224.;

/// Cap the scrollable action list height. The catalog has 29+ entries across
/// half a dozen categories; without a cap the popover overflows the window.
const POPOVER_LIST_MAX_H: f32 = 360.;

/// Build the popover body that re-binds a single `btn`.
///
/// `observer` is whatever entity wraps the trigger — it's notified after the
/// global updates so the trigger re-renders. Picking an action commits it and
/// dismisses the popover.
pub fn action_picker<T: 'static>(
    btn: ButtonId,
    observer: &Entity<T>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let current = cx
        .try_global::<AppState>()
        .and_then(|s| s.button_bindings.get(&btn).cloned());

    let observer = observer.clone();
    let popover = cx.entity().downgrade();
    let on_pick: PickFn = Rc::new(move |action, window, cx| {
        cx.update_global::<AppState, _>(|state, _| state.commit_binding(btn, action));
        observer.update(cx, |_, cx| cx.notify());
        if let Some(p) = popover.upgrade() {
            p.update(cx, |s, cx| s.dismiss(window, cx));
        }
    });

    v_flex()
        .min_w(px(POPOVER_W))
        .child(title(format!("Bind {}", btn.label())))
        .child(divider())
        .child(scroll_list(
            "picker-scroll",
            action_rows("action-item", current.as_ref(), &on_pick),
        ))
        .into_any_element()
}

/// Two-page gesture-button popover, dispatched on [`AppState::gesture_edit`]:
/// `None` shows the directions list, `Some(dir)` drills into that direction's
/// action picker.
pub fn gesture_picker<T: 'static>(
    observer: &Entity<T>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let edit = cx.try_global::<AppState>().and_then(|s| s.gesture_edit);
    match edit {
        Some(dir) => gesture_action_picker(dir, observer, cx),
        None => gesture_directions_list(observer, cx),
    }
}

/// Page 1: one row per direction, each showing the direction and its current
/// binding with a drill-in chevron. Clicking a row arms `gesture_edit`.
fn gesture_directions_list<T: 'static>(
    observer: &Entity<T>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let bindings = cx
        .try_global::<AppState>()
        .map(|s| s.gesture_bindings.clone())
        .unwrap_or_default();

    let rows: Vec<AnyElement> = GestureDirection::ALL
        .iter()
        .copied()
        .enumerate()
        .map(|(idx, dir)| {
            let action = bindings
                .get(&dir)
                .cloned()
                .unwrap_or_else(|| default_gesture_binding(dir));
            let observer = observer.clone();
            menu_row(("gesture-row", idx))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        // Fixed-width glyph so the direction names line up.
                        .child(
                            div()
                                .w(px(14.))
                                .text_color(rgb(TEXT_MUTED))
                                .child(dir.glyph()),
                        )
                        .child(div().text_color(rgb(TEXT_PRIMARY)).child(dir.label())),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(action.label())
                        .child("›"),
                )
                .on_click(move |_event, _window, cx| {
                    cx.update_global::<AppState, _>(|state, _| state.gesture_edit = Some(dir));
                    observer.update(cx, |_, cx| cx.notify());
                })
                .into_any_element()
        })
        .collect();

    v_flex()
        .min_w(px(POPOVER_W))
        .child(title("Gesture Button"))
        .child(divider())
        .children(rows)
        .into_any_element()
}

/// Page 2: the action catalog scoped to one `direction`. A back header returns
/// to the list without committing; picking an action commits and returns.
fn gesture_action_picker<T: 'static>(
    direction: GestureDirection,
    observer: &Entity<T>,
    cx: &mut Context<PopoverState>,
) -> AnyElement {
    let current = cx
        .try_global::<AppState>()
        .and_then(|s| s.gesture_bindings.get(&direction).cloned());

    let observer_pick = observer.clone();
    let on_pick: PickFn = Rc::new(move |action, _window, cx| {
        cx.update_global::<AppState, _>(|state, _| {
            state.commit_gesture_binding(direction, action);
            // Drop back to the directions list so the user can bind another
            // direction or dismiss the popover themselves.
            state.gesture_edit = None;
        });
        observer_pick.update(cx, |_, cx| cx.notify());
    });

    let observer_back = observer.clone();
    let back = h_flex()
        .id("gesture-back")
        .w_full()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(rgb(TEXT_MUTED))
        .hover(|s| s.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT_PRIMARY)))
        .child("‹")
        .child(format!("Gesture {}", direction.label()))
        .on_click(move |_event, _window, cx| {
            cx.update_global::<AppState, _>(|state, _| state.gesture_edit = None);
            observer_back.update(cx, |_, cx| cx.notify());
        });

    v_flex()
        .min_w(px(POPOVER_W))
        .child(back)
        .child(divider())
        .child(scroll_list(
            "gesture-picker-scroll",
            action_rows("gesture-action-item", current.as_ref(), &on_pick),
        ))
        .into_any_element()
}

// ── Shared building blocks ──────────────────────────────────────────────────

/// Commit callback invoked when a row is clicked. Boxed so the row builder can
/// be shared between the button picker and the gesture picker, which differ
/// only in what they do after committing.
type PickFn = Rc<dyn Fn(Action, &mut Window, &mut App)>;

/// The action catalog grouped by [`Category`], preserving catalog order within
/// each group and first-seen order across groups.
fn grouped_catalog() -> Vec<(Category, Vec<Action>)> {
    let mut sections: Vec<(Category, Vec<Action>)> = Vec::new();
    for action in Action::catalog() {
        let cat = action.category();
        if let Some(sec) = sections.iter_mut().find(|(c, _)| *c == cat) {
            sec.1.push(action);
        } else {
            sections.push((cat, vec![action]));
        }
    }
    sections
}

/// Build the category-grouped action rows. `current` is marked with accent
/// text + a check glyph; clicking any row invokes `on_pick`. `id_prefix`
/// disambiguates element IDs between the two pickers that share this builder.
fn action_rows(
    id_prefix: &'static str,
    current: Option<&Action>,
    on_pick: &PickFn,
) -> Vec<AnyElement> {
    let mut idx = 0usize;
    let mut children: Vec<AnyElement> = Vec::new();
    for (category, actions) in grouped_catalog() {
        children.push(section_header(category.label()));
        for action in actions {
            let selected = current == Some(&action);
            let label = action.label();
            let on_pick = on_pick.clone();
            let row_id = idx;
            idx += 1;
            children.push(
                menu_row((id_prefix, row_id))
                    .text_color(rgb(if selected { ACCENT_BLUE } else { TEXT_PRIMARY }))
                    .when(selected, |s| s.bg(hsla(0.6, 0.9, 0.6, 0.14)))
                    .child(div().child(label))
                    .when(selected, |s| {
                        s.child(div().text_color(rgb(ACCENT_BLUE)).child("✓"))
                    })
                    .on_click(move |_event, window, cx| (on_pick)(action.clone(), window, cx))
                    .into_any_element(),
            );
        }
    }
    children
}

/// A clickable, full-width menu row: transparent at rest, `SURFACE_HOVER` on
/// hover, `text-sm`, with its children spread left/right. Children are added
/// by the caller.
fn menu_row(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .py_1p5()
        .rounded_md()
        .text_sm()
        .hover(|s| s.bg(rgb(SURFACE_HOVER)))
}

/// Small uppercase muted group header.
fn section_header(label: &str) -> AnyElement {
    div()
        .w_full()
        .px_2()
        .pt_2()
        .pb_0p5()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT_MUTED))
        .child(label.to_uppercase())
        .into_any_element()
}

/// Popover title — the binding context, e.g. "Bind Back".
fn title(text: impl Into<gpui::SharedString>) -> impl IntoElement {
    div()
        .px_2()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(TEXT_MUTED))
        .child(text.into())
}

/// 1px hairline separating the title/back header from the list.
fn divider() -> impl IntoElement {
    div().mb_1().h(px(1.)).w_full().bg(rgb(BORDER))
}

/// Wrap `rows` in the height-capped, vertically scrollable list region.
fn scroll_list(id: &'static str, rows: Vec<AnyElement>) -> impl IntoElement {
    div()
        .id(id)
        .max_h(px(POPOVER_LIST_MAX_H))
        .overflow_y_scroll()
        .children(rows)
}

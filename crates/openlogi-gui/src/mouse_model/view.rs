//! Centre-of-screen mouse silhouette with clickable hotspots and side
//! labels connected by leader lines.
//!
//! Per UI.md phases 6 (silhouette + hotspots), 7 (labels + leader lines),
//! and 8 (breathing). When a [`ResolvedAsset`] is supplied by the asset
//! cache the synthetic silhouette is replaced by the real device PNG and
//! the hotspot/label positions come from the Logitech-format
//! `core_metadata.json`. Without an asset, we fall back to the original
//! shape-based silhouette plus [`default_hotspots`] / [`default_labels`].

use std::time::Duration;

use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, Context, ElementId, Entity, FontWeight,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, RenderOnce,
    StatefulInteractiveElement as _, Styled, Subscription, Window, canvas, div, ease_in_out, hsla,
    img, px, rgb,
};
use gpui_component::{Selectable, popover::Popover, v_flex};

use crate::asset::ResolvedAsset;
use crate::data::mouse_buttons::{Action, ButtonId, Hotspot, MOUSE_MODEL_SIZE, default_hotspots};
use crate::mouse_model::leader_lines::{
    Geometry as LeaderGeometry, Label, Side, paint as paint_leader_lines,
};
use crate::mouse_model::picker::{action_picker, gesture_picker};
use crate::state::AppState;
use crate::theme::{ACCENT_BLUE, BORDER, SURFACE_HOVER, TEXT_MUTED, TEXT_PRIMARY};

// Side-gutter geometry. Labels sit on the *left* of the mouse so the right
// half of the window is free for the DPI / gesture config column.
const SIDE_W: f32 = 180.;
const SIDE_GAP: f32 = 24.;
const LABEL_W: f32 = 156.;
// 56 px clears the two-line content (text-xs name + text-sm binding plus
// py-2 padding) without the bottom border slicing through the binding text.
// 44 was the exact content height and produced a "strikethrough" look on
// the binding row.
const LABEL_H: f32 = 56.;

/// Horizontal distance from the mouse silhouette's edge to the nearer
/// edge of a label card. Leader lines terminate at this offset so they
/// touch the card without crossing into the text.
const CARD_EDGE_INSET: f32 = SIDE_GAP + (SIDE_W - LABEL_W);

/// Approx pixel width of each hotspot hit-target. Logitech only gives us a
/// marker point per button, not a rectangle, so we size by hand.
const ASSET_HOTSPOT: f32 = 56.;

/// Diameter of the always-visible marker dot at each hotspot's centre. The
/// surrounding [`ASSET_HOTSPOT`] square stays as a transparent hit area so
/// clicks remain forgiving.
const HOTSPOT_DOT: f32 = 12.;

/// Vertical amplitude of the breathing loop. Two pixels reads as a soft
/// rise/fall without feeling unstable.
const BREATH_AMPLITUDE: f32 = 2.0;

pub struct MouseModelView {
    hovered: Option<ButtonId>,
    /// Repaints when the carousel switches devices. Held by value so the
    /// subscription stays alive for the entity's lifetime.
    _state_obs: Subscription,
}

impl MouseModelView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state_obs = cx.observe_global::<AppState>(|_view, cx| cx.notify());
        Self {
            hovered: None,
            _state_obs: state_obs,
        }
    }
}

impl Render for MouseModelView {
    #[allow(
        clippy::too_many_lines,
        reason = "the breathing + hotspots split + leader-canvas closure put the render fn over \
                  the pedantic limit; further extraction would just move noise around without \
                  making any single piece clearer"
    )]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pull everything that depends on the active device out of AppState
        // up front. Cloning is cheap (small structs, single small Vec) and it
        // releases the global borrow before the GPUI builders below.
        let (asset, active, bindings) = cx
            .try_global::<AppState>()
            .map(|s| {
                (
                    s.current_record().and_then(|r| r.asset.clone()),
                    s.active_button,
                    s.button_bindings.clone(),
                )
            })
            .unwrap_or_default();

        let (mouse_w, mouse_h) = MOUSE_MODEL_SIZE;
        let (mouse_w, mouse_h, hotspots, labels) = match asset.as_ref() {
            Some(a) => {
                let (w, h) = asset_dimensions_for_png(a, mouse_h);
                let hotspots = asset_hotspots_for_png(a, w, h);
                let labels = labels_from_hotspots(&hotspots);
                (w, h, hotspots, labels)
            }
            None => (mouse_w, mouse_h, default_hotspots(), default_labels()),
        };

        let canvas_w = SIDE_W + SIDE_GAP + mouse_w;
        let canvas_h = mouse_h;
        let mouse_left = SIDE_W + SIDE_GAP;

        let highlight = self.hovered.or(active);
        let view = cx.entity();
        let hovered = self.hovered;

        // The canvas closure takes ownership of one copy of hotspots/labels;
        // clone here so the outer label_card and hotspot_popover loops can
        // iterate over their own.
        let hotspots_outer = hotspots.clone();
        let labels_outer = labels.clone();
        let leader_canvas = canvas(
            move |_bounds, _, _| (hotspots, labels, highlight),
            move |bounds, payload, window, _app| {
                let (hotspots, labels, highlight) = payload;
                paint_leader_lines(
                    bounds,
                    LeaderGeometry {
                        mouse_origin: gpui::point(px(mouse_left), px(0.)),
                        mouse_w,
                        card_edge_inset: CARD_EDGE_INSET,
                    },
                    &hotspots,
                    &labels,
                    highlight,
                    window,
                );
            },
        )
        .size_full();

        // Breathing animation lives on a dedicated layer *behind* the
        // hotspot popovers. `.with_animation` rebuilds the wrapped
        // element each frame, which knocks gpui-component Popover's
        // keyed-state + deferred-anchored painting off the rails. Hotspots
        // stay in their own non-animated container; only the device PNG
        // (or synthetic silhouette) breathes.
        let device_art: AnyElement = match asset.as_ref() {
            Some(a) => img(a.image_path.clone())
                .w(px(mouse_w))
                .h(px(mouse_h))
                .into_any_element(),
            None => silhouette(mouse_w, mouse_h).into_any_element(),
        };
        let breathing_art = div()
            .absolute()
            .left(px(mouse_left))
            .top(px(0.))
            .w(px(mouse_w))
            .h(px(mouse_h))
            .child(device_art)
            .with_animation(
                "mouse-breath",
                Animation::new(Duration::from_secs(4))
                    .repeat()
                    .with_easing(ease_in_out),
                |this, delta| {
                    let dy = (delta * std::f32::consts::TAU).sin() * BREATH_AMPLITUDE;
                    this.top(px(dy))
                },
            );
        let hotspots_layer = div()
            .absolute()
            .left(px(mouse_left))
            .top(px(0.))
            .w(px(mouse_w))
            .h(px(mouse_h))
            .children(
                hotspots_outer
                    .iter()
                    .enumerate()
                    .map(|(idx, hotspot)| hotspot_popover(idx, *hotspot, hovered, active, &view)),
            );

        // z-order, bottom → top:
        //   1. device PNG (so leader lines don't disappear under the mouse)
        //   2. leader_canvas (lines over the PNG)
        //   3. label cards (so a line that grazes the card terminates
        //      cleanly behind the label instead of striking through it)
        //   4. hotspots (top, for hit-testing + popovers)
        div()
            .relative()
            .w(px(canvas_w))
            .h(px(canvas_h))
            .child(breathing_art)
            .child(leader_canvas)
            .children(labels_outer.iter().enumerate().map(|(idx, label)| {
                // GestureButton has 5 sub-bindings, not one — surface that
                // up front so the label isn't lying about what's bound.
                let binding = if label.id == ButtonId::GestureButton {
                    "5 directions".to_string()
                } else {
                    bindings
                        .get(&label.id)
                        .map_or_else(|| "Unbound".to_string(), Action::label)
                };
                label_popover(
                    idx,
                    *label,
                    binding,
                    highlight == Some(label.id),
                    mouse_left,
                    mouse_w,
                    hovered,
                    active,
                    &view,
                )
            }))
            .child(hotspots_layer)
    }
}

/// Scale the device image to fit a target height while preserving the
/// **actual PNG's** aspect ratio. The metadata's `origin` reports the
/// silhouette bbox inside the PNG, which is typically narrower than the
/// full image (Logi pads transparent strips on both sides); sizing by
/// origin causes `ObjectFit::Contain` to letterbox vertically and pulls
/// every hotspot off the rendered button.
#[allow(
    clippy::cast_precision_loss,
    reason = "device images are < 4096 px on either axis — well within f32 mantissa"
)]
fn asset_dimensions_for_png(asset: &ResolvedAsset, target_h: f32) -> (f32, f32) {
    if asset.png_height == 0 {
        return MOUSE_MODEL_SIZE;
    }
    let w = target_h * (asset.png_width as f32) / (asset.png_height as f32);
    (w, target_h)
}

/// Convert Logitech's percent-based markers into mouse-local pixel rects,
/// translating from the metadata's "origin" coord system (the silhouette
/// bbox) into the actual rendered PNG coord system.
///
/// Logi's markers are percentages of `origin` (the silhouette bbox).
/// Within the actual PNG, that bbox is centred with equal padding on the
/// left and right. We render at the *PNG's* full aspect (no letterboxing)
/// so the marker translation is:
///
/// ```text
/// bbox_w_rendered = mouse_w * origin.width  / png.width
/// bbox_x_offset   = (mouse_w - bbox_w_rendered) / 2
/// hotspot.x       = bbox_x_offset + marker.x / 100 * bbox_w_rendered
/// hotspot.y       = marker.y / 100 * mouse_h     // height ratio is 1:1
/// ```
///
/// Primary left/right clicks deliberately have no entry — Logi never
/// exposes them as remappable (and Options+ doesn't either), so we don't
/// invent markers for them.
#[allow(
    clippy::cast_precision_loss,
    reason = "device images are < 4096 px on either axis — well within f32 mantissa"
)]
fn asset_hotspots_for_png(asset: &ResolvedAsset, mouse_w: f32, mouse_h: f32) -> Vec<Hotspot> {
    let png_w = asset.png_width as f32;
    let origin_w = asset
        .metadata
        .origin()
        .map_or(png_w, |o| o.width as f32)
        .min(png_w);
    let bbox_w_rendered = if png_w > 0. {
        mouse_w * origin_w / png_w
    } else {
        mouse_w
    };
    let bbox_x_offset = (mouse_w - bbox_w_rendered) / 2.;
    let marker_to_canvas = |mx: f32, my: f32| -> (f32, f32) {
        let cx = bbox_x_offset + mx / 100. * bbox_w_rendered;
        let cy = my / 100. * mouse_h;
        (cx, cy)
    };

    asset
        .metadata
        .assignments()
        .filter_map(|a| {
            let id = map_slot_name(&a.slot_name)?;
            let (cx, cy) = marker_to_canvas(a.marker.x, a.marker.y);
            Some(Hotspot {
                id,
                x: cx - ASSET_HOTSPOT / 2.,
                y: cy - ASSET_HOTSPOT / 2.,
                w: ASSET_HOTSPOT,
                h: ASSET_HOTSPOT,
            })
        })
        .collect()
}

/// Logitech's stable slot vocabulary → OpenLogi's `ButtonId`. Intentionally
/// conservative; unknown names fall through so widening `ButtonId` later
/// doesn't break old depots.
fn map_slot_name(name: &str) -> Option<ButtonId> {
    match name {
        "SLOT_NAME_LEFT_BUTTON" => Some(ButtonId::LeftClick),
        "SLOT_NAME_RIGHT_BUTTON" => Some(ButtonId::RightClick),
        "SLOT_NAME_MIDDLE_BUTTON" => Some(ButtonId::MiddleClick),
        "SLOT_NAME_BACK_BUTTON" => Some(ButtonId::Back),
        "SLOT_NAME_FORWARD_BUTTON" => Some(ButtonId::Forward),
        "SLOT_NAME_MODESHIFT_BUTTON" => Some(ButtonId::DpiToggle),
        "SLOT_NAME_THUMBWHEEL" => Some(ButtonId::Thumbwheel),
        "SLOT_NAME_GESTURE_BUTTON" => Some(ButtonId::GestureButton),
        _ => None,
    }
}

/// Lay labels out on the left side, evenly spaced down the mouse's vertical
/// extent. Slots are assigned in order of the hotspots' y position (top
/// hotspot → top label) so leader lines don't cross — the previous
/// metadata-order layout produced a tangled mess on devices whose buttons
/// aren't listed top-to-bottom in the assignment array.
#[allow(
    clippy::cast_precision_loss,
    reason = "hotspot count is bounded by ButtonId variants — well under f32 mantissa"
)]
fn labels_from_hotspots(hotspots: &[Hotspot]) -> Vec<Label> {
    if hotspots.is_empty() {
        return Vec::new();
    }
    let mouse_h = MOUSE_MODEL_SIZE.1;
    let step = mouse_h / (hotspots.len() as f32 + 1.);

    // Sort indices by hotspot center y; the resulting rank is the slot
    // each label occupies on the left gutter.
    let mut ranks: Vec<usize> = (0..hotspots.len()).collect();
    ranks.sort_by(|&a, &b| {
        hotspots[a]
            .center()
            .1
            .partial_cmp(&hotspots[b].center().1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut slot_of: Vec<usize> = vec![0; hotspots.len()];
    for (rank, idx) in ranks.into_iter().enumerate() {
        slot_of[idx] = rank;
    }

    hotspots
        .iter()
        .enumerate()
        .map(|(i, h)| Label {
            id: h.id,
            side: Side::Left,
            y: step * (slot_of[i] as f32 + 1.),
        })
        .collect()
}

fn default_labels() -> Vec<Label> {
    vec![
        Label {
            id: ButtonId::MiddleClick,
            side: Side::Left,
            y: 120.,
        },
        Label {
            id: ButtonId::Back,
            side: Side::Left,
            y: 240.,
        },
        Label {
            id: ButtonId::Forward,
            side: Side::Left,
            y: 340.,
        },
        Label {
            id: ButtonId::DpiToggle,
            side: Side::Left,
            y: 440.,
        },
    ]
}

/// Position the popover wrapper at the label's slot in the side gutter and
/// host a Popover whose trigger is the label card itself. Same picker
/// content as the hotspot dot — clicking either entry point lands on the
/// same binding flow.
#[allow(
    clippy::too_many_arguments,
    reason = "wrapper position + trigger \
state both need this many inputs; bundling would just hide the dependency"
)]
fn label_popover(
    idx: usize,
    label: Label,
    binding: String,
    highlighted: bool,
    mouse_left: f32,
    mouse_w: f32,
    hovered: Option<ButtonId>,
    active: Option<ButtonId>,
    view: &Entity<MouseModelView>,
) -> AnyElement {
    let x = match label.side {
        Side::Left => mouse_left - SIDE_GAP - SIDE_W,
        Side::Right => mouse_left + mouse_w + SIDE_GAP,
    };
    let view = view.clone();
    let trigger = LabelTrigger {
        id: ("label-trigger", idx).into(),
        label,
        binding,
        highlighted: highlighted || hovered == Some(label.id) || active == Some(label.id),
        selected: false,
        view: view.clone(),
    };
    div()
        .absolute()
        .left(px(x))
        .top(px(label.y - LABEL_H / 2.))
        .w(px(LABEL_W))
        .h(px(LABEL_H))
        .child(
            Popover::new(("label-popover", idx))
                .anchor(Anchor::TopLeft)
                .mouse_button(MouseButton::Left)
                .trigger(trigger)
                .content(move |_state, _window, cx| {
                    if label.id == ButtonId::GestureButton {
                        gesture_picker(&view, cx)
                    } else {
                        action_picker(label.id, &view, cx)
                    }
                }),
        )
        .into_any_element()
}

#[derive(IntoElement)]
struct LabelTrigger {
    id: ElementId,
    label: Label,
    binding: String,
    highlighted: bool,
    selected: bool,
    view: Entity<MouseModelView>,
}

impl Selectable for LabelTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for LabelTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let highlighted = self.highlighted || self.selected;
        let btn = self.label.id;
        let view = self.view;
        div()
            .id(self.id)
            .w(px(LABEL_W))
            .h(px(LABEL_H))
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(if highlighted { ACCENT_BLUE } else { BORDER }))
            .bg(rgb(SURFACE_HOVER))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(self.label.id.label()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if highlighted {
                                ACCENT_BLUE
                            } else {
                                TEXT_PRIMARY
                            }))
                            .child(self.binding),
                    ),
            )
            .on_hover(move |hovered, _window, cx| {
                let is_hovered = *hovered;
                view.update(cx, |this, cx| {
                    if is_hovered {
                        this.hovered = Some(btn);
                    } else if this.hovered == Some(btn) {
                        this.hovered = None;
                    }
                    cx.notify();
                });
            })
    }
}

/// Shape-based silhouette used when no asset is cached for the device.
fn silhouette(w: f32, h: f32) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .w(px(w))
        .h(px(h))
        .rounded_3xl()
        .border_1()
        .border_color(rgb(TEXT_MUTED))
        .bg(rgb(SURFACE_HOVER))
        .child(
            div()
                .absolute()
                .left(px(w / 2. - 14.))
                .top(px(90.))
                .w(px(28.))
                .h(px(110.))
                .rounded_md()
                .bg(hsla(0., 0., 0.25, 1.0)),
        )
        .child(
            div()
                .absolute()
                .left(px(w / 2.))
                .top(px(20.))
                .w(px(1.))
                .h(px(240.))
                .bg(rgb(BORDER)),
        )
        .child(
            div()
                .absolute()
                .left(px(8.))
                .top(px(210.))
                .w(px(34.))
                .h(px(150.))
                .rounded_md()
                .bg(hsla(0., 0., 0.25, 1.0)),
        )
}

fn hotspot_popover(
    idx: usize,
    hotspot: Hotspot,
    hovered: Option<ButtonId>,
    active: Option<ButtonId>,
    view: &Entity<MouseModelView>,
) -> AnyElement {
    // Position the Popover wrapper, not the trigger. gpui-component's
    // Popover renders its trigger inside a parent div that carries the
    // `on_mouse_down` handler; if the trigger is `.absolute()`, the
    // wrapper div collapses to 0×0 and clicks never hit the handler.
    // The trigger sizes itself in explicit px (see HotspotTrigger),
    // and the wrapper here carries the absolute positioning.
    let view = view.clone();
    let trigger = HotspotTrigger {
        id: ("hotspot-trigger", idx).into(),
        hotspot,
        hovered: hovered == Some(hotspot.id) || active == Some(hotspot.id),
        view: view.clone(),
        selected: false,
    };
    div()
        .absolute()
        .left(px(hotspot.x))
        .top(px(hotspot.y))
        .w(px(hotspot.w))
        .h(px(hotspot.h))
        .child(
            Popover::new(("hotspot-popover", idx))
                .anchor(Anchor::TopRight)
                .mouse_button(MouseButton::Left)
                .trigger(trigger)
                .content(move |_state, _window, cx| {
                    if hotspot.id == ButtonId::GestureButton {
                        gesture_picker(&view, cx)
                    } else {
                        action_picker(hotspot.id, &view, cx)
                    }
                }),
        )
        .into_any_element()
}

#[derive(IntoElement)]
struct HotspotTrigger {
    id: ElementId,
    hotspot: Hotspot,
    hovered: bool,
    view: Entity<MouseModelView>,
    selected: bool,
}

impl Selectable for HotspotTrigger {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for HotspotTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let highlighted = self.hovered || self.selected;
        let view = self.view;
        let hotspot = self.hotspot;
        let btn = hotspot.id;

        // Explicit pixel dimensions, not `.size_full()`. gpui-component's
        // Popover wraps the trigger in a `div().child(trigger)` with no
        // explicit size — that div sizes to its child. If the child is
        // `.size_full()`, the resolved size is 0×0 (no parent reference
        // for the percentage) and the popover's `on_mouse_down` never
        // receives clicks. Painting explicit pixels gives the popover's
        // wrapper a real hit-test region.
        //
        // The outer 56×56 stays transparent so the cursor finds the
        // popover from a generous radius; the visible marker is a small
        // always-on dot centred inside.
        div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(hotspot.w))
            .h(px(hotspot.h))
            // Marker dot: dark core + thin light ring. The two-tone helps
            // it read on both the light grey mouse top and any darker
            // shadow areas; a plain mid-grey washed out on the light PNG.
            .child(
                div()
                    .w(px(HOTSPOT_DOT))
                    .h(px(HOTSPOT_DOT))
                    .rounded_full()
                    .border_1()
                    .border_color(if highlighted {
                        gpui::Hsla::from(rgb(ACCENT_BLUE))
                    } else {
                        hsla(0., 0., 0.95, 0.85)
                    })
                    .bg(if highlighted {
                        gpui::Hsla::from(rgb(ACCENT_BLUE))
                    } else {
                        hsla(0., 0., 0.18, 0.85)
                    }),
            )
            .on_hover(move |hovered, _window, cx| {
                let is_hovered = *hovered;
                view.update(cx, |this, cx| {
                    if is_hovered {
                        this.hovered = Some(btn);
                    } else if this.hovered == Some(btn) {
                        this.hovered = None;
                    }
                    cx.notify();
                });
            })
    }
}

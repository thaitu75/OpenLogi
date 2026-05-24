//! Canvas-painted leader lines from each hotspot to its side-label anchor.
//!
//! Per UI.md Phase 7. Each polyline is hotspot-centre → short horizontal
//! stub → diagonal to the label anchor. The active hotspot's line is
//! coloured blue and stroked thicker; everything else stays muted.

use gpui::{Bounds, PathBuilder, Pixels, Point, Window, hsla, point, px, rgb};

use crate::data::mouse_buttons::{ButtonId, Hotspot};
use crate::theme::ACCENT_BLUE;

/// Length of the horizontal stub before turning toward the label.
const STUB: f32 = 28.;
/// Horizontal distance from the stub to the label anchor.
const LEAD_RUN: f32 = 140.;

/// Which side of the mouse a label sits on. `Right` is unused in the current
/// view (the right half of the window is reserved for the DPI / gesture
/// column) but the routing logic is kept so labels can move later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Right variant kept for future right-side labelling"
)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct Label {
    pub id: ButtonId,
    pub side: Side,
    /// Y of the label anchor, in mouse-canvas coords (i.e. relative to the
    /// canvas's top-left, not the mouse silhouette's top-left).
    pub y: f32,
}

/// Paint every leader line. `mouse_origin` is the top-left of the mouse
/// silhouette in *canvas-local* coords; hotspot coords are mouse-local;
/// label `y` is canvas-local. Everything is converted to window-absolute
/// before being handed to `PathBuilder` — `paint_path` expects absolute
/// coordinates and there is no implicit canvas-to-window transform.
pub fn paint(
    canvas_bounds: Bounds<Pixels>,
    mouse_origin: Point<Pixels>,
    mouse_w: f32,
    hotspots: &[Hotspot],
    labels: &[Label],
    highlighted: Option<ButtonId>,
    window: &mut Window,
) {
    for label in labels {
        let Some(hotspot) = hotspots.iter().find(|h| h.id == label.id) else {
            continue;
        };
        paint_one(
            canvas_bounds.origin,
            mouse_origin,
            mouse_w,
            *hotspot,
            *label,
            highlighted == Some(label.id),
            window,
        );
    }
}

fn paint_one(
    canvas_screen_origin: Point<Pixels>,
    mouse_origin_local: Point<Pixels>,
    mouse_w: f32,
    hotspot: Hotspot,
    label: Label,
    highlight: bool,
    window: &mut Window,
) {
    // Mouse silhouette's top-left in window-absolute coords. Every other
    // coordinate is derived from this so we don't accidentally mix
    // coordinate systems again.
    let mouse_screen = canvas_screen_origin + mouse_origin_local;

    let (hx, hy) = hotspot.center();
    let hotspot_centre = mouse_screen + point(px(hx), px(hy));

    let (stub_x, anchor_x) = match label.side {
        Side::Left => (
            mouse_screen.x - px(STUB),
            mouse_screen.x - px(STUB) - px(LEAD_RUN),
        ),
        Side::Right => (
            mouse_screen.x + px(mouse_w) + px(STUB),
            mouse_screen.x + px(mouse_w) + px(STUB) + px(LEAD_RUN),
        ),
    };
    let stub = Point {
        x: stub_x,
        y: hotspot_centre.y,
    };
    let anchor = Point {
        x: anchor_x,
        y: mouse_screen.y + px(label.y),
    };

    let width = if highlight { px(2.) } else { px(1.) };
    let mut path = PathBuilder::stroke(width);
    path.move_to(hotspot_centre);
    path.line_to(stub);
    path.line_to(anchor);

    if let Ok(built) = path.build() {
        if highlight {
            window.paint_path(built, rgb(ACCENT_BLUE));
        } else {
            // Muted gray — readable against the dark background without
            // competing with the highlighted line.
            window.paint_path(built, hsla(0., 0., 0.55, 0.35));
        }
    }
}

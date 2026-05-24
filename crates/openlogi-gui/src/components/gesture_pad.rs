//! Gesture recording pad.
//!
//! 300×300 region: press and drag with the left button to record a path,
//! release to classify into one of eight cardinal/diagonal directions. The
//! live path is drawn via `canvas` + `PathBuilder::stroke`.
//!
//! Per UI.md Phase 5.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, PathBuilder, Pixels,
    Point, Render, Styled, Window, canvas, div, px, rgb,
};
use gpui_component::v_flex;

use crate::theme::{ACCENT_BLUE, BORDER, SURFACE, TEXT_MUTED};

const PAD_SIZE: f32 = 300.;
/// Minimum start-to-end distance (px) below which we don't bother classifying
/// — anything shorter is treated as a tap, not a gesture.
const MIN_TRAVEL: f32 = 24.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    UpRight,
    Right,
    DownRight,
    Down,
    DownLeft,
    Left,
    UpLeft,
}

impl Direction {
    fn arrow(self) -> &'static str {
        match self {
            Direction::Up => "↑ Up",
            Direction::UpRight => "↗ Up-Right",
            Direction::Right => "→ Right",
            Direction::DownRight => "↘ Down-Right",
            Direction::Down => "↓ Down",
            Direction::DownLeft => "↙ Down-Left",
            Direction::Left => "← Left",
            Direction::UpLeft => "↖ Up-Left",
        }
    }
}

pub struct GesturePad {
    /// Points captured in window-space pixels — converted to pad-local during
    /// paint by subtracting `bounds.origin`.
    points: Vec<Point<Pixels>>,
    /// Result of the last completed gesture, displayed under the pad.
    last: Option<Direction>,
    /// True between mouse_down and mouse_up; gates mouse_move accumulation.
    drawing: bool,
}

impl GesturePad {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            points: Vec::new(),
            last: None,
            drawing: false,
        }
    }
}

impl Render for GesturePad {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let points = self.points.clone();
        let down_entity = entity.clone();
        let move_entity = entity.clone();
        let up_entity = entity.clone();

        v_flex()
            .gap_2()
            .child(
                div()
                    .id("gesture-pad")
                    .relative()
                    .w(px(PAD_SIZE))
                    .h(px(PAD_SIZE))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                        let pos = event.position;
                        down_entity.update(cx, |this, cx| {
                            this.points.clear();
                            this.points.push(pos);
                            this.last = None;
                            this.drawing = true;
                            cx.notify();
                        });
                    })
                    .on_mouse_move(move |event, _window, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        let pos = event.position;
                        move_entity.update(cx, |this, cx| {
                            if !this.drawing {
                                return;
                            }
                            this.points.push(pos);
                            cx.notify();
                        });
                    })
                    .on_mouse_up(MouseButton::Left, move |_event, _window, cx| {
                        up_entity.update(cx, |this, cx| {
                            this.drawing = false;
                            this.last = classify(&this.points);
                            cx.notify();
                        });
                    })
                    .child(
                        canvas(
                            move |_bounds, _, _| points,
                            |bounds, pts, window, app| paint_path(bounds, &pts, window, app),
                        )
                        .size_full(),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT_MUTED))
                    .child(match self.last {
                        Some(d) => d.arrow().to_string(),
                        None => {
                            if self.drawing {
                                "Recording…".to_string()
                            } else {
                                "Drag with the left button to record a gesture.".to_string()
                            }
                        }
                    }),
            )
    }
}

/// Canvas paint callback. `points` are already in window-absolute coords
/// (that's what `MouseDownEvent::position` gives us) and `paint_path` also
/// expects window-absolute coords, so no transform is needed. The `bounds`
/// argument is used only to clip via the content mask `paint_path` applies.
fn paint_path(
    _bounds: gpui::Bounds<Pixels>,
    points: &[Point<Pixels>],
    window: &mut Window,
    _app: &mut gpui::App,
) {
    if points.len() < 2 {
        return;
    }
    let mut path = PathBuilder::stroke(px(3.));
    path.move_to(points[0]);
    for p in &points[1..] {
        path.line_to(*p);
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, rgb(ACCENT_BLUE));
    }
}

/// Classify a captured stroke into one of eight compass directions. Returns
/// `None` when the stroke is too short to be meaningful.
fn classify(points: &[Point<Pixels>]) -> Option<Direction> {
    if points.len() < 2 {
        return None;
    }
    let start = points[0];
    let end = points[points.len() - 1];
    let dx = f32::from(end.x - start.x);
    let dy = f32::from(end.y - start.y);
    if (dx * dx + dy * dy).sqrt() < MIN_TRAVEL {
        return None;
    }
    // atan2 returns radians in (-π, π]. Map to eight 45° sectors centred on
    // each compass direction. Note: GPUI's y axis grows downward, so
    // positive dy means "down".
    let angle = dy.atan2(dx);
    let sector = angle_to_sector(angle);
    Some(SECTOR_TO_DIRECTION[sector])
}

/// 0 = East, then clockwise: NE? Actually with y-down, clockwise from East
/// is: E (0), SE (1), S (2), SW (3), W (4), NW (5), N (6), NE (7).
const SECTOR_TO_DIRECTION: [Direction; 8] = [
    Direction::Right,
    Direction::DownRight,
    Direction::Down,
    Direction::DownLeft,
    Direction::Left,
    Direction::UpLeft,
    Direction::Up,
    Direction::UpRight,
];

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "sector is taken modulo 8 immediately after the cast"
)]
fn angle_to_sector(angle: f32) -> usize {
    // Shift so the rounded sector lands on the centre of each 45° wedge.
    let normalized = angle / std::f32::consts::FRAC_PI_4;
    (normalized.round() as i32).rem_euclid(8) as usize
}

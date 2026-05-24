//! Root view: header (device carousel), body (mouse model + side config),
//! footer (settings / version).
//!
//! The body is now arranged per UI.md §1: the [`MouseModelView`] sits on
//! the left, and the right column stacks the DPI panel + gesture pad
//! (placeholders for the eventual multi-tab config panel).

use gpui::{
    AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement, Render, Styled,
    Window, div, px, rgb,
};
use gpui_component::{ActiveTheme, h_flex, v_flex};
use openlogi_core::device::DeviceInventory;

use crate::components::device_carousel::DeviceCarousel;
use crate::components::dpi_panel::DpiPanel;
use crate::components::gesture_pad::GesturePad;
use crate::mouse_model::view::MouseModelView;
use crate::state::AppState;
use crate::theme::{BG_DARK, BORDER, FOOTER_H, HEADER_H, TEXT_MUTED, TEXT_PRIMARY};

pub struct AppView {
    carousel: Entity<DeviceCarousel>,
    mouse_model: Entity<MouseModelView>,
    dpi_panel: Entity<DpiPanel>,
    gesture_pad: Entity<GesturePad>,
}

impl AppView {
    pub fn new(inventories: &[DeviceInventory], cx: &mut Context<Self>) -> Self {
        if !cx.has_global::<AppState>() {
            cx.set_global(AppState::new());
        }
        let carousel = cx.new(|cx| DeviceCarousel::new(inventories, cx));
        let mouse_model = cx.new(MouseModelView::new);
        let dpi_panel = cx.new(DpiPanel::new);
        let gesture_pad = cx.new(GesturePad::new);
        Self {
            carousel,
            mouse_model,
            dpi_panel,
            gesture_pad,
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(rgb(BG_DARK))
            .text_color(rgb(TEXT_PRIMARY))
            .child(header(&self.carousel))
            .child(body(&self.mouse_model, &self.dpi_panel, &self.gesture_pad))
            .child(footer(cx))
    }
}

fn header(carousel: &Entity<DeviceCarousel>) -> impl IntoElement {
    h_flex()
        .h(px(HEADER_H))
        .w_full()
        .px_5()
        .gap_4()
        .items_center()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child("OpenLogi"),
        )
        .child(div().flex_1().min_w_0().child(carousel.clone()))
}

fn body(
    mouse_model: &Entity<MouseModelView>,
    dpi_panel: &Entity<DpiPanel>,
    gesture_pad: &Entity<GesturePad>,
) -> impl IntoElement {
    h_flex()
        .flex_1()
        .w_full()
        .min_h_0()
        .items_start()
        .justify_center()
        .gap_6()
        .p_6()
        .child(mouse_model.clone())
        .child(
            v_flex()
                .gap_6()
                .child(dpi_panel.clone())
                .child(panel_label("Gestures"))
                .child(gesture_pad.clone()),
        )
}

fn panel_label(text: &'static str) -> impl IntoElement {
    div().text_sm().text_color(rgb(TEXT_MUTED)).child(text)
}

fn footer(cx: &Context<AppView>) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .h(px(FOOTER_H))
        .w_full()
        .px_5()
        .gap_4()
        .items_center()
        .justify_between()
        .border_t_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Settings · About"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(concat!("v", env!("CARGO_PKG_VERSION"))),
        )
}

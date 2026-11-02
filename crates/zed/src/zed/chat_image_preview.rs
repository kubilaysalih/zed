use gpui::{
    Bounds, Context, DismissEvent, EventEmitter, ImageSource, MouseButton, Pixels, Point, Render,
    ScrollDelta, Window,
};
use std::{cell::Cell, rc::Rc};
use ui::{Tooltip, prelude::*};

pub struct ChatImagePreview {
    image: ImageSource,
    zoom: f32,
    pan: Point<Pixels>,
    dragging: Option<Point<Pixels>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl ChatImagePreview {
    pub fn new(image: ImageSource) -> Self {
        Self {
            image,
            zoom: 1.,
            pan: Point::default(),
            dragging: None,
            bounds: Default::default(),
        }
    }

    fn zoom_by(&mut self, factor: f32, anchor: Point<Pixels>, cx: &mut Context<Self>) {
        let zoom = self.zoom * factor;
        let pan = anchor + (self.pan - anchor) * factor;
        let bounds = self.bounds.get();
        // Reject numeric overflow, without imposing an arbitrary magnification limit.
        if zoom <= 0.
            || !zoom.is_finite()
            || !f32::from(bounds.size.width * zoom).is_finite()
            || !f32::from(bounds.size.height * zoom).is_finite()
            || !f32::from(pan.x).is_finite()
            || !f32::from(pan.y).is_finite()
        {
            return;
        }
        self.zoom = zoom;
        self.pan = pan;
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for ChatImagePreview {}

impl Render for ChatImagePreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.image.clone();
        let bounds_cell = self.bounds.clone();
        let zoom = self.zoom;
        let pan = self.pan;
        v_flex()
            .id("chat-image-preview")
            .absolute()
            .inset_0()
            .size_full()
            .p_4()
            .gap_2()
            .bg(gpui::rgba(0x101114f5))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        IconButton::new("image-zoom-out", IconName::Dash)
                            .tooltip(Tooltip::text("Zoom out"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.zoom_by(1. / 1.25, Point::default(), cx)
                            })),
                    )
                    .child(Label::new(format!("{:.0}%", self.zoom * 100.)).size(LabelSize::Small))
                    .child(
                        IconButton::new("image-zoom-in", IconName::Plus)
                            .tooltip(Tooltip::text("Zoom in · mouse wheel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.zoom_by(1.25, Point::default(), cx)
                            })),
                    )
                    .child(Button::new("image-fit", "Fit").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.zoom = 1.;
                            this.pan = Point::default();
                            this.dragging = None;
                            cx.notify();
                        },
                    )))
                    .child(div().flex_1())
                    .child(
                        IconButton::new("close-image", IconName::Close)
                            .tooltip(Tooltip::text("Close preview · Escape"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    ),
            )
            .child(
                div()
                    .id("chat-image-viewport")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .cursor_move()
                    .on_scroll(cx.listener(|this, event: &gpui::ScrollEvent, _, cx| {
                        let delta = match event.delta {
                            ScrollDelta::Pixels(delta) => f32::from(delta.y),
                            ScrollDelta::Lines(delta) => delta.y * 40.,
                        };
                        let factor = if delta > 0. {
                            1. + delta * 0.01
                        } else {
                            1. / (1. - delta * 0.01)
                        };
                        let bounds = this.bounds.get();
                        let center = bounds.origin
                            + gpui::point(bounds.size.width / 2., bounds.size.height / 2.);
                        this.zoom_by(factor, event.position - center, cx);
                        cx.stop_propagation();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            this.dragging = Some(event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            this.dragging = None;
                            return;
                        }
                        if let Some(previous) = this.dragging {
                            this.pan += event.position - previous;
                            this.dragging = Some(event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dragging = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dragging = None;
                            cx.notify();
                        }),
                    )
                    .child(
                        gpui::canvas(
                            move |bounds, window, cx| {
                                bounds_cell.set(bounds);
                                let size =
                                    gpui::size(bounds.size.width * zoom, bounds.size.height * zoom);
                                let origin = bounds.origin
                                    + gpui::point(
                                        (bounds.size.width - size.width) / 2.,
                                        (bounds.size.height - size.height) / 2.,
                                    )
                                    + pan;
                                let mut image = gpui::img(image)
                                    .w(size.width)
                                    .h(size.height)
                                    .object_fit(gpui::ObjectFit::Contain)
                                    .into_any_element();
                                image.prepaint_as_root(origin, size.into(), window, cx);
                                image
                            },
                            |_, mut image, window, cx| image.paint(window, cx),
                        )
                        .size_full(),
                    ),
            )
    }
}

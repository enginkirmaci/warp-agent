//! Tiny animated leaf views. The pulse/spinner decorations used to animate
//! inline inside TileView/WarpWindow renders, and gpui's animation frame
//! notifies the view that mounted the element — so a 5px dot kept repainting
//! the whole tile (or the whole window) at frame rate for as long as a turn
//! ran. As standalone view entities the per-frame notify re-renders only the
//! leaf itself; an unmounted entity (turn over, session idle) costs nothing.

use std::time::Duration;

use gpui::{div, px, Animation, AnimationExt, Context, Hsla, IntoElement, Pixels, Render, Styled, Window};

/// Decorative opacity pulse: a 5px dot, or a spinner svg breathing the same
/// way. `low` is the dim end of the fade (busy dot 0.2, the softer pulses 0.25).
pub struct Pulse {
    kind: PulseKind,
    period: Duration,
    low: f32,
}

enum PulseKind {
    Dot(Hsla),
    Icon(&'static str, Pixels, Hsla),
}

impl Pulse {
    pub fn dot(color: impl Into<Hsla>, period: Duration, low: f32) -> Self {
        Self { kind: PulseKind::Dot(color.into()), period, low }
    }

    pub fn icon(path: &'static str, size: Pixels, color: impl Into<Hsla>, period: Duration, low: f32) -> Self {
        Self { kind: PulseKind::Icon(path, size, color.into()), period, low }
    }
}

impl Render for Pulse {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // 15 fps is plenty for a slow opacity breathe and keeps the notify
        // timer-paced instead of riding vsync
        let anim = Animation::new(self.period).repeat().with_max_fps(15.);
        let low = self.low;
        match &self.kind {
            PulseKind::Dot(color) => div()
                .size(px(5.))
                .rounded_full()
                .bg(*color)
                .with_animation("dot", anim, move |el, delta| {
                    el.opacity(low + (1.0 - low) * (1.0f32 - (delta - 0.5).abs() * 2.0).max(0.0))
                })
                .into_any_element(),
            PulseKind::Icon(path, size, color) => crate::ui::icon(path, *size, *color)
                .with_animation("icon", anim, move |el, delta| {
                    el.opacity(low + (1.0 - low) * (1.0f32 - (delta - 0.5).abs() * 2.0).max(0.0))
                })
                .into_any_element(),
        }
    }
}

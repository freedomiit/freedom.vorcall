//! The toast stack, bottom right: at most three, five seconds each.
//!
//! It is the one overlay that never blocks: the layer is a plain container, so a
//! press anywhere but on a toast's own × reaches whatever is under it.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};

use crate::app::App;
use crate::app::message::{Message, ToastKind, UiMsg};
use crate::icons::{self, Icon};
use crate::theme::styles;
use crate::view::TEXT_ROW;

const TOAST_WIDTH: f32 = 340.0;
const TOAST_ICON: f32 = 16.0;

pub fn view(app: &App) -> Element<'_, Message> {
    let tokens = &app.tokens;

    let mut stack = column![].spacing(8);
    for toast in &app.ui.toasts {
        let (glyph, color) = match toast.kind {
            ToastKind::Info => (Icon::Info, tokens.success),
            ToastKind::Error => (Icon::Warning, tokens.danger),
        };
        stack = stack.push(
            container(
                row![
                    icons::icon(glyph, TOAST_ICON, color),
                    text(toast.text.clone())
                        .size(TEXT_ROW)
                        .color(tokens.text_primary)
                        .width(Length::Fill),
                    button(icons::icon(Icon::Close, 14.0, tokens.text_muted))
                        .padding(2)
                        .style(styles::button::icon(tokens))
                        .on_press(Message::Ui(UiMsg::DismissToast(toast.id))),
                ]
                .spacing(10)
                .align_y(Vertical::Center),
            )
            .width(TOAST_WIDTH)
            .padding([10.0, 12.0])
            .style(styles::container::toast(tokens, toast.kind)),
        );
    }

    container(stack)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Right)
        .align_y(Vertical::Bottom)
        .padding(16)
        .into()
}

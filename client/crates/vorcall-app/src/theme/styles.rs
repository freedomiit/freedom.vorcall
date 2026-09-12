//! Every widget style the views use, as closures over a [`ThemeTokens`].
//!
//! The tokens are threaded explicitly rather than read back out of iced's
//! `Theme`: iced only carries the six palette colours, and a Vorcall widget
//! needs all 29. `ThemeTokens` is `Copy`, so each closure owns a copy of the
//! theme and can live as long as the element it styles.

// The widget modules are aliased: a style function below carries the same name as
// the widget it styles, and the two cannot share a namespace.
use iced::overlay::menu as menu_widget;
use iced::widget::pick_list as pick_list_widget;
use iced::widget::rule as rule_widget;
use iced::widget::scrollable as scrollable_widget;
use iced::widget::slider as slider_widget;
use iced::widget::svg as svg_widget;
use iced::widget::text_editor as text_editor_widget;
use iced::widget::text_input as text_input_widget;
use iced::widget::toggler as toggler_widget;
use iced::{Background, Border, Color, Shadow, Theme, Vector, border};

use super::tokens::ThemeTokens;
use crate::app::message::ToastKind;

/// Radii, from the design scale: chip, control, card, popover.
pub const RADIUS_CHIP: f32 = 4.0;
pub const RADIUS_CONTROL: f32 = 6.0;
pub const RADIUS_CARD: f32 = 8.0;
pub const RADIUS_POPOVER: f32 = 12.0;

/// How much of a colour is left when the control it paints is disabled.
const DISABLED_ALPHA: f32 = 0.4;

/// The hairline border an input or a card draws.
fn hairline(tokens: &ThemeTokens, radius: f32) -> Border {
    border::rounded(radius)
        .width(1.0)
        .color(tokens.border_subtle)
}

/// What is left of a colour on a control that cannot be used. A widget that
/// paints its own glyph needs it: the disabled text colour of a button never
/// reaches an SVG.
pub fn faded(color: Color) -> Color {
    Color {
        a: color.a * DISABLED_ALPHA,
        ..color
    }
}

/// The shadow a surface that floats over another one casts.
fn lift(offset: f32, blur: f32) -> Shadow {
    Shadow {
        color: Color {
            a: 0.35,
            ..Color::BLACK
        },
        offset: Vector::new(0.0, offset),
        blur_radius: blur,
    }
}

pub mod button {
    //! Every button shape. `row` and `row_selected` are the sidebar's rows, which
    //! are buttons so hover and focus come for free.

    use iced::widget::button::{Status, Style};
    use iced::{Background, Color, Theme, border};

    use super::{DISABLED_ALPHA, RADIUS_CONTROL, ThemeTokens, faded};

    /// How hard the rail's squares are rounded — more than a card, less than a
    /// circle.
    const RADIUS_RAIL: f32 = 14.0;

    /// The filled accent button: one per dialog, and the composer's send.
    pub fn primary(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match status {
                Status::Active => tokens.accent,
                Status::Hovered => tokens.accent_hover,
                Status::Pressed => tokens.accent_pressed,
                Status::Disabled => faded(tokens.accent),
            })),
            text_color: match status {
                Status::Disabled => faded(tokens.text_on_accent),
                _ => tokens.text_on_accent,
            },
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// The quiet filled button: cancel, and every secondary action.
    pub fn secondary(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match status {
                Status::Active | Status::Disabled => tokens.control_bg,
                Status::Hovered => tokens.control_bg_hover,
                Status::Pressed => tokens.bg_active,
            })),
            text_color: match status {
                Status::Disabled => tokens.text_muted,
                _ => tokens.text_primary,
            },
            border: border::rounded(RADIUS_CONTROL)
                .width(1.0)
                .color(tokens.control_border),
            ..Style::default()
        }
    }

    /// Text on nothing, until the pointer is on it.
    pub fn ghost(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: match status {
                Status::Hovered => Some(Background::Color(tokens.bg_hover)),
                Status::Pressed => Some(Background::Color(tokens.bg_active)),
                Status::Active | Status::Disabled => None,
            },
            text_color: match status {
                Status::Active => tokens.text_secondary,
                Status::Hovered | Status::Pressed => tokens.text_primary,
                Status::Disabled => tokens.text_muted,
            },
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// Anything that destroys something: delete, kick, ban, stop watching.
    pub fn danger(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match status {
                Status::Active => tokens.danger,
                Status::Hovered | Status::Pressed => Color {
                    a: tokens.danger.a * 0.85,
                    ..tokens.danger
                },
                Status::Disabled => faded(tokens.danger),
            })),
            text_color: tokens.text_on_accent,
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// The far-left rail's entries: a rounded square that fills with the accent
    /// tint when it is the view in front.
    pub fn rail(tokens: &ThemeTokens, selected: bool) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: match (selected, status) {
                (true, _) => Some(Background::Color(tokens.accent_tint)),
                (false, Status::Hovered) => Some(Background::Color(tokens.bg_hover)),
                (false, Status::Pressed) => Some(Background::Color(tokens.bg_active)),
                (false, _) => None,
            },
            text_color: if selected {
                tokens.text_primary
            } else {
                tokens.text_secondary
            },
            border: border::rounded(RADIUS_RAIL),
            ..Style::default()
        }
    }

    /// One row of the channel, member or DM list.
    pub fn row(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: match status {
                Status::Hovered => Some(Background::Color(tokens.bg_hover)),
                Status::Pressed => Some(Background::Color(tokens.bg_active)),
                Status::Active | Status::Disabled => None,
            },
            text_color: match status {
                Status::Disabled => tokens.text_muted,
                Status::Active => tokens.text_secondary,
                Status::Hovered | Status::Pressed => tokens.text_primary,
            },
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// The row in view. It keeps its ground whatever the pointer does.
    pub fn row_selected(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match status {
                Status::Hovered | Status::Pressed => tokens.bg_active,
                Status::Active | Status::Disabled => tokens.bg_selected,
            })),
            text_color: tokens.text_primary,
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// The neutral ground a row keeps while its own context menu is open: the
    /// pointer is on the menu by then, so the hover that opened it is gone.
    pub fn row_open(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, _status| Style {
            background: Some(Background::Color(tokens.bg_active)),
            text_color: tokens.text_primary,
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// `row` or `row_selected` by a runtime flag. The two return distinct
    /// opaque types, so an `if` over them cannot be one expression.
    pub fn row_for(tokens: &ThemeTokens, selected: bool) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |theme, status| {
            if selected {
                row_selected(&tokens)(theme, status)
            } else {
                row(&tokens)(theme, status)
            }
        }
    }

    /// Which of the three row grounds one row draws. Selection wins over an open
    /// menu: a row can be both.
    pub fn row_state(
        tokens: &ThemeTokens,
        selected: bool,
        open: bool,
    ) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |theme, status| match (selected, open) {
            (true, _) => row_selected(&tokens)(theme, status),
            (false, true) => row_open(&tokens)(theme, status),
            (false, false) => row(&tokens)(theme, status),
        }
    }

    /// A reaction chip: the accent behind it once this account is in it.
    pub fn reaction(tokens: &ThemeTokens, mine: bool) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match (mine, status) {
                (true, _) => tokens.accent_tint,
                (false, Status::Hovered | Status::Pressed) => tokens.control_bg_hover,
                (false, _) => tokens.bg_elevated,
            })),
            text_color: if mine {
                tokens.text_primary
            } else {
                tokens.text_secondary
            },
            border: border::rounded(RADIUS_CONTROL).width(1.0).color(if mine {
                tokens.accent
            } else {
                tokens.border_subtle
            }),
            ..Style::default()
        }
    }

    /// A bare icon: the ground appears under the pointer, nothing else.
    pub fn icon(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: match status {
                Status::Hovered => Some(Background::Color(tokens.bg_hover)),
                Status::Pressed => Some(Background::Color(tokens.bg_active)),
                Status::Active | Status::Disabled => None,
            },
            text_color: match status {
                Status::Disabled => Color {
                    a: tokens.text_muted.a * DISABLED_ALPHA,
                    ..tokens.text_muted
                },
                _ => tokens.text_secondary,
            },
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// A filled square icon button: the voice card's switches, which read as
    /// controls rather than as bare glyphs.
    pub fn icon_filled(tokens: &ThemeTokens) -> impl Fn(&Theme, Status) -> Style {
        let tokens = *tokens;
        move |_theme, status| Style {
            background: Some(Background::Color(match status {
                Status::Hovered => tokens.control_bg_hover,
                Status::Pressed => tokens.bg_active,
                Status::Active | Status::Disabled => tokens.control_bg,
            })),
            text_color: match status {
                Status::Disabled => faded(tokens.text_secondary),
                _ => tokens.text_primary,
            },
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }
}

pub mod container {
    //! The grounds: one per surface token, plus the shapes built on them.

    use iced::widget::container::Style;
    use iced::{Background, Color, Theme, border};

    use super::{
        RADIUS_CARD, RADIUS_CHIP, RADIUS_CONTROL, RADIUS_POPOVER, ThemeTokens, ToastKind, hairline,
        lift,
    };

    fn ground(color: Color) -> Style {
        Style {
            background: Some(Background::Color(color)),
            ..Style::default()
        }
    }

    pub fn rail(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.bg_rail)
    }

    pub fn sidebar(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.bg_sidebar)
    }

    pub fn chat(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.bg_chat)
    }

    pub fn elevated(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.bg_elevated)
    }

    /// A menu, a profile card, the quick switcher: off the surface, with a
    /// shadow under it.
    pub fn popover(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.bg_elevated)),
            border: border::rounded(RADIUS_POPOVER)
                .width(1.0)
                .color(tokens.border_strong),
            shadow: lift(4.0, 16.0),
            ..Style::default()
        }
    }

    /// The ground a text input or the composer sits on.
    pub fn input(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.bg_input)),
            border: hairline(&tokens, RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// An unread count, in the accent.
    pub fn badge(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.accent)),
            text_color: Some(tokens.text_on_accent),
            border: border::rounded(RADIUS_POPOVER),
            ..Style::default()
        }
    }

    /// A mention count, which is louder than an unread one.
    pub fn mention_badge(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.mention)),
            text_color: Some(tokens.text_on_accent),
            border: border::rounded(RADIUS_POPOVER),
            ..Style::default()
        }
    }

    /// A one-pixel line between things.
    pub fn divider(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.divider)
    }

    pub fn toast(tokens: &ThemeTokens, kind: ToastKind) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.bg_elevated)),
            text_color: Some(tokens.text_primary),
            border: border::rounded(RADIUS_CARD).width(1.0).color(match kind {
                ToastKind::Info => tokens.border_strong,
                ToastKind::Error => tokens.danger,
            }),
            shadow: lift(2.0, 12.0),
            ..Style::default()
        }
    }

    /// What a modal dims the window with.
    pub fn backdrop(_tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        move |_theme| {
            ground(Color {
                a: 0.45,
                ..Color::BLACK
            })
        }
    }

    /// A settings card, a profile panel, a dialog body.
    pub fn card(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.bg_elevated)),
            border: hairline(&tokens, RADIUS_CARD),
            ..Style::default()
        }
    }

    /// A chip: a role, a pending attachment, a keyboard hint.
    pub fn chip(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.control_bg)),
            text_color: Some(tokens.text_secondary),
            border: border::rounded(RADIUS_CHIP)
                .width(1.0)
                .color(tokens.control_border),
            ..Style::default()
        }
    }

    /// The line the "new messages" divider draws across the list.
    pub fn mention_line(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| ground(tokens.mention)
    }

    /// An outline with nothing behind it: the day divider in the message list.
    pub fn pill(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            border: border::rounded(RADIUS_POPOVER)
                .width(1.0)
                .color(tokens.border_subtle),
            ..Style::default()
        }
    }

    /// One message row, which takes a ground while the pointer is on it.
    pub fn message_row(tokens: &ThemeTokens, hovered: bool) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: hovered.then_some(Background::Color(tokens.bg_hover)),
            border: border::rounded(RADIUS_CONTROL),
            ..Style::default()
        }
    }

    /// The strip of actions that appears over a hovered message.
    pub fn hover_strip(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(tokens.bg_elevated)),
            border: border::rounded(RADIUS_CONTROL)
                .width(1.0)
                .color(tokens.border_subtle),
            shadow: lift(2.0, 8.0),
            ..Style::default()
        }
    }

    /// A chip that warns rather than informs: what the composer says about an
    /// `@everyone` nobody would receive.
    pub fn warning_chip(tokens: &ThemeTokens) -> impl Fn(&Theme) -> Style {
        let tokens = *tokens;
        move |_theme| Style {
            background: Some(Background::Color(Color {
                a: 0.16,
                ..tokens.warning
            })),
            text_color: Some(tokens.warning),
            border: border::rounded(RADIUS_CHIP)
                .width(1.0)
                .color(tokens.warning),
            ..Style::default()
        }
    }
}

/// `use<>`: the closure copies the tokens, so it must not capture the borrow —
/// one caller styles a field from tokens it builds on the spot.
pub fn text_input(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, text_input_widget::Status) -> text_input_widget::Style + use<> {
    use text_input_widget::Status;

    let tokens = *tokens;
    move |_theme, status| text_input_widget::Style {
        background: Background::Color(tokens.bg_input),
        border: border::rounded(RADIUS_CONTROL)
            .width(1.0)
            .color(match status {
                Status::Focused { .. } => tokens.accent,
                Status::Hovered => tokens.border_strong,
                Status::Active | Status::Disabled => tokens.border_subtle,
            }),
        icon: tokens.text_muted,
        placeholder: tokens.text_muted,
        value: match status {
            Status::Disabled => tokens.text_muted,
            _ => tokens.text_primary,
        },
        selection: tokens.accent_tint,
    }
}

/// The composer's editor, which draws no ground of its own: the box around it
/// carries the input's ground and border, with the attach button inside the same
/// outline.
pub fn text_editor_bare(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, text_editor_widget::Status) -> text_editor_widget::Style {
    use text_editor_widget::Status;

    let tokens = *tokens;
    move |_theme, status| text_editor_widget::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        placeholder: tokens.text_muted,
        value: match status {
            Status::Disabled => tokens.text_muted,
            _ => tokens.text_primary,
        },
        selection: tokens.accent_tint,
    }
}

pub fn scrollable(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, scrollable_widget::Status) -> scrollable_widget::Style {
    use scrollable_widget::{AutoScroll, Rail, Scroller, Status, Style};

    let tokens = *tokens;
    move |_theme, status| {
        let hovered = matches!(status, Status::Hovered { .. } | Status::Dragged { .. });
        let rail = Rail {
            background: None,
            border: border::rounded(RADIUS_CHIP),
            scroller: Scroller {
                background: Background::Color(if hovered {
                    tokens.control_border
                } else {
                    tokens.track
                }),
                border: border::rounded(RADIUS_CHIP),
            },
        };
        Style {
            container: iced::widget::container::Style::default(),
            vertical_rail: rail,
            horizontal_rail: rail,
            gap: None,
            auto_scroll: AutoScroll {
                background: Background::Color(tokens.bg_elevated),
                border: border::rounded(RADIUS_POPOVER)
                    .width(1.0)
                    .color(tokens.border_strong),
                shadow: lift(0.0, 4.0),
                icon: tokens.text_secondary,
            },
        }
    }
}

pub fn slider(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, slider_widget::Status) -> slider_widget::Style {
    use slider_widget::{Handle, HandleShape, Rail, Status, Style};

    let tokens = *tokens;
    move |_theme, status| {
        let filled = match status {
            Status::Active => tokens.accent,
            Status::Hovered => tokens.accent_hover,
            Status::Dragged => tokens.accent_pressed,
        };
        Style {
            rail: Rail {
                backgrounds: (Background::Color(filled), Background::Color(tokens.track)),
                width: 4.0,
                border: border::rounded(2.0),
            },
            handle: Handle {
                shape: HandleShape::Circle { radius: 7.0 },
                background: Background::Color(tokens.thumb),
                border_width: 0.0,
                border_color: Color::TRANSPARENT,
            },
        }
    }
}

pub fn toggler(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, toggler_widget::Status) -> toggler_widget::Style {
    use toggler_widget::{Status, Style};

    let tokens = *tokens;
    move |_theme, status| {
        let (toggled, disabled) = match status {
            Status::Active { is_toggled } | Status::Hovered { is_toggled } => (is_toggled, false),
            Status::Disabled { is_toggled } => (is_toggled, true),
        };
        let background = match (toggled, disabled) {
            (true, false) => tokens.accent,
            (true, true) => faded(tokens.accent),
            (false, false) => tokens.track,
            (false, true) => faded(tokens.track),
        };
        Style {
            background: Background::Color(background),
            background_border_width: 0.0,
            background_border_color: Color::TRANSPARENT,
            foreground: Background::Color(if disabled {
                faded(tokens.thumb)
            } else {
                tokens.thumb
            }),
            foreground_border_width: 0.0,
            foreground_border_color: Color::TRANSPARENT,
            text_color: Some(tokens.text_primary),
            border_radius: None,
            padding_ratio: 0.1,
        }
    }
}

pub fn pick_list(
    tokens: &ThemeTokens,
) -> impl Fn(&Theme, pick_list_widget::Status) -> pick_list_widget::Style {
    use pick_list_widget::{Status, Style};

    let tokens = *tokens;
    move |_theme, status| Style {
        text_color: tokens.text_primary,
        placeholder_color: tokens.text_muted,
        handle_color: tokens.text_secondary,
        background: Background::Color(match status {
            Status::Hovered => tokens.control_bg_hover,
            _ => tokens.control_bg,
        }),
        border: border::rounded(RADIUS_CONTROL)
            .width(1.0)
            .color(match status {
                Status::Opened { .. } => tokens.accent,
                _ => tokens.control_border,
            }),
    }
}

/// The pick list's dropped-open list, which is a popover of its own.
pub fn menu(tokens: &ThemeTokens) -> impl Fn(&Theme) -> menu_widget::Style {
    let tokens = *tokens;
    move |_theme| menu_widget::Style {
        background: Background::Color(tokens.bg_elevated),
        border: border::rounded(RADIUS_CONTROL)
            .width(1.0)
            .color(tokens.border_strong),
        text_color: tokens.text_primary,
        selected_text_color: tokens.text_primary,
        selected_background: Background::Color(tokens.bg_active),
        shadow: lift(4.0, 12.0),
    }
}

pub fn rule(tokens: &ThemeTokens) -> impl Fn(&Theme) -> rule_widget::Style {
    let tokens = *tokens;
    move |_theme| rule_widget::Style {
        color: tokens.border_subtle,
        radius: 0.0.into(),
        fill_mode: rule_widget::FillMode::Full,
        snap: true,
    }
}

/// An icon is one colour, whatever the SVG says.
pub fn svg(color: Color) -> impl Fn(&Theme, svg_widget::Status) -> svg_widget::Style {
    move |_theme, _status| svg_widget::Style { color: Some(color) }
}

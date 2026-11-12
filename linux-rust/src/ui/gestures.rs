//! The "Controls" card: one row per stem gesture, each with a dropdown of
//! what the gesture does.

use crate::gestures::{StemAction, StemActions, StemGesture};
use crate::ui::window::Message;
use iced::border::Radius;
use iced::overlay::menu;
use iced::widget::button::Style;
use iced::widget::rule::FillMode;
use iced::widget::{Column, button, column, container, pick_list, row, rule, text};
use iced::{Background, Border, Center, Length, Padding, Theme};

/// An entry in a gesture's dropdown.
#[derive(Debug, Clone, PartialEq)]
pub enum StemChoice {
    Action(StemAction),
    /// Start listening for the key or shortcut to bind.
    CaptureKey,
}

impl std::fmt::Display for StemChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Action(action) => action.fmt(f),
            Self::CaptureKey => write!(f, "Keyboard key…"),
        }
    }
}

fn muted(theme: &Theme) -> text::Style {
    let mut style = text::Style::default();
    style.color = Some(theme.palette().text.scale_alpha(0.7));
    style
}

/// `capturing`: the gesture whose key is being recorded, if any.
pub fn stem_controls_card<'a>(
    actions: &StemActions,
    capturing: Option<StemGesture>,
) -> iced::widget::Container<'a, Message> {
    let mut rows: Column<'a, Message> = column![].spacing(4).padding(8);

    for gesture in StemGesture::ALL {
        let current = actions.get(gesture).clone();

        let label = column![
            text(gesture.label()).size(16),
            text(format!("AirPods default: {}", gesture.native_action()))
                .size(12)
                .style(muted)
                .width(Length::Fill),
        ]
        .width(Length::Fill);

        let control: iced::Element<'a, Message> = if capturing == Some(gesture) {
            row![
                text("Press a key or shortcut…").size(14),
                button(text("Cancel").size(14))
                    .on_press(Message::StemKeyCaptureCancelled)
                    .style(|theme: &Theme, _status| {
                        let mut style = Style::default();
                        style.background =
                            Some(Background::Color(theme.palette().primary.scale_alpha(0.2)));
                        style.text_color = theme.palette().text;
                        style.border = Border::default().rounded(8.0);
                        style
                    })
                    .padding(Padding {
                        top: 4.0,
                        bottom: 4.0,
                        left: 10.0,
                        right: 10.0,
                    }),
            ]
            .align_y(Center)
            .spacing(10)
            .into()
        } else {
            let mut choices: Vec<StemChoice> = StemAction::BUILT_IN
                .into_iter()
                .map(StemChoice::Action)
                .collect();
            if matches!(current, StemAction::Key(_)) {
                choices.push(StemChoice::Action(current.clone()));
            }
            choices.push(StemChoice::CaptureKey);

            pick_list(choices, Some(StemChoice::Action(current)), move |choice| {
                Message::StemChoiceSelected(gesture, choice)
            })
            .text_size(14)
            .width(Length::from(240))
            .padding(Padding {
                top: 5.0,
                bottom: 5.0,
                left: 10.0,
                right: 10.0,
            })
            .style(|theme: &Theme, _status| pick_list::Style {
                text_color: theme.palette().text,
                placeholder_color: theme.palette().text.scale_alpha(0.7),
                handle_color: theme.palette().text,
                background: Background::Color(theme.palette().primary.scale_alpha(0.2)),
                border: Border {
                    width: 1.0,
                    color: theme.palette().text.scale_alpha(0.3),
                    radius: Radius::from(4.0),
                },
            })
            .menu_style(|theme: &Theme| menu::Style {
                background: Background::Color(theme.palette().background),
                border: Border {
                    width: 1.0,
                    color: theme.palette().text,
                    radius: Radius::from(4.0),
                },
                text_color: theme.palette().text,
                selected_text_color: theme.palette().text,
                selected_background: Background::Color(theme.palette().primary.scale_alpha(0.3)),
                shadow: Default::default(),
            })
            .into()
        };

        rows = rows.push(row![label, control].align_y(Center).spacing(8));
        rows = rows.push(rule::horizontal(1).style(|theme: &Theme| rule::Style {
            color: theme.palette().text.scale_alpha(0.2),
            radius: Radius::from(12),
            fill_mode: FillMode::Full,
            snap: false,
        }));
    }

    rows = rows.push(
        text(
            "A remapped gesture is carried out by LibrePods on this computer instead of by the \
             AirPods. A keyboard key is pressed as if you had typed it, so set the same key as \
             the shortcut in the other app (for example Discord's Toggle Mute keybind).",
        )
        .size(12)
        .style(muted)
        .width(Length::Fill),
    );

    container(rows)
        .padding(Padding {
            top: 5.0,
            bottom: 5.0,
            left: 10.0,
            right: 10.0,
        })
        .style(|theme: &Theme| {
            let mut style = container::Style::default();
            style.background = Some(Background::Color(theme.palette().primary.scale_alpha(0.1)));
            let mut border = Border::default();
            border.color = theme.palette().primary.scale_alpha(0.5);
            style.border = border.rounded(16);
            style
        })
}

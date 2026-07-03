use std::time::{SystemTime, UNIX_EPOCH};

use cosmic::app::Core;
use cosmic::applet::cosmic_panel_config::PanelAnchor;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::widget::{column, row};
use cosmic::iced::{Alignment, Subscription, window};
use cosmic::widget::{button, container, divider, icon, text};
use cosmic::{Element, Task, theme};

use crate::dbus::{self, DeviceInfo, State};

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}

pub struct Window {
    core: Core,
    popup: Option<window::Id>,
    /// `None` until the first subscription message or while the daemon is
    /// unreachable.
    state: State,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    State(State),
    /// Periodic re-render while the popup is open ("N s ago" freshness).
    Tick,
}

impl Window {
    /// The device shown in the panel: prefer a connected one, fall back to
    /// anything with battery data (last known value while asleep).
    fn primary(&self) -> Option<&DeviceInfo> {
        let devices = self.state.as_ref()?;
        devices
            .iter()
            .find(|d| d.connected && d.has_battery_data())
            .or_else(|| devices.iter().find(|d| d.has_battery_data()))
    }

    fn panel_icon(&self) -> String {
        let Some(dev) = self.primary() else {
            return "battery-missing-symbolic".to_string();
        };
        // Round to the nearest icon step (0, 10, .., 100).
        let level = (usize::from(dev.level.min(100)) + 5) / 10 * 10;
        let suffix = match (level, dev.charging || dev.charge_state == 4) {
            (_, false) => "",
            (100, true) => "-charged",
            (_, true) => "-charging",
        };
        format!("battery-level-{level}{suffix}-symbolic")
    }

    #[allow(clippy::unused_self)] // consistency with view methods
    fn device_details<'a>(&self, dev: &'a DeviceInfo) -> Element<'a, Message> {
        let title = row![
            text::title4(&dev.name),
            cosmic::widget::space::horizontal(),
            text::title4(if dev.has_battery_data() {
                format!("{}%", dev.level)
            } else {
                "—".to_string()
            }),
        ]
        .align_y(Alignment::Center);

        let status = if dev.connected {
            dev.charge_state_label().to_string()
        } else if dev.has_battery_data() {
            format!("{} (asleep, last known)", dev.charge_state_label())
        } else {
            "Asleep or disconnected".to_string()
        };

        let mut rows: Vec<Element<'a, Message>> = vec![title.into(), text::body(status).into()];

        if dev.has_battery_data() {
            let volts = |mv: u16| format!("{:.2} V", f32::from(mv) / 1000.0);
            let mut detail = |label: &str, value: String| {
                rows.push(
                    row![
                        text::caption(label.to_string()),
                        cosmic::widget::space::horizontal(),
                        text::caption(value),
                    ]
                    .align_y(Alignment::Center)
                    .into(),
                );
            };
            detail("Battery voltage", volts(dev.battery_voltage));
            detail("System voltage", volts(dev.system_voltage));
            if dev.input_voltage > 0 {
                detail("Input voltage", volts(dev.input_voltage));
                detail("Input current", format!("{} mA", dev.input_current));
            }
            detail("Current", format!("{} mA", dev.current));
            detail(
                "Temperature",
                format!("{:.1} °C", f32::from(dev.temperature) / 1000.0),
            );
            if dev.last_updated > 0 {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let ago = now.saturating_sub(dev.last_updated);
                let ago = if ago < 60 {
                    format!("{ago} s ago")
                } else if ago < 3600 {
                    format!("{} min ago", ago / 60)
                } else {
                    format!("{} h ago", ago / 3600)
                };
                detail("Updated", ago);
            }
        }

        cosmic::iced::widget::Column::with_children(rows)
            .spacing(4)
            .into()
    }
}

impl cosmic::Application for Window {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "io.github.steambattery.Applet";

    fn init(core: Core, (): Self::Flags) -> (Self, Task<cosmic::Action<Message>>) {
        (
            Self {
                core,
                popup: None,
                state: None,
            },
            Task::none(),
        )
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![dbus::subscription().map(Message::State)];
        if self.popup.is_some() {
            subs.push(
                cosmic::iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::Tick),
            );
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p);
                }
                let new_id = window::Id::unique();
                self.popup = Some(new_id);
                let popup_settings = self.core.applet.get_popup_settings(
                    self.core.main_window_id().unwrap(),
                    new_id,
                    Some((1, 1)),
                    None,
                    None,
                );
                get_popup(popup_settings)
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                }
                Task::none()
            }
            Message::State(state) => {
                self.state = state;
                Task::none()
            }
            Message::Tick => Task::none(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );
        let suggested = self.core.applet.suggested_size(true);
        let padding = self.core.applet.suggested_padding(true);

        let mut children: Vec<Element<'_, Message>> = vec![
            icon::from_name(self.panel_icon())
                .size(suggested.0)
                .symbolic(true)
                .into(),
        ];
        if let Some(dev) = self.primary() {
            children.push(
                self.core
                    .applet
                    .text(format!("{}%", dev.level))
                    .font(cosmic::font::default())
                    .into(),
            );
        }

        let inner: Element<'_, Message> = if horizontal {
            cosmic::iced::widget::Row::with_children(children)
                .align_y(Alignment::Center)
                .spacing(4)
                .into()
        } else {
            cosmic::iced::widget::Column::with_children(children)
                .align_x(Alignment::Center)
                .spacing(4)
                .into()
        };

        let button = button::custom(inner)
            .padding(if horizontal {
                [0, padding.0]
            } else {
                [padding.0, 0]
            })
            .on_press_down(Message::TogglePopup)
            .class(theme::Button::AppletIcon);

        self.core.applet.autosize_window(button).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let spacing = theme::active().cosmic().spacing;

        let content: Element<'_, Message> = self.state.as_ref().map_or_else(
            || text::body("steambatteryd is not running").into(),
            |devices| {
                // Slots that have never seen a controller are noise.
                let interesting: Vec<&DeviceInfo> = devices
                    .iter()
                    .filter(|d| d.connected || d.has_battery_data())
                    .collect();
                if interesting.is_empty() {
                    text::body("No Steam Controller detected").into()
                } else {
                    let mut col = column![].spacing(spacing.space_s);
                    for (i, dev) in interesting.iter().enumerate() {
                        if i > 0 {
                            col = col.push(divider::horizontal::default());
                        }
                        col = col.push(self.device_details(dev));
                    }
                    col.into()
                }
            },
        );

        self.core
            .applet
            .popup_container(container(content).padding([spacing.space_s, spacing.space_m]))
            .into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

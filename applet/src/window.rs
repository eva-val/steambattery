use std::time::{SystemTime, UNIX_EPOCH};

use cosmic::app::Core;
use cosmic::applet::cosmic_panel_config::PanelAnchor;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::widget::{column, row};
use cosmic::iced::{Alignment, Length, Subscription, window};
use cosmic::widget::{button, container, divider, icon, text};
use cosmic::{Element, Task, theme};

use crate::dbus::{self, ChargeState, DeviceInfo, State};

/// Themed gamepad glyph used as the panel identity. Loaded from the active
/// icon theme at runtime, so it adapts per system.
const GAMEPAD_ICON: &str = "input-gaming-symbolic";

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

    /// Name of the stock `battery-level-*` glyph for the current state; used as
    /// the corner badge on the gamepad (and as a standalone fallback).
    fn battery_icon(&self) -> String {
        let Some(dev) = self.primary() else {
            return "battery-missing-symbolic".to_string();
        };
        // Round to the nearest icon step (0, 10, .., 100).
        let level = (usize::from(dev.level.min(100)) + 5) / 10 * 10;
        let plugged = dev.charging() || dev.charge_state == ChargeState::ChargingDone;
        let suffix = match (level, plugged) {
            (_, false) => "",
            (100, true) => "-charged",
            (_, true) => "-charging",
        };
        format!("battery-level-{level}{suffix}-symbolic")
    }

    /// Panel glyph: the themed gamepad, dimmed to a watermark, with the battery
    /// state stacked as a badge in the bottom-right corner. Both layers are
    /// themed symbolic icons — nothing is bundled. Separation is by opacity, not
    /// colour: COSMIC recolours every symbolic icon to the one panel foreground,
    /// so two solid layers would merge into each other. Dimming the base is the
    /// same trick Adwaita's own multi-layer symbolic icons use.
    fn panel_glyph(&self, size: u16) -> Element<'_, Message> {
        let sf = f32::from(size);
        let base: Element<'_, Message> = icon::from_name(GAMEPAD_ICON)
            .icon()
            .into_svg_handle()
            .map_or_else(
                // Theme ships the gamepad only as a raster: fall back to a solid icon.
                || {
                    icon::from_name(GAMEPAD_ICON)
                        .size(size)
                        .symbolic(true)
                        .into()
                },
                |handle| {
                    cosmic::widget::Svg::new(handle)
                        .symbolic(true)
                        .opacity(0.34_f32)
                        .width(Length::Fixed(sf))
                        .height(Length::Fixed(sf))
                        .into()
                },
            );

        // ~56% of the icon box, floored, but never so small it's illegible.
        let badge_px = (size.saturating_mul(56) / 100).max(8);
        let badge = container(
            icon::from_name(self.battery_icon())
                .size(badge_px)
                .symbolic(true),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::End);

        // Pin the stack to the full icon box so `autosize_window` still sizes the
        // panel slot from `size`, even though the badge only fills a corner.
        cosmic::iced::widget::Stack::new()
            .push(base)
            .push(badge)
            .width(Length::Fixed(sf))
            .height(Length::Fixed(sf))
            .into()
    }

    fn device_details<'a>(dev: &'a DeviceInfo) -> Element<'a, Message> {
        // The name (e.g. "Steam Controller (puck slot 0)") can be wide; let it
        // take the remaining width and wrap rather than pushing the popup out.
        let title = row![
            text::title4(&dev.name).width(Length::Fill),
            text::title4(dev.level_label()),
        ]
        .spacing(8)
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
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "io.github.steambattery.Applet";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

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

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
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

        let mut children: Vec<Element<'_, Message>> = vec![self.panel_glyph(suggested.0)];
        if let Some(dev) = self.primary() {
            children.push(
                self.core
                    .applet
                    .text(dev.level_label())
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
                        col = col.push(Self::device_details(dev));
                    }
                    col.into()
                }
            },
        );

        // A fixed content width bounds the title/detail rows so long names wrap
        // and the label/value pairs justify left/right instead of the popup
        // autosizing to whatever the widest string happens to be.
        self.core
            .applet
            .popup_container(
                container(content)
                    .width(Length::Fixed(320.0))
                    .padding([spacing.space_s, spacing.space_m]),
            )
            .into()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

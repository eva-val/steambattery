//! COSMIC panel applet showing Steam Controller 2 battery status, fed by
//! steambatteryd over the session D-Bus.

mod dbus;
mod window;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cosmic_applet_steambattery=info".into()),
        )
        .init();
    window::run()
}

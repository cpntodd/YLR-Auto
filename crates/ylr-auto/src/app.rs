//! The application: startup, the tray, notifications and the theme.

use crate::config::{Config, Paths, BACKGROUND_FLAG};
use crate::logging;
use crate::tray::{Tray, TrayEvent, TrayState};
use crate::window::MainWindow;
use crate::{APP_ID, APP_NAME};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::env;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use ylr_core::VERSION;

/// VSCodium's Dark+ palette, mapped onto libadwaita's colour variables so every
/// standard widget follows it. Dim text is raised from libadwaita's default opacity,
/// which falls below 4.5:1 contrast on these greys.
const DARK_GREY_THEME_CSS: &str = "
:root {
  --window-bg-color: #1e1e1e;
  --window-fg-color: #cccccc;
  --view-bg-color: #1e1e1e;
  --view-fg-color: #cccccc;
  --headerbar-bg-color: #323233;
  --headerbar-fg-color: #cccccc;
  --headerbar-backdrop-color: #2d2d2d;
  --sidebar-bg-color: #252526;
  --sidebar-fg-color: #cccccc;
  --card-bg-color: #2d2d30;
  --card-fg-color: #cccccc;
  --dialog-bg-color: #252526;
  --dialog-fg-color: #cccccc;
  --popover-bg-color: #2d2d30;
  --popover-fg-color: #cccccc;
  --accent-bg-color: #0e639c;
  --accent-fg-color: #ffffff;
  --accent-color: #3794ff;
  --dim-opacity: 75%;
}
.ylr-sidebar { background-color: var(--sidebar-bg-color); color: var(--sidebar-fg-color); }
banner > revealer > widget { background-color: #333337; color: #cccccc; }
";

pub struct AppState {
    application: adw::Application,
    paths: Paths,
    start_hidden: Cell<bool>,
    window: RefCell<Option<Rc<MainWindow>>>,
    tray: RefCell<Option<Tray>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    /// Applies the app's colours; the CSS provider exists only while in use.
    theme_provider: RefCell<Option<gtk::CssProvider>>,
}

impl AppState {
    pub fn new(start_hidden: bool) -> Rc<Self> {
        let application = adw::Application::builder().application_id(APP_ID).build();
        let state = Rc::new(Self {
            application,
            paths: Paths::from_environment(),
            start_hidden: Cell::new(start_hidden),
            window: RefCell::new(None),
            tray: RefCell::new(None),
            hold: RefCell::new(None),
            theme_provider: RefCell::new(None),
        });

        state.application.connect_startup(glib::clone!(
            #[strong]
            state,
            move |_| state.on_startup()
        ));
        state.application.connect_activate(glib::clone!(
            #[strong]
            state,
            move |_| state.on_activate()
        ));
        state
    }

    pub fn application(&self) -> &adw::Application {
        &self.application
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn window(&self) -> Option<Rc<MainWindow>> {
        self.window.borrow().clone()
    }

    fn on_startup(self: &Rc<Self>) {
        logging::init(&self.paths.log_file);
        let config = Config::load(&self.paths, None);
        self.apply_theme(config.flag("use_system_theme"));

        let show = gio::SimpleAction::new("show", None);
        show.connect_activate(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |_, _| state.show_window()
        ));
        self.application.add_action(&show);

        let show_rip = gio::SimpleAction::new("show-rip", Some(glib::VariantTy::STRING));
        show_rip.connect_activate(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |_, folder| {
                state.show_window();
                let folder = folder.and_then(|folder| folder.str().map(str::to_string));
                if let (Some(window), Some(folder)) = (state.window(), folder) {
                    window.select_rip(Path::new(&folder));
                }
            }
        ));
        self.application.add_action(&show_rip);

        // Held from the start, so a hidden start survives until the tray answers.
        *self.hold.borrow_mut() = Some(self.application.hold());
        let (tray, events) = Tray::start();
        *self.tray.borrow_mut() = Some(tray);
        let state = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                state.on_tray_event(event);
            }
        });
    }

    fn on_activate(self: &Rc<Self>) {
        if self.window().is_none() {
            gtk::Window::set_default_icon_name(APP_ID);
            let window = MainWindow::new(self);
            *self.window.borrow_mut() = Some(window);
        }
        if self.start_hidden.replace(false) {
            return;
        }
        if let Some(window) = self.window() {
            window.widget().present();
        }
    }

    fn show_window(&self) {
        self.application.activate();
    }

    pub fn tray_is_ready(&self) -> bool {
        self.tray.borrow().as_ref().is_some_and(Tray::is_ready)
    }

    pub fn show_tray_state(&self, state: TrayState) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.show_state(state);
        }
    }

    fn on_tray_event(self: &Rc<Self>, event: TrayEvent) {
        match event {
            TrayEvent::Ready => {
                if let Some(tray) = self.tray.borrow().as_ref() {
                    tray.set_ready(true);
                }
                if let Some(window) = self.window() {
                    window.poll_monitor();
                }
            }
            TrayEvent::Unavailable => self.on_tray_lost(),
            TrayEvent::Show => self.show_window(),
            TrayEvent::Quit => self.request_quit(),
        }
    }

    /// With no tray a hidden window could not be reached, so bring it back and let
    /// closing it end the app.
    fn on_tray_lost(&self) {
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.set_ready(false);
        }
        log::warn!("The tray icon is unavailable, so closing the window will quit.");
        if let Some(window) = self.window().filter(|window| !window.widget().is_visible()) {
            window.widget().present();
        }
        let hold = self.hold.borrow_mut().take();
        drop(hold);
    }

    fn request_quit(&self) {
        if let Some(window) = self
            .window()
            .filter(|window| window.is_filing_in_progress())
        {
            self.show_window();
            window.toast("A rip is being filed. Quit once it finishes.");
            return;
        }
        if let Some(window) = self
            .window()
            .filter(|window| window.running_drive_job_count() > 0)
        {
            self.show_window();
            window.confirm_quit_while_ripping();
            return;
        }
        self.application.quit();
    }

    pub fn notify(&self, id: &str, title: &str, body: &str, folder: Option<&Path>) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_icon(&gio::ThemedIcon::new(APP_ID));
        match folder {
            Some(folder) => {
                let target = folder.to_string_lossy().to_variant();
                notification.set_default_action_and_target_value("app.show-rip", Some(&target));
            }
            None => notification.set_default_action("app.show"),
        }
        self.application.send_notification(Some(id), &notification);
    }

    pub fn apply_theme(&self, use_system_theme: bool) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let previous = self.theme_provider.borrow_mut().take();
        if let Some(previous) = previous {
            gtk::style_context_remove_provider_for_display(&display, &previous);
        }

        let style_manager = adw::StyleManager::default();
        if use_system_theme {
            style_manager.set_color_scheme(adw::ColorScheme::Default);
            return;
        }
        style_manager.set_color_scheme(adw::ColorScheme::ForceDark);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(DARK_GREY_THEME_CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        *self.theme_provider.borrow_mut() = Some(provider);
    }
}

pub fn run() -> ExitCode {
    let mut arguments: Vec<String> = env::args().collect();
    if arguments.iter().any(|argument| argument == "--version") {
        println!("{APP_NAME} {VERSION}");
        return ExitCode::SUCCESS;
    }
    let start_hidden = arguments.iter().any(|argument| argument == BACKGROUND_FLAG);
    arguments.retain(|argument| argument != BACKGROUND_FLAG);

    let state = AppState::new(start_hidden);
    if let Err(e) = state.application.register(gio::Cancellable::NONE) {
        eprintln!("Could not start {APP_NAME}: {e}. Check that a desktop session is running.");
        return ExitCode::FAILURE;
    }
    // A login start must not raise the window of a copy that is already running.
    if start_hidden && state.application.is_remote() {
        return ExitCode::SUCCESS;
    }
    let status = state.application.run_with_args(&arguments);
    ExitCode::from(status.get())
}

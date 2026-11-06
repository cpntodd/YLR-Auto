//! Helpers shared by the app's windows.

use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, pango};
use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use ylr_core::tmdb::{self, Match};

/// Gtk.Image fits a poster inside a square of this size, keeping its shape.
pub const POSTER_DISPLAY_PIXELS: i32 = 60;
/// Fits a laptop screen, and stays within the 500x750 image downloaded for the preview.
const POSTER_PREVIEW_PIXELS: f64 = 600.0;
const POSTER_PREVIEW_DELAY: Duration = Duration::from_millis(2000);

/// Run `work` on a thread, then call `on_done` on the GTK main loop. A panic in `work`
/// reaches `on_done` as an error, so a flag such as "scanning" cannot stay set forever.
/// A plain thread, not a pool, because work on a dead network share can block for a
/// long time and must not hold up the rest.
pub fn run_in_background<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    on_done: impl FnOnce(Result<T, String>) + 'static,
) {
    let on_done = glib::thread_guard::ThreadGuard::new(on_done);
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| {
            Err("An internal error occurred; see the terminal output.".to_string())
        });
        glib::MainContext::default().invoke(move || (on_done.into_inner())(result));
    });
}

pub fn size_text(size: u64) -> String {
    let size = size as f64;
    if size >= 1e8 {
        format!("{:.1} GB", size / 1e9)
    } else {
        format!("{:.0} MB", size / 1e6)
    }
}

pub fn rate_text(bytes_per_second: f64) -> String {
    format!("{:.1} MB/s", bytes_per_second / 1e6)
}

pub fn elapsed_text(seconds: f64) -> String {
    let total_minutes = (seconds / 60.0).floor() as i64;
    if total_minutes < 60 {
        return format!("{total_minutes} min");
    }
    format!("{} h {} min", total_minutes / 60, total_minutes % 60)
}

pub fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

pub fn clear_grid(grid: &gtk::Grid) {
    while let Some(child) = grid.first_child() {
        grid.remove(&child);
    }
}

pub fn labelled(text: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::builder().spacing(8).build();
    row.append(&gtk::Label::builder().label(text).xalign(0.0).build());
    row.append(widget);
    row
}

mod exact_size {
    use super::*;

    #[derive(Default)]
    pub struct ExactSizePaintable {
        pub paintable: RefCell<Option<gdk::Paintable>>,
        pub size: Cell<(i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ExactSizePaintable {
        const NAME: &'static str = "YlrExactSizePaintable";
        type Type = super::ExactSizePaintable;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ExactSizePaintable {}

    impl WidgetImpl for ExactSizePaintable {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let (width, height) = self.size.get();
            let length = if orientation == gtk::Orientation::Horizontal {
                width
            } else {
                height
            };
            (length, length, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(paintable) = self.paintable.borrow().as_ref() {
                paintable.snapshot(
                    snapshot,
                    f64::from(widget.width()),
                    f64::from(widget.height()),
                );
            }
        }
    }
}

glib::wrapper! {
    /// A paintable drawn at an exact size. Gtk.Image always reserves a square, which
    /// would leave bars beside a portrait poster, and Gtk.Picture asks for the image's
    /// own pixel size.
    pub struct ExactSizePaintable(ObjectSubclass<exact_size::ExactSizePaintable>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ExactSizePaintable {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ExactSizePaintable {
    pub fn set_paintable(&self, paintable: &impl IsA<gdk::Paintable>, width: f64, height: f64) {
        let inner = self.imp();
        *inner.paintable.borrow_mut() = Some(paintable.clone().upcast());
        inner
            .size
            .set((width.round() as i32, height.round() as i32));
        self.queue_resize();
    }
}

/// The width and height of a poster fitted inside a square, keeping its shape.
fn fitted_poster_size(texture: &gdk::Texture, box_pixels: f64) -> (f64, f64) {
    let aspect = texture.intrinsic_aspect_ratio();
    (
        box_pixels * aspect.min(1.0),
        box_pixels * (1.0 / aspect).min(1.0),
    )
}

/// Fetch a poster off the GUI thread, then pass its texture to on_loaded. A missing
/// poster costs only the picture, so a failure is logged and on_loaded is not called.
fn load_poster(
    found: &Match,
    size: &'static str,
    cache: PathBuf,
    on_loaded: impl FnOnce(gdk::Texture) + 'static,
) {
    let title = found.title.clone();
    let Some(poster_path) = found.poster_path.clone() else {
        return;
    };
    run_in_background(
        move || tmdb::fetch_poster(&poster_path, &cache, size).map_err(|e| e.to_string()),
        move |result| {
            let data = match result {
                Ok(data) => data,
                Err(e) => {
                    log::info!("No poster for {title}: {e}");
                    return;
                }
            };
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)) {
                Ok(texture) => on_loaded(texture),
                Err(e) => log::info!("Could not read the poster for {title}: {}", e.message()),
            }
        },
    );
}

/// Shows a large poster once the pointer has rested on the thumbnail.
///
/// The large image downloads only when the pointer starts to rest, so a search does
/// not fetch a large poster per result. Until it arrives the preview shows the
/// thumbnail's image enlarged.
///
/// The thumbnail's widget is a square wider than a portrait poster, so the pointer
/// position is checked against the drawn image, not the widget. The popover sits
/// beside the thumbnail and never takes focus, so it cannot cover the pointer and
/// flicker, or take keyboard input from the search box.
///
/// The thumbnail is held weakly: its controllers own this struct, so a strong
/// reference would keep every result of every search alive for the app's lifetime.
struct PosterPreview {
    thumbnail: glib::WeakRef<gtk::Image>,
    found: Match,
    cache: PathBuf,
    texture: RefCell<Option<gdk::Texture>>,
    delay_source: RefCell<Option<glib::SourceId>>,
    large_requested: Cell<bool>,
    image: ExactSizePaintable,
    popover: gtk::Popover,
}

impl PosterPreview {
    fn attach(thumbnail: &gtk::Image, found: &Match, cache: PathBuf) -> Rc<Self> {
        let image = ExactSizePaintable::default();
        let popover = gtk::Popover::builder()
            .child(&image)
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Right)
            .can_focus(false)
            .build();
        popover.set_parent(thumbnail);

        let preview = Rc::new(Self {
            thumbnail: thumbnail.downgrade(),
            found: found.clone(),
            cache,
            texture: RefCell::new(None),
            delay_source: RefCell::new(None),
            large_requested: Cell::new(false),
            image,
            popover,
        });

        let motion = gtk::EventControllerMotion::new();
        let on_enter = preview.clone();
        motion.connect_enter(move |_, x, y| on_enter.on_pointer(x, y));
        let on_motion = preview.clone();
        motion.connect_motion(move |_, x, y| on_motion.on_pointer(x, y));
        let on_leave = preview.clone();
        motion.connect_leave(move |_| on_leave.hide());
        thumbnail.add_controller(motion);

        // A new search removes the row under a resting pointer without a "leave".
        let on_unmap = preview.clone();
        thumbnail.connect_unmap(move |_| on_unmap.hide());
        let on_destroy = preview.clone();
        thumbnail.connect_destroy(move |_| on_destroy.popover.unparent());
        preview
    }

    fn set_texture(&self, texture: gdk::Texture) {
        self.show_in_preview(&texture);
        *self.texture.borrow_mut() = Some(texture);
    }

    fn show_in_preview(&self, texture: &gdk::Texture) {
        let (width, height) = fitted_poster_size(texture, POSTER_PREVIEW_PIXELS);
        self.image.set_paintable(texture, width, height);
    }

    fn request_large_poster(self: &Rc<Self>) {
        if self.large_requested.replace(true) {
            return;
        }
        let preview = self.clone();
        load_poster(
            &self.found,
            tmdb::POSTER_SIZE_PREVIEW,
            self.cache.clone(),
            move |texture| {
                preview.show_in_preview(&texture);
            },
        );
    }

    fn pointer_is_on_poster(&self, x: f64, y: f64) -> bool {
        let (Some(texture), Some(thumbnail)) =
            (self.texture.borrow().clone(), self.thumbnail.upgrade())
        else {
            return false;
        };
        let (drawn_width, drawn_height) =
            fitted_poster_size(&texture, f64::from(POSTER_DISPLAY_PIXELS));
        (x - f64::from(thumbnail.width()) / 2.0).abs() <= drawn_width / 2.0
            && (y - f64::from(thumbnail.height()) / 2.0).abs() <= drawn_height / 2.0
    }

    fn on_pointer(self: &Rc<Self>, x: f64, y: f64) {
        if !self.pointer_is_on_poster(x, y) {
            self.hide();
            return;
        }
        if self.delay_source.borrow().is_some() || self.popover.is_visible() {
            return;
        }
        let preview = self.clone();
        let source = glib::timeout_add_local_once(POSTER_PREVIEW_DELAY, move || {
            preview.delay_source.borrow_mut().take();
            preview.popover.popup();
        });
        *self.delay_source.borrow_mut() = Some(source);
        self.request_large_poster();
    }

    fn hide(&self) {
        let source = self.delay_source.borrow_mut().take();
        if let Some(source) = source {
            source.remove();
        }
        self.popover.popdown();
    }
}

const POSTER_PLACEHOLDER_PIXELS: i32 = 32;
const POSTER_PLACEHOLDER_ICON: &str = "video-x-generic-symbolic";

/// The placeholder now, replaced by the poster if one arrives, so results are usable
/// at once and a film with no poster keeps its place in line.
pub fn poster_image(found: &Match, cache: &std::path::Path) -> gtk::Image {
    let image = gtk::Image::builder()
        .icon_name(POSTER_PLACEHOLDER_ICON)
        .pixel_size(POSTER_PLACEHOLDER_PIXELS)
        .width_request(POSTER_DISPLAY_PIXELS)
        .height_request(POSTER_DISPLAY_PIXELS)
        .valign(gtk::Align::Center)
        .build();
    image.update_property(&[gtk::accessible::Property::Label(&format!(
        "Poster for {}",
        found.title
    ))]);
    if found.poster_path.is_none() {
        return image;
    }

    let preview = PosterPreview::attach(&image, found, cache.to_path_buf());
    let shown = image.clone();
    load_poster(
        found,
        tmdb::POSTER_SIZE_THUMBNAIL,
        cache.to_path_buf(),
        move |texture| {
            shown.set_paintable(Some(&texture));
            shown.set_pixel_size(POSTER_DISPLAY_PIXELS);
            preview.set_texture(texture);
        },
    );
    image
}

pub fn ellipsized_label(mode: pango::EllipsizeMode) -> gtk::Label {
    gtk::Label::builder().xalign(0.0).ellipsize(mode).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_megabytes_then_gigabytes() {
        assert_eq!(size_text(5_000_000), "5 MB");
        assert_eq!(size_text(99_000_000), "99 MB");
        assert_eq!(size_text(100_000_000), "0.1 GB");
        assert_eq!(size_text(25_300_000_000), "25.3 GB");
    }

    #[test]
    fn elapsed_time_adds_hours_past_an_hour() {
        assert_eq!(elapsed_text(0.0), "0 min");
        assert_eq!(elapsed_text(3599.0), "59 min");
        assert_eq!(elapsed_text(3600.0), "1 h 0 min");
        assert_eq!(elapsed_text(7500.0), "2 h 5 min");
    }

    #[test]
    fn rate_is_in_megabytes_per_second() {
        assert_eq!(rate_text(12_340_000.0), "12.3 MB/s");
    }
}

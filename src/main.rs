extern crate epoxy;
extern crate gdk;
extern crate glib_itc;
extern crate gtk;
extern crate servo;
extern crate shared_library;

use std::cell::RefCell;
use std::env;
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gdk::{Display, ScrollDirection, POINTER_MOTION_MASK, SCROLL_MASK};
use gdk::enums::key;
use glib_itc::{Sender, channel};
use gtk::{
    ContainerExt,
    Continue,
    GLArea,
    GLAreaExt,
    Inhibit,
    WidgetExt,
    Window,
    WindowExt,
    WindowType,
};
use gtk::Orientation::Vertical;
use servo::BrowserId;
use servo::compositing::compositor_thread::EventLoopWaker;
use servo::compositing::windowing::{WindowEvent, WindowMethods};
use servo::euclid::{Point2D, ScaleFactor, Size2D, TypedPoint2D, TypedRect, TypedSize2D, TypedVector2D};
use servo::gl;
use servo::ipc_channel::ipc;
use servo::msg::constellation_msg::{Key, KeyModifiers};
use servo::net_traits::net_error_list::NetError;
use servo::script_traits::{LoadData, TouchEventType};
use servo::servo_config::opts;
use servo::servo_config::resource_files::set_resources_path;
use servo::servo_geometry::DeviceIndependentPixel;
use servo::servo_url::ServoUrl;
use servo::style_traits::cursor::Cursor;
use servo::style_traits::DevicePixel;
use shared_library::dynamic_library::DynamicLibrary;

fn main() {
    gtk::init().unwrap();

    println!("Servo version: {}", servo::config::servo_version());

    let gtk_window = Window::new(WindowType::Toplevel);
    gtk_window.set_size_request(800, 600);
    gtk_window.add_events((POINTER_MOTION_MASK | SCROLL_MASK).bits() as i32);

    let vbox = gtk::Box::new(Vertical, 0);
    gtk_window.add(&vbox);

    let gl_area = GLArea::new();
    gl_area.set_auto_render(false);
    gl_area.set_has_depth_buffer(true);
    gl_area.add_events((POINTER_MOTION_MASK | SCROLL_MASK).bits() as i32);
    gl_area.set_vexpand(true);
    vbox.add(&gl_area);

    gtk_window.connect_delete_event(|_, _| {
        gtk::main_quit();
        Inhibit(false)
    });

    gtk_window.show_all();

    gl_area.make_current();

    epoxy::load_with(|s| {
        unsafe {
            match DynamicLibrary::open(None).unwrap().symbol(s) {
                Ok(v) => v,
                Err(_) => ptr::null(),
            }
        }
    });
    let gl = unsafe {
        gl::GlFns::load_with(epoxy::get_proc_addr)
    };

    let path = env::current_dir().unwrap().join("resources");
    let path = path.to_str().unwrap().to_string();
    set_resources_path(Some(path));

    let opts = opts::default_opts();
    opts::set_defaults(opts);

    let (tx, mut rx) = channel();

    let waker = Box::new(GtkEventLoopWaker {
        tx: Arc::new(Mutex::new(tx)),
    });

    let window = Rc::new(ServoWindow {
        gl_area: gl_area.clone(),
        gtk_window: gtk_window.clone(),
        waker,
        gl,
    });

    let servo = Rc::new(RefCell::new(servo::Servo::new(window.clone())));

    {
        let servo = servo.clone();
        rx.connect_recv(move || {
            servo.borrow_mut().handle_events(vec![]);
            Continue(true)
        });
    }

    let url = ServoUrl::parse("https://servo.org").unwrap();
    let (sender, receiver) = ipc::channel().unwrap();
    servo.borrow_mut().handle_events(vec![WindowEvent::NewBrowser(url, sender)]);
    let browser_id = receiver.recv().unwrap();
    servo.borrow_mut().handle_events(vec![WindowEvent::SelectBrowser(browser_id)]);

    let pointer = Rc::new(RefCell::new((0.0, 0.0)));
    {
        let pointer = pointer.clone();
        let servo = servo.clone();
        gl_area.connect_motion_notify_event(move |_, event| {
            let (x, y) = event.get_position();
            *pointer.borrow_mut() = (x, y);
            let event = WindowEvent::MouseWindowMoveEventClass(TypedPoint2D::new(x as f32, y as f32));
            servo.borrow_mut().handle_events(vec![event]);
            Inhibit(false)
        });
    }

    {
        let servo = servo.clone();
        let window = window.clone();
        gl_area.connect_resize(move |_, _, _| {
            let event = WindowEvent::Resize(window.framebuffer_size());
            servo.borrow_mut().handle_events(vec![event]);
        });
    }

    {
        let pointer = pointer.clone();
        let servo = servo.clone();
        gtk_window.connect_scroll_event(move |_, event| {
            let (dx, dy) = event.get_delta();
            let dy = dy * -38.0;
            let scroll_location = servo::webrender_api::ScrollLocation::Delta(TypedVector2D::new(dx as f32, dy as f32));
            let phase = match event.get_direction() {
                ScrollDirection::Down => TouchEventType::Down,
                ScrollDirection::Up => TouchEventType::Up,
                ScrollDirection::Left => TouchEventType::Cancel, // FIXME
                ScrollDirection::Right => TouchEventType::Cancel, // FIXME
                ScrollDirection::Smooth | _ =>
                    if dy > 0.0 {
                        TouchEventType::Down
                    } else {
                        TouchEventType::Up
                    },
            };
            let pointer = {
                let pointer = pointer.borrow();
                TypedPoint2D::new(pointer.0 as i32, pointer.1 as i32)
            };
            let event = WindowEvent::Scroll(scroll_location, pointer, phase);
            servo.borrow_mut().handle_events(vec![event]);
            Inhibit(false)
        });
    }

    {
        let servo = servo.clone();
        gtk_window.connect_key_press_event(move |_, event| {
            if event.get_keyval() == key::R {
                let event = WindowEvent::Reload(browser_id);
                servo.borrow_mut().handle_events(vec![event]);
            }
            Inhibit(false)
        });
    }

    gtk::main();
}

pub struct GtkEventLoopWaker {
    tx: Arc<Mutex<Sender>>,
}

impl EventLoopWaker for GtkEventLoopWaker {
    // Use by servo to share the "event loop waker" across threads
    fn clone(&self) -> Box<EventLoopWaker + Send> {
        Box::new(GtkEventLoopWaker {
            tx: self.tx.clone(),
        })
    }
    // Called by servo when the main thread needs to wake up
    fn wake(&self) {
        self.tx.lock().unwrap().send();
    }
}

struct ServoWindow {
    // All these fields will be used in WindowMethods implementations
    gl_area: GLArea,
    gtk_window: Window,
    waker: Box<EventLoopWaker>,
    gl: Rc<gl::Gl>,
}

impl WindowMethods for ServoWindow {
    fn prepare_for_composite(&self, _width: usize, _height: usize) -> bool {
        self.gl_area.make_current();
        true
    }

    fn present(&self) {
        self.gl_area.queue_render();
    }

    fn supports_clipboard(&self) -> bool {
        false
    }

    fn create_event_loop_waker(&self) -> Box<EventLoopWaker> {
        self.waker.clone()
    }

    fn gl(&self) -> Rc<gl::Gl> {
        self.gl.clone()
    }

    fn hidpi_factor(&self) -> ScaleFactor<f32, DeviceIndependentPixel, DevicePixel> {
        ScaleFactor::new(self.gtk_window.get_scale_factor() as f32)
    }

    fn framebuffer_size(&self) -> TypedSize2D<u32, DevicePixel> {
        let (width, height) = self.gtk_window.get_size();
        let scale_factor = self.gtk_window.get_scale_factor() as u32;
        TypedSize2D::new(scale_factor * width as u32, scale_factor * height as u32)
    }

    fn window_rect(&self) -> TypedRect<u32, DevicePixel> {
        TypedRect::new(TypedPoint2D::new(0, 0), self.framebuffer_size())
    }

    fn size(&self) -> TypedSize2D<f32, DeviceIndependentPixel> {
        let (width, height) = self.gtk_window.get_size();
        TypedSize2D::new(width as f32, height as f32)
    }

    fn client_window(&self, _id: BrowserId) -> (Size2D<u32>, Point2D<i32>) {
        let (width, height) = self.gtk_window.get_size();
        let (x, y) = self.gtk_window.get_position();
        (Size2D::new(width as u32, height as u32), Point2D::new(x as i32, y as i32))
    }

    fn set_page_title(&self, _id: BrowserId, title: Option<String>) {
        self.gtk_window.set_title(match title {
            Some(ref title) => title,
            None => "",
        });
    }

    fn allow_navigation(&self, _id: BrowserId, _url: ServoUrl, chan: ipc::IpcSender<bool>) {
        chan.send(true).ok();
    }

    fn set_inner_size(&self, _id: BrowserId, _size: Size2D<u32>) {
    }

    fn set_position(&self, _id: BrowserId, _point: Point2D<i32>) {
    }

    fn set_fullscreen_state(&self, _id: BrowserId, _state: bool) {
    }

    fn status(&self, _id: BrowserId, _status: Option<String>) {
    }

    fn load_start(&self, _id: BrowserId) {
    }

    fn load_end(&self, _id: BrowserId) {
    }

    fn load_error(&self, _id: BrowserId, _: NetError, _url: String) {
    }

    fn head_parsed(&self, _id: BrowserId) {
    }

    fn history_changed(&self, _id: BrowserId, _entries: Vec<LoadData>, _current: usize) {
    }

    fn set_cursor(&self, cursor: Cursor) {
        let cursor_name = match cursor {
            Cursor::Pointer => "pointer",
            _ => "default",
        };
        let display = Display::get_default().unwrap();
        let cursor = gdk::Cursor::new_from_name(&display, cursor_name);
        let window = self.gtk_window.get_window().unwrap();
        gdk::WindowExt::set_cursor(&window, &cursor);
    }

    fn set_favicon(&self, _id: BrowserId, _url: ServoUrl) {
    }

    fn handle_key(&self, _id: Option<BrowserId>, _ch: Option<char>, _key: Key, _mods: KeyModifiers) {
    }
}

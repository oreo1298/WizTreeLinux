//! GTK widget showing a treemap. Rendering happens on a worker thread; the
//! widget only uploads the finished image and draws hover/selection outlines.

use crate::format;
use crate::settings::TreemapStyle;
use crate::tree::{ExtId, NodeId, SizeMode, Tree, ROOT};
use crate::treemap::{self, Params, Rendered};
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

type NodeCallback = Box<dyn Fn(Option<NodeId>)>;
type SelectCallback = Box<dyn Fn(NodeId)>;
type ContextCallback = Box<dyn Fn(NodeId, f64, f64)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Treemap {
        pub tree: RefCell<Option<Arc<Tree>>>,
        pub root: Cell<NodeId>,
        pub colors: RefCell<Arc<Vec<[u8; 3]>>>,
        pub highlight: Cell<Option<ExtId>>,
        pub mode: Cell<SizeMode>,
        pub style: Cell<TreemapStyle>,
        pub rendered: RefCell<Option<Rendered>>,
        pub texture: RefCell<Option<gdk::Texture>>,
        pub generation: Arc<AtomicU64>,
        pub render_source: RefCell<Option<glib::SourceId>>,
        pub hover: Cell<Option<usize>>,
        pub selected: Cell<Option<NodeId>>,
        pub on_hover: RefCell<Option<NodeCallback>>,
        pub on_select: RefCell<Option<SelectCallback>>,
        pub on_context: RefCell<Option<ContextCallback>>,
        pub on_root_changed: RefCell<Option<SelectCallback>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Treemap {
        const NAME: &'static str = "DendroTreemap";
        type Type = super::TreemapWidget;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("treemap");
            klass.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl ObjectImpl for Treemap {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.add_css_class("dendro-treemap");
            obj.set_has_tooltip(true);
            obj.update_property(&[gtk::accessible::Property::Label("Treemap of disk usage")]);

            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(glib::clone!(
                #[weak]
                obj,
                move |_, x, y| obj.imp().set_hover(obj.imp().leaf_index_at(x, y))
            ));
            motion.connect_leave(glib::clone!(
                #[weak]
                obj,
                move |_| obj.imp().set_hover(None)
            ));
            obj.add_controller(motion);

            let click = gtk::GestureClick::new();
            click.set_button(0);
            click.connect_pressed(glib::clone!(
                #[weak]
                obj,
                move |gesture, n_press, x, y| {
                    let imp = obj.imp();
                    match gesture.current_button() {
                        gdk::BUTTON_PRIMARY => {
                            if let Some(node) = imp.node_at(x, y) {
                                if n_press == 2 {
                                    imp.zoom_towards(node);
                                } else if let Some(cb) = &*imp.on_select.borrow() {
                                    cb(node);
                                }
                            }
                        }
                        gdk::BUTTON_SECONDARY => {
                            if let Some(node) = imp.node_at(x, y) {
                                if let Some(cb) = &*imp.on_select.borrow() {
                                    cb(node);
                                }
                                if let Some(cb) = &*imp.on_context.borrow() {
                                    cb(node, x, y);
                                }
                            }
                        }
                        8 => obj.zoom_out(),
                        _ => {}
                    }
                }
            ));
            obj.add_controller(click);

            obj.connect_query_tooltip(|obj, x, y, _keyboard, tooltip| obj.imp().query_tooltip(x, y, tooltip));
        }

        fn dispose(&self) {
            // Make any in-flight render obsolete.
            self.generation.fetch_add(1, Relaxed);
            if let Some(id) = self.render_source.take() {
                id.remove();
            }
        }
    }

    impl WidgetImpl for Treemap {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (60, 400, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            let (w, h) = self.device_size();
            let up_to_date = self.rendered.borrow().as_ref().is_some_and(|r| r.width == w && r.height == h);
            if !up_to_date {
                let delay = if self.rendered.borrow().is_some() { 90 } else { 0 };
                self.schedule_render(delay);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            if let Some(tex) = &*self.texture.borrow() {
                snapshot.append_scaled_texture(tex, gsk::ScalingFilter::Linear, &graphene::Rect::new(0.0, 0.0, w, h));
            }
            let rendered = self.rendered.borrow();
            let Some(r) = rendered.as_ref() else { return };
            let (sx, sy) = (w / r.width as f32, h / r.height as f32);
            let to_widget =
                |rc: [f32; 4]| graphene::Rect::new(rc[0] * sx, rc[1] * sy, (rc[2] - rc[0]) * sx, (rc[3] - rc[1]) * sy);

            if let Some(leaf) = self.hover.get().and_then(|i| r.leaves.get(i)) {
                let rect = to_widget([leaf.x0 as f32, leaf.y0 as f32, leaf.x1 as f32, leaf.y1 as f32]);
                outline(snapshot, &rect, &gdk::RGBA::new(0.0, 0.0, 0.0, 0.45), 1.0);
                outline(snapshot, &rect.inset_r(1.0, 1.0), &gdk::RGBA::new(1.0, 1.0, 1.0, 0.85), 1.0);
            }
            if let Some(rc) = self.selected.get().and_then(|id| r.rect_of(id)) {
                let mut rect = to_widget(rc);
                // Keep tiny selections visible.
                let min = 8.0;
                if rect.width() < min || rect.height() < min {
                    let (cx, cy) = (rect.x() + rect.width() / 2.0, rect.y() + rect.height() / 2.0);
                    let (rw, rh) = (rect.width().max(min), rect.height().max(min));
                    rect = graphene::Rect::new(cx - rw / 2.0, cy - rh / 2.0, rw, rh);
                }
                let accent = obj.color();
                outline(snapshot, &rect.inset_r(-1.0, -1.0), &gdk::RGBA::new(1.0, 1.0, 1.0, 0.9), 1.0);
                outline(snapshot, &rect, &accent, 2.5);
            }
        }
    }

    fn outline(snapshot: &gtk::Snapshot, rect: &graphene::Rect, color: &gdk::RGBA, width: f32) {
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        let rr = gsk::RoundedRect::from_rect(*rect, 0.0);
        snapshot.append_border(&rr, &[width; 4], &[*color; 4]);
    }

    impl Treemap {
        pub fn device_size(&self) -> (u32, u32) {
            let obj = self.obj();
            let scale = obj
                .native()
                .and_then(|n| n.surface())
                .map(|s| s.scale())
                .unwrap_or_else(|| obj.scale_factor() as f64)
                .max(1.0);
            let w = (obj.width() as f64 * scale).round().max(0.0) as u32;
            let h = (obj.height() as f64 * scale).round().max(0.0) as u32;
            (w, h)
        }

        pub fn schedule_render(&self, delay_ms: u64) {
            if let Some(id) = self.render_source.take() {
                id.remove();
            }
            let weak = self.obj().downgrade();
            let id = glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
                if let Some(obj) = weak.upgrade() {
                    let _ = obj.imp().render_source.take();
                    obj.imp().start_render();
                }
            });
            self.render_source.replace(Some(id));
        }

        fn start_render(&self) {
            let Some(tree) = self.tree.borrow().clone() else { return };
            let (w, h) = self.device_size();
            if w == 0 || h == 0 {
                return;
            }
            let generation = self.generation.fetch_add(1, Relaxed) + 1;
            let params = Params {
                root: self.root.get(),
                width: w,
                height: h,
                mode: self.mode.get(),
                colors: self.colors.borrow().clone(),
                highlight: self.highlight.get(),
                style: self.style.get(),
                dark: adw::StyleManager::default().is_dark(),
            };
            let current = self.generation.clone();
            let weak = self.obj().downgrade();
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    let stale = || current.load(Relaxed) != generation;
                    treemap::render(&tree, &params, &stale)
                })
                .await;
                let Some(obj) = weak.upgrade() else { return };
                let imp = obj.imp();
                if imp.generation.load(Relaxed) != generation {
                    return;
                }
                if let Ok(Some(rendered)) = result {
                    imp.install(rendered);
                }
            });
        }

        fn install(&self, mut r: Rendered) {
            let pixels = std::mem::take(&mut r.pixels);
            let bytes = glib::Bytes::from_owned(pixels);
            let texture = gdk::MemoryTexture::new(
                r.width as i32,
                r.height as i32,
                gdk::MemoryFormat::R8g8b8,
                &bytes,
                r.width as usize * 3,
            );
            self.texture.replace(Some(texture.upcast()));
            self.rendered.replace(Some(r));
            self.hover.set(None);
            self.obj().queue_draw();
        }

        fn image_coords(&self, x: f64, y: f64) -> Option<(u32, u32)> {
            let obj = self.obj();
            let r = self.rendered.borrow();
            let r = r.as_ref()?;
            let (w, h) = (obj.width() as f64, obj.height() as f64);
            if w <= 0.0 || h <= 0.0 || x < 0.0 || y < 0.0 {
                return None;
            }
            Some(((x * r.width as f64 / w) as u32, (y * r.height as f64 / h) as u32))
        }

        pub fn leaf_index_at(&self, x: f64, y: f64) -> Option<usize> {
            let (ix, iy) = self.image_coords(x, y)?;
            let r = self.rendered.borrow();
            r.as_ref()?.leaf_index_at(ix, iy)
        }

        pub fn node_at(&self, x: f64, y: f64) -> Option<NodeId> {
            let i = self.leaf_index_at(x, y)?;
            self.rendered.borrow().as_ref()?.leaves.get(i).map(|l| l.node)
        }

        fn set_hover(&self, idx: Option<usize>) {
            if self.hover.get() == idx {
                return;
            }
            self.hover.set(idx);
            self.obj().queue_draw();
            let node = idx.and_then(|i| self.rendered.borrow().as_ref()?.leaves.get(i).map(|l| l.node));
            if let Some(cb) = &*self.on_hover.borrow() {
                cb(node);
            }
        }

        /// Double click: zoom one level towards the clicked item.
        fn zoom_towards(&self, node: NodeId) {
            let Some(tree) = self.tree.borrow().clone() else { return };
            let chain = tree.chain(node);
            let root = self.root.get();
            if let Some(pos) = chain.iter().position(|&n| n == root) {
                if let Some(&next) = chain.get(pos + 1) {
                    if tree.is_dir(next) && tree.has_live_children(next) {
                        self.obj().set_root(next);
                    }
                }
            }
        }

        fn query_tooltip(&self, x: i32, y: i32, tooltip: &gtk::Tooltip) -> bool {
            let Some(tree) = self.tree.borrow().clone() else { return false };
            let Some(i) = self.leaf_index_at(x as f64, y as f64) else { return false };
            let obj = self.obj();
            let rendered = self.rendered.borrow();
            let Some(r) = rendered.as_ref() else { return false };
            let leaf = r.leaves[i];
            let node = leaf.node;
            let name = tree.display_name(node);
            let folder = tree.parent(node).map(|p| tree.path(p).to_string_lossy().into_owned()).unwrap_or_default();
            let size_line = if tree.is_dir(node) {
                format!(
                    "{} · {}",
                    format::size(tree.size(node, self.mode.get())),
                    format::plural(tree.file_count(node), "file", "files")
                )
            } else {
                format!(
                    "{} on disk · {} data",
                    format::size(tree.alloc_size(node)),
                    format::size(tree.logical_size(node))
                )
            };
            tooltip.set_markup(Some(&format!(
                "<b>{}</b>\n{}\n<small>{}</small>",
                glib::markup_escape_text(&name),
                glib::markup_escape_text(&size_line),
                glib::markup_escape_text(&folder)
            )));
            let (w, h) = (obj.width() as f64, obj.height() as f64);
            let (sx, sy) = (w / r.width as f64, h / r.height as f64);
            let x0 = (leaf.x0 as f64 * sx).floor() as i32;
            let y0 = (leaf.y0 as f64 * sy).floor() as i32;
            let x1 = (leaf.x1 as f64 * sx).ceil() as i32;
            let y1 = (leaf.y1 as f64 * sy).ceil() as i32;
            tooltip.set_tip_area(&gdk::Rectangle::new(x0, y0, (x1 - x0).max(1), (y1 - y0).max(1)));
            true
        }
    }
}

glib::wrapper! {
    pub struct TreemapWidget(ObjectSubclass<imp::Treemap>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for TreemapWidget {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl TreemapWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shows a new tree (or nothing), resetting zoom, selection and highlight.
    pub fn set_tree(&self, tree: Option<Arc<Tree>>, colors: Arc<Vec<[u8; 3]>>) {
        let imp = self.imp();
        imp.tree.replace(tree);
        imp.colors.replace(colors);
        imp.root.set(ROOT);
        imp.selected.set(None);
        imp.highlight.set(None);
        imp.hover.set(None);
        imp.rendered.replace(None);
        imp.texture.replace(None);
        imp.generation.fetch_add(1, Relaxed);
        self.queue_draw();
        imp.schedule_render(0);
    }

    pub fn tree(&self) -> Option<Arc<Tree>> {
        self.imp().tree.borrow().clone()
    }

    pub fn set_colors(&self, colors: Arc<Vec<[u8; 3]>>) {
        self.imp().colors.replace(colors);
        self.refresh();
    }

    pub fn root(&self) -> NodeId {
        self.imp().root.get()
    }

    pub fn set_root(&self, id: NodeId) {
        let imp = self.imp();
        if imp.root.get() == id {
            return;
        }
        imp.root.set(id);
        imp.schedule_render(0);
        if let Some(cb) = &*imp.on_root_changed.borrow() {
            cb(id);
        }
    }

    pub fn zoom_out(&self) {
        let parent = self.tree().and_then(|t| t.parent(self.root()));
        if let Some(p) = parent {
            self.set_root(p);
        }
    }

    pub fn set_highlight(&self, ext: Option<ExtId>) {
        if self.imp().highlight.replace(ext) != ext {
            self.refresh();
        }
    }

    pub fn set_selected(&self, node: Option<NodeId>) {
        if self.imp().selected.replace(node) != node {
            self.queue_draw();
        }
    }

    pub fn set_mode(&self, mode: SizeMode) {
        if self.imp().mode.replace(mode) != mode {
            self.refresh();
        }
    }

    pub fn set_style(&self, style: TreemapStyle) {
        if self.imp().style.replace(style) != style {
            self.refresh();
        }
    }

    /// Renders again, e.g. after items were deleted or the theme changed.
    pub fn refresh(&self) {
        self.imp().schedule_render(0);
    }

    pub fn connect_hover(&self, f: impl Fn(Option<NodeId>) + 'static) {
        self.imp().on_hover.replace(Some(Box::new(f)));
    }

    pub fn connect_select(&self, f: impl Fn(NodeId) + 'static) {
        self.imp().on_select.replace(Some(Box::new(f)));
    }

    pub fn connect_context(&self, f: impl Fn(NodeId, f64, f64) + 'static) {
        self.imp().on_context.replace(Some(Box::new(f)));
    }

    pub fn connect_root_changed(&self, f: impl Fn(NodeId) + 'static) {
        self.imp().on_root_changed.replace(Some(Box::new(f)));
    }
}

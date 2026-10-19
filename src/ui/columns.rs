//! Building blocks for the column views: cell factories, sorters, icons.

use super::objects::{node_of, ExtObject, NodeObject};
use crate::tree::{flag, Kind, NodeId, Tree};
use gtk::prelude::*;
use gtk::{cairo, gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

/// Called on right click: (widget, x, y, list position, item).
pub type ContextFn = Rc<dyn Fn(&gtk::Widget, f64, f64, u32, &glib::Object)>;

pub fn list_item(obj: &glib::Object) -> &gtk::ListItem {
    obj.downcast_ref::<gtk::ListItem>().expect("factory item is a GtkListItem")
}

/// Runs `update` whenever the cell shows another item, and whenever the
/// shown object emits "changed".
pub fn bind<T: IsA<glib::Object>>(
    item: &gtk::ListItem,
    extract: impl Fn(&glib::Object) -> Option<T> + 'static,
    update: impl Fn(&T) + 'static,
) {
    let update = Rc::new(update);
    let current: Rc<RefCell<Option<(T, glib::SignalHandlerId)>>> = Rc::default();
    item.connect_item_notify(move |item| {
        if let Some((old, handler)) = current.borrow_mut().take() {
            old.disconnect(handler);
        }
        let Some(obj) = item.item().as_ref().and_then(&extract) else { return };
        update(&obj);
        let u = update.clone();
        let handler = obj.connect_local("changed", false, move |args| {
            if let Ok(o) = args[0].get::<glib::Object>() {
                if let Ok(t) = o.downcast::<T>() {
                    u(&t);
                }
            }
            None
        });
        current.replace(Some((obj, handler)));
    });
}

pub fn bind_node(item: &gtk::ListItem, update: impl Fn(&NodeObject) + 'static) {
    bind(item, node_of, update);
}

pub fn bind_ext(item: &gtk::ListItem, update: impl Fn(&ExtObject) + 'static) {
    bind(item, |o| o.clone().downcast::<ExtObject>().ok(), update);
}

/// Adds a right-click handler to a cell widget.
pub fn add_context(item: &gtk::ListItem, widget: &impl IsA<gtk::Widget>, ctx: &ContextFn) {
    let gesture = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
    let item = item.downgrade();
    let target = widget.upcast_ref::<gtk::Widget>().downgrade();
    let ctx = ctx.clone();
    gesture.connect_pressed(move |g, _, x, y| {
        let (Some(item), Some(target)) = (item.upgrade(), target.upgrade()) else { return };
        let Some(obj) = item.item() else { return };
        g.set_state(gtk::EventSequenceState::Claimed);
        ctx(&target, x, y, item.position(), &obj);
    });
    widget.add_controller(gesture);
}

pub fn label(numeric: bool) -> gtk::Label {
    let l = gtk::Label::builder()
        .xalign(if numeric { 1.0 } else { 0.0 })
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    if numeric {
        l.add_css_class("numeric");
    }
    l
}

/// A column whose cells are a single label computed from a node.
pub fn node_text_column(
    title: &str,
    numeric: bool,
    ctx: &ContextFn,
    text: impl Fn(&Tree, NodeId) -> String + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let text = Rc::new(text);
    let ctx = ctx.clone();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let label = label(numeric);
        item.set_child(Some(&label));
        add_context(item, &label, &ctx);
        let text = text.clone();
        bind_node(item, move |node| label.set_text(&text(node.tree(), node.id())));
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

/// Draws a rounded progress bar in the widget's CSS colour.
fn draw_bar(area: &gtk::DrawingArea, cr: &cairo::Context, w: i32, h: i32, frac: f64) {
    let c = area.color();
    let (w, h) = (w as f64, h as f64);
    let r = h / 2.0;
    let rounded = |cr: &cairo::Context, width: f64| {
        cr.new_sub_path();
        cr.arc(width - r, r, r, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
        cr.arc(r, r, r, std::f64::consts::FRAC_PI_2, 3.0 * std::f64::consts::FRAC_PI_2);
        cr.close_path();
    };
    rounded(cr, w);
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.16);
    let _ = cr.fill();
    if frac > 0.0 {
        rounded(cr, (w * frac.min(1.0)).max(h));
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 1.0);
        let _ = cr.fill();
    }
}

/// A column with a small bar and a percentage label.
pub fn percent_column(
    title: &str,
    ctx: &ContextFn,
    fraction: impl Fn(&Tree, NodeId) -> f64 + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let fraction = Rc::new(fraction);
    let ctx = ctx.clone();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let value = Rc::new(Cell::new(0.0f64));
        let area = gtk::DrawingArea::builder()
            .content_width(30)
            .content_height(8)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        area.add_css_class("dendro-bar");
        let v = value.clone();
        area.set_draw_func(move |a, cr, w, h| draw_bar(a, cr, w, h, v.get()));
        let label = label(true);
        label.set_hexpand(false);
        label.set_width_chars(6);
        hbox.append(&area);
        hbox.append(&label);
        item.set_child(Some(&hbox));
        add_context(item, &hbox, &ctx);
        let fraction = fraction.clone();
        bind_node(item, move |node| {
            let f = fraction(node.tree(), node.id());
            value.set(f);
            area.queue_draw();
            label.set_text(&crate::format::percent(f));
        });
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col.set_fixed_width(124);
    col
}

/// The extension list's percentage column.
pub fn ext_percent_column(title: &str, fraction: impl Fn(&ExtObject) -> f64 + 'static) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let fraction = Rc::new(fraction);
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let value = Rc::new(Cell::new(0.0f64));
        let area = gtk::DrawingArea::builder()
            .content_width(24)
            .content_height(8)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        area.add_css_class("dendro-bar");
        let v = value.clone();
        area.set_draw_func(move |a, cr, w, h| draw_bar(a, cr, w, h, v.get()));
        let label = label(true);
        label.set_hexpand(false);
        label.set_width_chars(6);
        hbox.append(&area);
        hbox.append(&label);
        item.set_child(Some(&hbox));
        let fraction = fraction.clone();
        bind_ext(item, move |ext| {
            let f = fraction(ext);
            value.set(f);
            area.queue_draw();
            label.set_text(&crate::format::percent(f));
        });
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col.set_fixed_width(106);
    col
}

pub fn ext_text_column(
    title: &str,
    numeric: bool,
    text: impl Fn(&ExtObject) -> String + 'static,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let text = Rc::new(text);
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let label = label(numeric);
        item.set_child(Some(&label));
        let text = text.clone();
        bind_ext(item, move |ext| label.set_text(&text(ext)));
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col
}

/// Extension name with its treemap colour.
pub fn ext_name_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let color = Rc::new(Cell::new([0u8; 3]));
        let swatch =
            gtk::DrawingArea::builder().content_width(14).content_height(14).valign(gtk::Align::Center).build();
        let c = color.clone();
        swatch.set_draw_func(move |_, cr, w, h| {
            let [r, g, b] = c.get();
            let (w, h) = (w as f64, h as f64);
            let rad = 3.5;
            cr.new_sub_path();
            cr.arc(w - rad, rad, rad, -std::f64::consts::FRAC_PI_2, 0.0);
            cr.arc(w - rad, h - rad, rad, 0.0, std::f64::consts::FRAC_PI_2);
            cr.arc(rad, h - rad, rad, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
            cr.arc(rad, rad, rad, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
            cr.close_path();
            cr.set_source_rgb(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
            let _ = cr.fill();
        });
        let label = label(false);
        hbox.append(&swatch);
        hbox.append(&label);
        item.set_child(Some(&hbox));
        bind_ext(item, move |ext| {
            color.set(ext.color());
            swatch.queue_draw();
            let name = ext.name();
            if name.is_empty() {
                label.set_text("(none)");
                label.add_css_class("dim-label");
            } else {
                label.set_text(&format!(".{name}"));
                label.remove_css_class("dim-label");
            }
        });
    });
    let col = gtk::ColumnViewColumn::new(Some("Type"), Some(factory));
    col.set_resizable(true);
    col.set_fixed_width(100);
    col
}

thread_local! {
    static ICONS: RefCell<HashMap<String, gio::Icon>> = RefCell::new(HashMap::new());
}

fn themed(name: &str) -> gio::Icon {
    ICONS.with(|icons| {
        icons.borrow_mut().entry(format!("icon:{name}")).or_insert_with(|| gio::ThemedIcon::new(name).upcast()).clone()
    })
}

fn icon_for_content_type(ctype: &str) -> gio::Icon {
    ICONS.with(|icons| {
        icons
            .borrow_mut()
            .entry(format!("type:{ctype}"))
            .or_insert_with(|| gio::content_type_get_symbolic_icon(ctype))
            .clone()
    })
}

pub fn icon_for(tree: &Tree, id: NodeId) -> gio::Icon {
    let flags = tree.flags(id);
    match tree.kind(id) {
        Kind::Dir if flags & flag::MOUNT != 0 => themed("drive-harddisk-symbolic"),
        Kind::Dir if flags & flag::ERROR != 0 => themed("dialog-warning-symbolic"),
        Kind::Dir => themed("folder-symbolic"),
        Kind::Symlink => icon_for_content_type("inode/symlink"),
        Kind::Other => themed("text-x-generic-symbolic"),
        Kind::File => {
            let ext = tree.ext_name(tree.ext(id));
            let key = if ext.is_empty() { tree.display_name(id) } else { format!("x.{ext}") };
            let (ctype, _) = gio::content_type_guess(Some(key.as_str()), None);
            icon_for_content_type(&ctype)
        }
    }
}

/// Tooltip explaining special entries.
pub fn note_for(tree: &Tree, id: NodeId) -> Option<&'static str> {
    let flags = tree.flags(id);
    if flags & flag::VIRTUAL != 0 {
        Some("Virtual filesystem (not scanned)")
    } else if flags & flag::MOUNT != 0 {
        Some("Another filesystem is mounted here (not scanned)")
    } else if flags & flag::ERROR != 0 {
        Some("Could not be read completely")
    } else if flags & flag::HARDLINK != 0 {
        Some("Hard link to a file that was already counted")
    } else {
        None
    }
}

/// Name cell: icon + label, with a tree expander when `tree` is true.
pub fn name_column(title: &str, tree_mode: bool, ctx: &ContextFn) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let ctx = ctx.clone();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::new();
        let label = label(false);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        hbox.append(&icon);
        hbox.append(&label);
        if tree_mode {
            let expander = gtk::TreeExpander::new();
            expander.set_indent_for_icon(true);
            expander.set_child(Some(&hbox));
            item.set_child(Some(&expander));
            item.connect_item_notify(move |item| {
                expander.set_list_row(item.item().and_downcast_ref::<gtk::TreeListRow>());
            });
        } else {
            item.set_child(Some(&hbox));
        }
        add_context(item, &hbox, &ctx);
        bind_node(item, move |node| {
            let tree = node.tree();
            let id = node.id();
            let name = if id == crate::tree::ROOT {
                tree.root_path.to_string_lossy().into_owned()
            } else {
                tree.display_name(id)
            };
            if tree.name(id).first() == Some(&b'.') && id != crate::tree::ROOT {
                label.add_css_class("dim-label");
            } else {
                label.remove_css_class("dim-label");
            }
            label.set_text(&name);
            icon.set_from_gicon(&icon_for(tree, id));
            let tip = match note_for(tree, id) {
                Some(note) => format!("{name}\n{note}"),
                None => name,
            };
            hbox.set_tooltip_text(Some(&tip));
        });
    });
    let col = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    col.set_resizable(true);
    col.set_expand(true);
    col
}

pub fn node_sorter(cmp: impl Fn(&Tree, NodeId, NodeId) -> Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| match (node_of(a), node_of(b)) {
        (Some(a), Some(b)) => cmp(a.tree(), a.id(), b.id()).into(),
        _ => gtk::Ordering::Equal,
    })
}

pub fn ext_sorter(cmp: impl Fn(&ExtObject, &ExtObject) -> Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| match (a.downcast_ref::<ExtObject>(), b.downcast_ref::<ExtObject>()) {
        (Some(a), Some(b)) => cmp(a, b).into(),
        _ => gtk::Ordering::Equal,
    })
}

pub fn compare_names(tree: &Tree, a: NodeId, b: NodeId) -> Ordering {
    let (x, y) = (tree.name(a), tree.name(b));
    x.iter().map(u8::to_ascii_lowercase).cmp(y.iter().map(u8::to_ascii_lowercase)).then_with(|| x.cmp(y))
}

pub fn format_time(t: i64) -> String {
    if t <= 0 {
        return String::new();
    }
    glib::DateTime::from_unix_local(t)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

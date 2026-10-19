//! GObject wrappers that let GTK list models hold tree nodes and
//! extension statistics.

use crate::tree::{ExtStat, NodeId, Tree};
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use std::sync::Arc;

mod imp {
    use super::*;
    use gtk::glib::subclass::Signal;
    use std::cell::{Cell, OnceCell, RefCell};
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct NodeObject {
        pub tree: OnceCell<Arc<Tree>>,
        pub id: Cell<NodeId>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NodeObject {
        const NAME: &'static str = "DendroNodeObject";
        type Type = super::NodeObject;
    }

    impl ObjectImpl for NodeObject {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("changed").build()])
        }
    }

    #[derive(Default)]
    pub struct ExtObject {
        pub stat: Cell<ExtStat>,
        pub name: RefCell<String>,
        pub color: Cell<[u8; 3]>,
        pub description: OnceCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ExtObject {
        const NAME: &'static str = "DendroExtObject";
        type Type = super::ExtObject;
    }

    impl ObjectImpl for ExtObject {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| vec![Signal::builder("changed").build()])
        }
    }
}

glib::wrapper! {
    /// One node of a scanned [`Tree`].
    pub struct NodeObject(ObjectSubclass<imp::NodeObject>);
}

impl NodeObject {
    pub fn new(tree: &Arc<Tree>, id: NodeId) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj.imp().tree.set(tree.clone());
        obj.imp().id.set(id);
        obj
    }

    pub fn id(&self) -> NodeId {
        self.imp().id.get()
    }

    pub fn tree(&self) -> &Arc<Tree> {
        self.imp().tree.get().expect("node object without tree")
    }

    /// Tells bound widgets that the node's values changed.
    pub fn emit_changed(&self) {
        self.emit_by_name::<()>("changed", &[]);
    }
}

glib::wrapper! {
    /// Statistics of one file extension.
    pub struct ExtObject(ObjectSubclass<imp::ExtObject>);
}

impl ExtObject {
    pub fn new(stat: ExtStat, name: &str, color: [u8; 3]) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().stat.set(stat);
        obj.imp().name.replace(name.to_string());
        obj.imp().color.set(color);
        obj
    }

    pub fn stat(&self) -> ExtStat {
        self.imp().stat.get()
    }

    pub fn set_stat(&self, stat: ExtStat) {
        self.imp().stat.set(stat);
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Extension without the dot; empty for files without one.
    pub fn name(&self) -> String {
        self.imp().name.borrow().clone()
    }

    pub fn color(&self) -> [u8; 3] {
        self.imp().color.get()
    }

    /// Human readable file type, e.g. "Matroska video".
    pub fn description(&self) -> String {
        self.imp()
            .description
            .get_or_init(|| {
                let name = self.name();
                if name.is_empty() {
                    return "Files without extension".to_string();
                }
                let (ctype, uncertain) = gtk::gio::content_type_guess(Some(format!("file.{name}")), None);
                if uncertain && ctype == "application/octet-stream" {
                    format!("{} file", name.to_uppercase())
                } else {
                    gtk::gio::content_type_get_description(&ctype).to_string()
                }
            })
            .clone()
    }
}

/// Extracts the node behind a list item (plain or wrapped in a tree row).
pub fn node_of(item: &glib::Object) -> Option<NodeObject> {
    if let Some(row) = item.downcast_ref::<gtk::TreeListRow>() {
        return row.item().and_downcast::<NodeObject>();
    }
    item.clone().downcast::<NodeObject>().ok()
}

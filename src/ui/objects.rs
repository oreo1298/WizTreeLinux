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

mod child_list_imp {
    use super::*;
    use gtk::gio;
    use gtk::gio::subclass::prelude::*;
    use std::cell::{Cell, OnceCell, RefCell};

    /// Child node ids and the row objects created for them so far.
    pub type Items = (Vec<NodeId>, Vec<Option<NodeObject>>);

    #[derive(Default)]
    pub struct ChildList {
        pub tree: OnceCell<Arc<Tree>>,
        pub parent: Cell<NodeId>,
        /// Live children and their objects, filled on first use.
        pub items: RefCell<Option<Items>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ChildList {
        const NAME: &'static str = "DendroChildList";
        type Type = super::ChildList;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for ChildList {}

    impl ChildList {
        pub fn with_items<R>(&self, f: impl FnOnce(&mut Vec<NodeId>, &mut Vec<Option<NodeObject>>) -> R) -> R {
            let mut items = self.items.borrow_mut();
            let (ids, objs) = items.get_or_insert_with(|| {
                let tree = self.tree.get().expect("child list without tree");
                let ids: Vec<NodeId> = tree.live_children(self.parent.get()).collect();
                let objs = vec![None; ids.len()];
                (ids, objs)
            });
            f(ids, objs)
        }
    }

    impl ListModelImpl for ChildList {
        fn item_type(&self) -> glib::Type {
            NodeObject::static_type()
        }

        fn n_items(&self) -> u32 {
            self.with_items(|ids, _| ids.len() as u32)
        }

        fn item(&self, position: u32) -> Option<glib::Object> {
            let tree = self.tree.get()?.clone();
            self.with_items(|ids, objs| {
                let id = *ids.get(position as usize)?;
                let obj = objs[position as usize].get_or_insert_with(|| NodeObject::new(&tree, id));
                Some(obj.clone().upcast())
            })
        }
    }
}

glib::wrapper! {
    /// The children of one folder, as a list model that creates row
    /// objects only when GTK asks for them. GTK creates these models just
    /// to find out whether a row can be expanded, so they must be cheap.
    pub struct ChildList(ObjectSubclass<child_list_imp::ChildList>)
        @implements gtk::gio::ListModel;
}

impl ChildList {
    pub fn new(tree: &Arc<Tree>, parent: NodeId) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj.imp().tree.set(tree.clone());
        obj.imp().parent.set(parent);
        obj
    }

    /// Drops a deleted child from the list.
    pub fn remove_node(&self, node: NodeId) {
        let pos = self.imp().with_items(|ids, objs| {
            let pos = ids.iter().position(|&id| id == node)?;
            ids.remove(pos);
            objs.remove(pos);
            Some(pos)
        });
        if let Some(pos) = pos {
            self.items_changed(pos as u32, 1, 0);
        }
    }

    /// Row objects that have been handed to GTK so far.
    pub fn created_objects(&self) -> Vec<NodeObject> {
        match &*self.imp().items.borrow() {
            Some((_, objs)) => objs.iter().flatten().cloned().collect(),
            None => Vec::new(),
        }
    }
}

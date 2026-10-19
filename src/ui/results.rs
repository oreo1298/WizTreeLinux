//! The results page: folder tree, largest files, file types and treemap.

use super::columns::{self, ContextFn};
use super::objects::{node_of, ChildList, ExtObject, NodeObject};
use super::start_page::usage_bar;
use super::treemap_widget::TreemapWidget;
use crate::format;
use crate::helper;
use crate::mounts;
use crate::settings;
use crate::tree::{flag, ExtStat, NodeId, Query, SizeMode, Tree, ROOT};
use crate::treemap;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;

/// How many files the "Largest Files" view lists.
const FILES_LIMIT: usize = 5000;

type RescanFn = Rc<dyn Fn(bool)>;

#[derive(Default)]
struct State {
    tree: Option<Arc<Tree>>,
    mode: SizeMode,
    colors: Arc<Vec<[u8; 3]>>,
    /// The folder tree's top row.
    root_obj: Option<NodeObject>,
    /// Child models created by the tree view, per directory.
    stores: HashMap<NodeId, glib::WeakRef<ChildList>>,
    /// Node of the current tree that the item actions apply to.
    context: Option<NodeId>,
    hovered: Option<NodeId>,
    selected: Option<NodeId>,
    files_generation: u64,
    stats_generation: u64,
    usage_generation: u64,
}

pub struct ResultsView {
    pub root: adw::ToolbarView,
    window: glib::WeakRef<adw::ApplicationWindow>,
    toasts: adw::ToastOverlay,
    view_stack: adw::ViewStack,
    actions: gio::SimpleActionGroup,

    sum_title: gtk::Label,
    sum_details: gtk::Label,
    sum_total: gtk::Label,
    sum_usage: gtk::Label,
    sum_bar: gtk::LevelBar,
    banner: adw::Banner,

    tree_view: gtk::ColumnView,
    tree_root: gio::ListStore,
    tree_model: gtk::TreeListModel,
    tree_sort: gtk::SortListModel,
    tree_sel: gtk::SingleSelection,
    tree_size_cols: [gtk::ColumnViewColumn; 2],

    files_view: gtk::ColumnView,
    files_store: gio::ListStore,
    files_sel: gtk::SingleSelection,
    files_search: gtk::SearchEntry,
    files_info: gtk::Label,
    files_size_cols: [gtk::ColumnViewColumn; 2],

    ext_view: gtk::ColumnView,
    ext_store: gio::ListStore,
    ext_sel: gtk::SingleSelection,
    ext_clear: gtk::Button,
    ext_size_col: gtk::ColumnViewColumn,

    treemap: TreemapWidget,
    map_title: gtk::Label,
    map_up: gtk::Button,
    map_top: gtk::Button,
    status: gtk::Label,

    paned_v: gtk::Paned,
    paned_h: gtk::Paned,
    panes_ready: Cell<(bool, bool)>,

    state: RefCell<State>,
    on_rescan: RefCell<Option<RescanFn>>,
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder().child(child).vexpand(true).hexpand(true).build()
}

fn mode_size(tree: &Tree, id: NodeId) -> u64 {
    tree.size(id, settings::get().size_mode)
}

impl ResultsView {
    pub fn new(window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay, menu: &gio::MenuModel) -> Rc<Self> {
        Rc::new_cyclic(|weak: &Weak<ResultsView>| Self::build(weak.clone(), window, toasts, menu))
    }

    fn build(
        weak: Weak<Self>,
        window: &adw::ApplicationWindow,
        toasts: &adw::ToastOverlay,
        menu: &gio::MenuModel,
    ) -> Self {
        let ctx: ContextFn = {
            let weak = weak.clone();
            Rc::new(move |widget, x, y, pos, item| {
                let Some(this) = weak.upgrade() else { return };
                let Some(node) = node_of(item).and_then(|n| this.current_id(&n)) else { return };
                // Select the row that was right-clicked.
                if item.is::<gtk::TreeListRow>() {
                    this.tree_sel.set_selected(pos);
                } else {
                    this.files_sel.set_selected(pos);
                }
                this.show_context_menu(widget, x, y, node);
            })
        };

        // ── Header ──────────────────────────────────────────────────────
        let view_stack = adw::ViewStack::new();
        let header = adw::HeaderBar::new();
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back to Locations")
            .action_name("win.home")
            .build();
        let rescan = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Scan Again")
            .action_name("win.rescan")
            .build();
        header.pack_start(&back);
        header.pack_start(&rescan);
        let switcher = adw::ViewSwitcher::builder().stack(&view_stack).policy(adw::ViewSwitcherPolicy::Wide).build();
        header.set_title_widget(Some(&switcher));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(menu)
            .tooltip_text("Main Menu")
            .primary(true)
            .build();
        header.pack_end(&menu_button);
        let search_button = gtk::Button::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text("Search Files")
            .action_name("win.search")
            .build();
        header.pack_end(&search_button);

        // ── Summary strip ───────────────────────────────────────────────
        let summary = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        summary.add_css_class("dendro-summary");
        let folder_icon = gtk::Image::from_icon_name("folder-symbolic");
        folder_icon.set_icon_size(gtk::IconSize::Large);
        let left = gtk::Box::new(gtk::Orientation::Vertical, 2);
        left.set_hexpand(true);
        left.set_valign(gtk::Align::Center);
        let sum_title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        sum_title.add_css_class("heading");
        let sum_details = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        sum_details.add_css_class("dim-label");
        sum_details.add_css_class("caption");
        left.append(&sum_title);
        left.append(&sum_details);
        let right = gtk::Box::new(gtk::Orientation::Vertical, 4);
        right.set_valign(gtk::Align::Center);
        let sum_total = gtk::Label::builder().xalign(1.0).build();
        sum_total.add_css_class("heading");
        sum_total.add_css_class("numeric");
        let usage_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let sum_bar = usage_bar(0.0);
        sum_bar.set_size_request(140, -1);
        sum_bar.set_valign(gtk::Align::Center);
        let sum_usage = gtk::Label::builder().xalign(1.0).build();
        sum_usage.add_css_class("dim-label");
        sum_usage.add_css_class("caption");
        sum_usage.add_css_class("numeric");
        usage_row.append(&sum_usage);
        usage_row.append(&sum_bar);
        usage_row.set_halign(gtk::Align::End);
        right.append(&sum_total);
        right.append(&usage_row);
        summary.append(&folder_icon);
        summary.append(&left);
        summary.append(&right);

        let banner = adw::Banner::new("");
        banner.set_button_label(Some("Details"));

        // ── Folder tree ─────────────────────────────────────────────────
        let tree_root = gio::ListStore::new::<NodeObject>();
        let create_weak = weak.clone();
        let tree_model = gtk::TreeListModel::new(tree_root.clone(), false, false, move |item| {
            let node = item.downcast_ref::<NodeObject>()?;
            create_weak.upgrade()?.child_model(node)
        });
        let tree_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        tree_view.add_css_class("data-table");
        tree_view.set_reorderable(false);
        let name_col = columns::name_column("Name", true, &ctx);
        name_col.set_fixed_width(276);
        name_col.set_sorter(Some(&columns::node_sorter(columns::compare_names)));
        let pct_col = columns::percent_column("% of Parent", &ctx, |tree, id| {
            let parent = tree.parent(id).map(|p| mode_size(tree, p)).unwrap_or(0);
            if id == ROOT {
                1.0
            } else if parent == 0 {
                0.0
            } else {
                tree.counted_size(id, settings::get().size_mode) as f64 / parent as f64
            }
        });
        pct_col.set_sorter(Some(&columns::node_sorter(|t, a, b| mode_size(t, a).cmp(&mode_size(t, b)))));
        let alloc_col = columns::node_text_column("On Disk", true, &ctx, |t, id| format::size(t.alloc_size(id)));
        alloc_col.set_sorter(Some(&columns::node_sorter(|t, a, b| t.alloc_size(a).cmp(&t.alloc_size(b)))));
        alloc_col.set_fixed_width(92);
        let size_col = columns::node_text_column("Size", true, &ctx, |t, id| format::size(t.logical_size(id)));
        size_col.set_sorter(Some(&columns::node_sorter(|t, a, b| t.logical_size(a).cmp(&t.logical_size(b)))));
        size_col.set_fixed_width(92);
        let files_col = columns::node_text_column("Files", true, &ctx, |t, id| {
            if t.is_dir(id) {
                format::count(t.file_count(id))
            } else {
                String::new()
            }
        });
        files_col.set_sorter(Some(&columns::node_sorter(|t, a, b| t.file_count(a).cmp(&t.file_count(b)))));
        files_col.set_fixed_width(80);
        let dirs_col = columns::node_text_column("Folders", true, &ctx, |t, id| {
            if t.is_dir(id) {
                format::count(t.dir_count(id))
            } else {
                String::new()
            }
        });
        dirs_col.set_sorter(Some(&columns::node_sorter(|t, a, b| t.dir_count(a).cmp(&t.dir_count(b)))));
        dirs_col.set_fixed_width(72);
        let mtime_col = columns::node_text_column("Modified", true, &ctx, |t, id| columns::format_time(t.mtime(id)));
        mtime_col.set_sorter(Some(&columns::node_sorter(|t, a, b| t.mtime(a).cmp(&t.mtime(b)))));
        mtime_col.set_fixed_width(146);
        for c in [&name_col, &pct_col, &alloc_col, &size_col, &files_col, &dirs_col, &mtime_col] {
            tree_view.append_column(c);
        }
        let tree_sorter = gtk::TreeListRowSorter::new(tree_view.sorter());
        let tree_sort = gtk::SortListModel::new(Some(tree_model.clone()), Some(tree_sorter));
        let tree_sel = gtk::SingleSelection::new(Some(tree_sort.clone()));
        tree_sel.set_autoselect(false);
        tree_sel.set_can_unselect(true);
        tree_view.set_model(Some(&tree_sel));

        // ── Largest files ───────────────────────────────────────────────
        let files_store = gio::ListStore::new::<NodeObject>();
        let files_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        files_view.add_css_class("data-table");
        let f_name = columns::name_column("Name", false, &ctx);
        f_name.set_fixed_width(260);
        f_name.set_sorter(Some(&columns::node_sorter(columns::compare_names)));
        let f_folder = columns::node_text_column("Folder", false, &ctx, |t, id| {
            t.parent(id).map(|p| t.path(p).to_string_lossy().into_owned()).unwrap_or_default()
        });
        f_folder.set_expand(true);
        f_folder.set_fixed_width(240);
        f_folder.set_sorter(Some(&columns::node_sorter(|t, a, b| {
            let (pa, pb) = (t.parent(a).unwrap_or(ROOT), t.parent(b).unwrap_or(ROOT));
            t.path(pa).cmp(&t.path(pb))
        })));
        let f_alloc = columns::node_text_column("On Disk", true, &ctx, |t, id| format::size(t.alloc_size(id)));
        f_alloc.set_sorter(Some(&columns::node_sorter(|t, a, b| t.alloc_size(a).cmp(&t.alloc_size(b)))));
        f_alloc.set_fixed_width(92);
        let f_size = columns::node_text_column("Size", true, &ctx, |t, id| format::size(t.logical_size(id)));
        f_size.set_sorter(Some(&columns::node_sorter(|t, a, b| t.logical_size(a).cmp(&t.logical_size(b)))));
        f_size.set_fixed_width(92);
        let f_mtime = columns::node_text_column("Modified", true, &ctx, |t, id| columns::format_time(t.mtime(id)));
        f_mtime.set_sorter(Some(&columns::node_sorter(|t, a, b| t.mtime(a).cmp(&t.mtime(b)))));
        f_mtime.set_fixed_width(150);
        for c in [&f_name, &f_folder, &f_alloc, &f_size, &f_mtime] {
            files_view.append_column(c);
        }
        let files_sort = gtk::SortListModel::new(Some(files_store.clone()), files_view.sorter());
        let files_sel = gtk::SingleSelection::new(Some(files_sort));
        files_sel.set_autoselect(false);
        files_sel.set_can_unselect(true);
        files_view.set_model(Some(&files_sel));

        let files_search = gtk::SearchEntry::builder()
            .placeholder_text("Filter by name, or *.ext for a file type")
            .hexpand(true)
            .build();
        let files_info = gtk::Label::new(None);
        files_info.add_css_class("dim-label");
        files_info.add_css_class("caption");
        files_info.add_css_class("numeric");
        let search_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        search_bar.add_css_class("dendro-searchbar");
        search_bar.append(&files_search);
        search_bar.append(&files_info);
        let files_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        files_page.append(&search_bar);
        files_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        files_page.append(&scrolled(&files_view));

        view_stack.add_titled_with_icon(&scrolled(&tree_view), Some("folders"), "Folders", "folder-symbolic");
        view_stack.add_titled_with_icon(&files_page, Some("files"), "Largest Files", "view-list-symbolic");

        // ── File types ──────────────────────────────────────────────────
        let ext_store = gio::ListStore::new::<ExtObject>();
        let ext_view = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        ext_view.add_css_class("data-table");
        let e_name = columns::ext_name_column();
        e_name.set_sorter(Some(&columns::ext_sorter(|a, b| a.name().cmp(&b.name()))));
        let ext_weak = weak.clone();
        let total_of = move || ext_weak.upgrade().map(|t| t.ext_total()).unwrap_or(0);
        let e_pct = columns::ext_percent_column("%", move |e| {
            let total = total_of();
            if total == 0 {
                0.0
            } else {
                e.stat().get(settings::get().size_mode) as f64 / total as f64
            }
        });
        let e_size = columns::ext_text_column("Size", true, |e| format::size(e.stat().get(settings::get().size_mode)));
        e_size.set_fixed_width(84);
        e_size.set_sorter(Some(&columns::ext_sorter(|a, b| {
            let m = settings::get().size_mode;
            a.stat().get(m).cmp(&b.stat().get(m))
        })));
        e_pct.set_sorter(e_size.sorter().as_ref());
        let e_count = columns::ext_text_column("Files", true, |e| format::count(e.stat().count));
        e_count.set_sorter(Some(&columns::ext_sorter(|a, b| a.stat().count.cmp(&b.stat().count))));
        e_count.set_fixed_width(72);
        let e_desc = columns::ext_text_column("Description", false, |e| e.description());
        e_desc.set_expand(true);
        e_desc.set_fixed_width(160);
        e_desc.set_sorter(Some(&columns::ext_sorter(|a, b| a.description().cmp(&b.description()))));
        for c in [&e_name, &e_pct, &e_size, &e_count, &e_desc] {
            ext_view.append_column(c);
        }
        let ext_sort = gtk::SortListModel::new(Some(ext_store.clone()), ext_view.sorter());
        let ext_sel = gtk::SingleSelection::new(Some(ext_sort));
        ext_sel.set_autoselect(false);
        ext_sel.set_can_unselect(true);
        ext_view.set_model(Some(&ext_sel));
        ext_view.sort_by_column(Some(&e_size), gtk::SortType::Descending);

        let ext_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        ext_header.add_css_class("dendro-pane-header");
        let ext_title = gtk::Label::builder().label("File Types").xalign(0.0).hexpand(true).build();
        ext_title.add_css_class("heading");
        let ext_clear = gtk::Button::builder()
            .label("Clear Highlight")
            .tooltip_text("Show all file types in the treemap")
            .visible(false)
            .valign(gtk::Align::Center)
            .build();
        ext_clear.add_css_class("flat");
        ext_clear.add_css_class("caption");
        ext_header.append(&ext_title);
        ext_header.append(&ext_clear);
        let types_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        types_box.append(&ext_header);
        types_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        types_box.append(&scrolled(&ext_view));

        // ── Treemap ─────────────────────────────────────────────────────
        let treemap = TreemapWidget::new();
        let map_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        map_header.add_css_class("dendro-pane-header");
        let map_up =
            gtk::Button::builder().icon_name("go-up-symbolic").tooltip_text("Zoom Out").sensitive(false).build();
        map_up.add_css_class("flat");
        let map_top = gtk::Button::builder()
            .icon_name("go-top-symbolic")
            .tooltip_text("Show Everything")
            .sensitive(false)
            .build();
        map_top.add_css_class("flat");
        let map_title =
            gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
        map_title.add_css_class("heading");
        let map_hint = gtk::Label::new(Some("Double-click to zoom in · right-click for actions"));
        map_hint.add_css_class("dim-label");
        map_hint.add_css_class("caption");
        map_header.append(&map_up);
        map_header.append(&map_top);
        map_header.append(&map_title);
        map_header.append(&map_hint);
        let map_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        map_box.append(&map_header);
        map_box.append(&treemap);

        // ── Layout ──────────────────────────────────────────────────────
        let paned_h = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&view_stack)
            .end_child(&types_box)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .resize_end_child(false)
            .build();
        view_stack.set_size_request(320, 120);
        types_box.set_size_request(260, 120);
        let paned_v = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&paned_h)
            .end_child(&map_box)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        map_box.set_size_request(-1, 140);

        let status =
            gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).hexpand(true).build();
        status.add_css_class("caption");
        status.add_css_class("numeric");
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        status_bar.add_css_class("dendro-statusbar");
        status_bar.append(&status);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&banner);
        body.append(&paned_v);
        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.add_top_bar(&summary);
        root.add_bottom_bar(&status_bar);
        root.set_content(Some(&body));
        root.set_top_bar_style(adw::ToolbarStyle::Raised);
        root.set_bottom_bar_style(adw::ToolbarStyle::Raised);

        let actions = gio::SimpleActionGroup::new();
        root.insert_action_group("item", Some(&actions));

        let this = ResultsView {
            root,
            window: window.downgrade(),
            toasts: toasts.clone(),
            view_stack,
            actions,
            sum_title,
            sum_details,
            sum_total,
            sum_usage,
            sum_bar,
            banner,
            tree_view,
            tree_root,
            tree_model,
            tree_sort,
            tree_sel,
            tree_size_cols: [alloc_col, size_col],
            files_view,
            files_store,
            files_sel,
            files_search,
            files_info,
            files_size_cols: [f_alloc, f_size],
            ext_view,
            ext_store,
            ext_sel,
            ext_clear,
            ext_size_col: e_size,
            treemap,
            map_title,
            map_up,
            map_top,
            status,
            paned_v,
            paned_h,
            panes_ready: Cell::new((false, false)),
            state: RefCell::default(),
            on_rescan: RefCell::default(),
        };
        this.connect_signals(weak);
        this
    }

    fn connect_signals(&self, weak: Weak<Self>) {
        macro_rules! with {
            ($weak:expr, |$this:ident| $body:expr) => {{
                let w = $weak.clone();
                move || {
                    if let Some($this) = w.upgrade() {
                        $body;
                    }
                }
            }};
        }

        // Item actions.
        for name in ["open", "show", "copy", "trash", "delete", "zoom"] {
            let action = gio::SimpleAction::new(name, None);
            let w = weak.clone();
            action.connect_activate(move |a, _| {
                if let Some(this) = w.upgrade() {
                    this.run_item_action(&a.name());
                }
            });
            self.actions.add_action(&action);
        }

        // Keyboard shortcuts on the lists.
        for (view, files) in [(&self.tree_view, false), (&self.files_view, true)] {
            let controller = gtk::ShortcutController::new();
            for (accel, action) in [
                ("Delete", "trash"),
                ("KP_Delete", "trash"),
                ("<Shift>Delete", "delete"),
                ("<Control>c", "copy"),
                ("<Control>Return", "open"),
                ("<Control><Shift>Return", "show"),
            ] {
                let w = weak.clone();
                let cb = gtk::CallbackAction::new(move |_, _| {
                    if let Some(this) = w.upgrade() {
                        let node = if files { this.files_selected() } else { this.tree_selected() };
                        if let Some(node) = node {
                            this.state.borrow_mut().context = Some(node);
                            this.update_item_actions();
                            gtk::prelude::ActionGroupExt::activate_action(&this.actions, action, None);
                        }
                    }
                    glib::Propagation::Stop
                });
                controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(cb)));
            }
            view.add_controller(controller);
        }

        // Selection sync.
        let w = weak.clone();
        self.tree_sel.connect_selection_changed(move |sel, _, _| {
            if let Some(this) = w.upgrade() {
                let node = sel.selected_item().and_then(|o| node_of(&o)).and_then(|n| this.current_id(&n));
                this.on_list_selection(node);
            }
        });
        let w = weak.clone();
        self.files_sel.connect_selection_changed(move |sel, _, _| {
            if let Some(this) = w.upgrade() {
                let node = sel.selected_item().and_then(|o| node_of(&o)).and_then(|n| this.current_id(&n));
                this.on_list_selection(node);
            }
        });
        let w = weak.clone();
        self.tree_view.connect_activate(move |_, pos| {
            if let Some(this) = w.upgrade() {
                if let Some(row) = this.tree_sort.item(pos).and_downcast::<gtk::TreeListRow>() {
                    row.set_expanded(!row.is_expanded());
                }
            }
        });
        let w = weak.clone();
        self.files_view.connect_activate(move |_, pos| {
            if let Some(this) = w.upgrade() {
                if let Some(node) = this.files_sel.item(pos).and_then(|o| node_of(&o)).and_then(|n| this.current_id(&n))
                {
                    this.view_stack.set_visible_child_name("folders");
                    this.reveal(node);
                }
            }
        });

        let w = weak.clone();
        self.ext_sel.connect_selection_changed(move |sel, _, _| {
            if let Some(this) = w.upgrade() {
                let ext = sel.selected_item().and_downcast::<ExtObject>().map(|e| e.stat().ext);
                this.treemap.set_highlight(ext);
                this.ext_clear.set_visible(ext.is_some());
            }
        });
        let w = weak.clone();
        self.ext_view.connect_activate(move |_, pos| {
            if let Some(this) = w.upgrade() {
                if let Some(ext) = this.ext_sel.item(pos).and_downcast::<ExtObject>() {
                    let name = ext.name();
                    if !name.is_empty() {
                        this.files_search.set_text(&format!("*.{name}"));
                        this.view_stack.set_visible_child_name("files");
                    }
                }
            }
        });
        self.ext_clear.connect_clicked(glib::clone!(
            #[weak(rename_to = sel)]
            self.ext_sel,
            move |_| sel.set_selected(gtk::INVALID_LIST_POSITION)
        ));

        let w = weak.clone();
        self.files_search.connect_search_changed(move |_| {
            if let Some(this) = w.upgrade() {
                this.run_files_query();
            }
        });

        // Treemap.
        let w = weak.clone();
        self.treemap.connect_hover(move |node| {
            if let Some(this) = w.upgrade() {
                this.state.borrow_mut().hovered = node;
                this.update_status();
            }
        });
        let w = weak.clone();
        self.treemap.connect_select(move |node| {
            if let Some(this) = w.upgrade() {
                this.reveal(node);
            }
        });
        let w = weak.clone();
        self.treemap.connect_context(move |node, x, y| {
            if let Some(this) = w.upgrade() {
                let widget = this.treemap.clone().upcast::<gtk::Widget>();
                this.show_context_menu(&widget, x, y, node);
            }
        });
        let w = weak.clone();
        self.treemap.connect_root_changed(move |_| {
            if let Some(this) = w.upgrade() {
                this.update_map_header();
            }
        });
        self.map_up.connect_clicked(glib::clone!(
            #[weak(rename_to = map)]
            self.treemap,
            move |_| map.zoom_out()
        ));
        self.map_top.connect_clicked(glib::clone!(
            #[weak(rename_to = map)]
            self.treemap,
            move |_| map.set_root(ROOT)
        ));

        let w = weak.clone();
        self.banner.connect_button_clicked(move |_| {
            if let Some(this) = w.upgrade() {
                this.show_errors();
            }
        });

        // Theme changes affect the treemap colours.
        let refresh = with!(weak, |this| this.treemap.refresh());
        adw::StyleManager::default().connect_dark_notify(move |_| refresh());

        // Restore pane positions once the panes know their size.
        let w = weak.clone();
        self.paned_v.connect_max_position_notify(move |p| {
            if let Some(this) = w.upgrade() {
                let (v, h) = this.panes_ready.get();
                if !v && p.height() > 200 {
                    this.panes_ready.set((true, h));
                    p.set_position((p.height() as f64 * settings::get().split_vertical) as i32);
                }
            }
        });
        let w = weak;
        self.paned_h.connect_max_position_notify(move |p| {
            if let Some(this) = w.upgrade() {
                let (v, h) = this.panes_ready.get();
                if !h && p.width() > 400 {
                    this.panes_ready.set((v, true));
                    p.set_position((p.width() as f64 * settings::get().split_horizontal) as i32);
                }
            }
        });
    }

    pub fn connect_rescan(&self, f: impl Fn(bool) + 'static) {
        self.on_rescan.replace(Some(Rc::new(f)));
    }

    pub fn tree(&self) -> Option<Arc<Tree>> {
        self.state.borrow().tree.clone()
    }

    /// Pane sizes as fractions, for saving.
    pub fn pane_fractions(&self) -> Option<(f64, f64)> {
        let (v, h) = self.panes_ready.get();
        if !v || !h {
            return None;
        }
        let fv = self.paned_v.position() as f64 / self.paned_v.height().max(1) as f64;
        let fh = self.paned_h.position() as f64 / self.paned_h.width().max(1) as f64;
        Some((fv.clamp(0.1, 0.9), fh.clamp(0.1, 0.9)))
    }

    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(&glib::markup_escape_text(text));
        toast.set_timeout(4);
        self.toasts.add_toast(toast);
    }

    // ── Showing a tree ──────────────────────────────────────────────────

    pub fn set_tree(self: &Rc<Self>, tree: Arc<Tree>) {
        // Nothing from the previous scan may stay selectable: its node ids
        // mean something else in the new tree.
        self.tree_root.remove_all();
        self.files_store.remove_all();
        let mode = settings::get().size_mode;
        let stats = tree.ext_stats(mode);
        let colors = Arc::new(treemap::ext_colors(&stats, tree.ext_count()));
        let root = NodeObject::new(&tree, ROOT);
        {
            let mut st = self.state.borrow_mut();
            st.tree = Some(tree.clone());
            st.mode = mode;
            st.colors = colors.clone();
            st.root_obj = Some(root.clone());
            st.stores.clear();
            st.context = None;
            st.hovered = None;
            st.selected = None;
        }
        self.treemap.set_mode(mode);
        self.treemap.set_style(settings::get().treemap_style);
        self.treemap.set_tree(Some(tree.clone()), colors.clone());

        self.tree_root.append(&root);
        if let Some(row) = self.tree_model.row(0) {
            row.set_expanded(true);
        }
        self.sort_by_mode();
        self.tree_sel.set_selected(gtk::INVALID_LIST_POSITION);

        self.fill_ext(&tree, &stats, &colors);
        self.files_search.set_text("");
        self.run_files_query();
        self.update_summary();
        self.update_banner();
        self.update_map_header();
        self.update_status();
        self.view_stack.set_visible_child_name("folders");
        if let Some(w) = self.window.upgrade() {
            w.set_title(Some(&format!("{} – {}", tree.root_path.display(), super::APP_NAME)));
        }
    }

    /// Releases the current tree (frees its memory).
    pub fn clear(&self) {
        {
            let mut st = self.state.borrow_mut();
            st.tree = None;
            st.root_obj = None;
            st.stores.clear();
            st.context = None;
        }
        self.tree_root.remove_all();
        self.files_store.remove_all();
        self.ext_store.remove_all();
        self.treemap.set_tree(None, Arc::default());
    }

    fn sort_by_mode(&self) {
        let idx = match settings::get().size_mode {
            SizeMode::Allocated => 0,
            SizeMode::Logical => 1,
        };
        self.tree_view.sort_by_column(Some(&self.tree_size_cols[idx]), gtk::SortType::Descending);
        self.files_view.sort_by_column(Some(&self.files_size_cols[idx]), gtk::SortType::Descending);
        self.ext_view.sort_by_column(Some(&self.ext_size_col), gtk::SortType::Descending);
    }

    /// The node id of `obj` if it belongs to the tree currently shown.
    fn current_id(&self, obj: &NodeObject) -> Option<NodeId> {
        let st = self.state.borrow();
        let current = st.tree.as_ref()?;
        (Arc::ptr_eq(current, obj.tree()) && !current.is_deleted(obj.id())).then(|| obj.id())
    }

    fn is_current(&self, tree: &Arc<Tree>) -> bool {
        self.state.borrow().tree.as_ref().is_some_and(|t| Arc::ptr_eq(t, tree))
    }

    /// Children of a folder row. GTK calls this every time it needs to know
    /// whether a collapsed row can be expanded, so it must stay cheap.
    fn child_model(&self, node: &NodeObject) -> Option<gio::ListModel> {
        let tree = node.tree();
        let id = node.id();
        if !self.is_current(tree) || !tree.is_dir(id) || !tree.has_live_children(id) {
            return None;
        }
        let model = ChildList::new(tree, id);
        self.state.borrow_mut().stores.insert(id, model.downgrade());
        Some(model.upcast())
    }

    fn fill_ext(&self, tree: &Tree, stats: &[ExtStat], colors: &[[u8; 3]]) {
        let objs: Vec<ExtObject> = stats
            .iter()
            .map(|s| {
                ExtObject::new(
                    *s,
                    tree.ext_name(s.ext),
                    colors.get(s.ext as usize).copied().unwrap_or(treemap::OTHER_COLOR),
                )
            })
            .collect();
        self.ext_store.splice(0, self.ext_store.n_items(), &objs);
    }

    fn ext_total(&self) -> u64 {
        let mode = settings::get().size_mode;
        (0..self.ext_store.n_items())
            .filter_map(|i| self.ext_store.item(i).and_downcast::<ExtObject>())
            .map(|e| e.stat().get(mode))
            .sum()
    }

    fn update_summary(self: &Rc<Self>) {
        let Some(tree) = self.tree() else { return };
        let info = &tree.info;
        self.sum_title.set_text(&tree.root_path.to_string_lossy());
        let mut details = format!(
            "{} · {} · scanned in {}",
            format::plural(tree.file_count(ROOT), "file", "files"),
            format::plural(tree.dir_count(ROOT), "folder", "folders"),
            format::duration(info.duration)
        );
        if info.elevated {
            details.push_str(" · as administrator");
        }
        if !info.one_filesystem {
            details.push_str(" · all filesystems");
        }
        self.sum_details.set_text(&details);
        self.sum_total.set_text(&format!(
            "{} on disk · {} of data",
            format::size(tree.alloc_size(ROOT)),
            format::size(tree.logical_size(ROOT))
        ));

        let generation = {
            let mut st = self.state.borrow_mut();
            st.usage_generation += 1;
            st.usage_generation
        };
        let path = tree.root_path.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let usage = gio::spawn_blocking(move || mounts::fs_usage(&path)).await.ok().flatten();
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().usage_generation != generation {
                return;
            }
            match usage {
                Some(u) if u.total > 0 => {
                    let f = u.used as f64 / u.total as f64;
                    this.sum_bar.set_value(f);
                    this.sum_bar.set_visible(true);
                    this.sum_usage.set_text(&format!(
                        "Drive: {} used of {} · {} free",
                        format::size(u.used),
                        format::size(u.total),
                        format::size(u.available)
                    ));
                }
                _ => {
                    this.sum_bar.set_visible(false);
                    this.sum_usage.set_text("");
                }
            }
        });
    }

    fn can_elevate(&self) -> bool {
        !rustix::process::geteuid().is_root() && helper::pkexec_path().is_some()
    }

    fn update_banner(&self) {
        let Some(tree) = self.tree() else { return };
        let n = tree.info.error_count;
        if n == 0 {
            self.banner.set_revealed(false);
            return;
        }
        let what = format::plural(n, "item", "items");
        let hint = if tree.info.elevated || !self.can_elevate() {
            String::new()
        } else {
            " Scan as administrator to include them.".to_string()
        };
        self.banner.set_title(&glib::markup_escape_text(&format!(
            "{what} could not be read, so totals may be too low.{hint}"
        )));
        self.banner.set_revealed(true);
    }

    fn show_errors(&self) {
        let Some(tree) = self.tree() else { return };
        let Some(window) = self.window.upgrade() else { return };
        let mut text = String::new();
        for (path, msg) in &tree.info.errors {
            text.push_str(&format!("{}\n    {msg}\n", path.display()));
        }
        let shown = tree.info.errors.len() as u64;
        if tree.info.error_count > shown {
            text.push_str(&format!("…and {} more\n", format::count(tree.info.error_count - shown)));
        }
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .monospace(true)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        view.buffer().set_text(text.trim_end());
        view.add_css_class("caption");
        view.add_css_class("card");
        let sw = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(120)
            .max_content_height(320)
            .propagate_natural_height(true)
            .build();
        let dialog = adw::AlertDialog::new(
            Some("Unreadable Items"),
            Some("These items could not be read. Their sizes are missing from the totals."),
        );
        dialog.set_extra_child(Some(&sw));
        dialog.add_response("close", "_Close");
        if !tree.info.elevated && self.can_elevate() {
            dialog.add_response("admin", "Scan as _Administrator");
            dialog.set_response_appearance("admin", adw::ResponseAppearance::Suggested);
        }
        dialog.set_close_response("close");
        let rescan = self.on_rescan.borrow().clone();
        dialog.connect_response(None, move |_, response| {
            if response == "admin" {
                if let Some(cb) = &rescan {
                    cb(true);
                }
            }
        });
        dialog.present(Some(&window));
    }

    fn update_map_header(&self) {
        let Some(tree) = self.tree() else { return };
        let root = self.treemap.root();
        self.map_title.set_text(&tree.path(root).to_string_lossy());
        self.map_up.set_sensitive(root != ROOT);
        self.map_top.set_sensitive(root != ROOT);
    }

    fn describe(tree: &Tree, id: NodeId) -> String {
        let path = tree.path(id);
        let mode = settings::get().size_mode;
        let share = {
            let total = tree.size(ROOT, mode);
            if total > 0 {
                format::percent(tree.counted_size(id, mode) as f64 / total as f64)
            } else {
                String::new()
            }
        };
        if tree.is_dir(id) {
            format!(
                "{}  —  {} on disk · {} · {} · {} of total",
                path.display(),
                format::size(tree.alloc_size(id)),
                format::size(tree.logical_size(id)),
                format::plural(tree.file_count(id), "file", "files"),
                share
            )
        } else {
            let modified = columns::format_time(tree.mtime(id));
            format!(
                "{}  —  {} on disk · {} · modified {} · {} of total",
                path.display(),
                format::size(tree.alloc_size(id)),
                format::size(tree.logical_size(id)),
                modified,
                share
            )
        }
    }

    fn update_status(&self) {
        let (tree, node) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.hovered.or(st.selected))
        };
        match (tree, node) {
            (Some(t), Some(id)) if !t.is_deleted(id) => self.status.set_text(&Self::describe(&t, id)),
            (Some(t), _) => self.status.set_text(&Self::describe(&t, ROOT)),
            _ => self.status.set_text(""),
        }
    }

    // ── Selection ───────────────────────────────────────────────────────

    fn tree_selected(&self) -> Option<NodeId> {
        self.tree_sel.selected_item().and_then(|o| node_of(&o)).and_then(|n| self.current_id(&n))
    }

    fn files_selected(&self) -> Option<NodeId> {
        self.files_sel.selected_item().and_then(|o| node_of(&o)).and_then(|n| self.current_id(&n))
    }

    fn on_list_selection(&self, node: Option<NodeId>) {
        self.state.borrow_mut().selected = node;
        self.treemap.set_selected(node);
        self.update_status();
    }

    /// Expands the folder tree down to `node`, selects and shows it.
    pub fn reveal(&self, node: NodeId) {
        let Some(tree) = self.tree() else { return };
        if tree.is_deleted(node) {
            return;
        }
        let chain = tree.chain(node);
        let model = &self.tree_sort;
        let mut pos = 0u32;
        for &next in &chain[1..] {
            let Some(row) = model.item(pos).and_downcast::<gtk::TreeListRow>() else { return };
            row.set_expanded(true);
            let depth = row.depth();
            let n = model.n_items();
            let mut found = None;
            let mut p = pos + 1;
            while p < n {
                let Some(r) = model.item(p).and_downcast::<gtk::TreeListRow>() else { break };
                let d = r.depth();
                if d <= depth {
                    break;
                }
                if d == depth + 1 && r.item().and_downcast::<NodeObject>().is_some_and(|o| o.id() == next) {
                    found = Some(p);
                    break;
                }
                p += 1;
            }
            match found {
                Some(p) => pos = p,
                None => return,
            }
        }
        self.tree_sel.set_selected(pos);
        self.tree_view.scroll_to(pos, None, gtk::ListScrollFlags::NONE, None);
        self.treemap.set_selected(Some(node));
    }

    // ── Largest files ───────────────────────────────────────────────────

    pub fn focus_search(&self) {
        self.view_stack.set_visible_child_name("files");
        self.files_search.grab_focus();
    }

    fn run_files_query(self: &Rc<Self>) {
        let Some(tree) = self.tree() else { return };
        let input = self.files_search.text().to_string();
        let query = Query::parse(&input);
        let mode = settings::get().size_mode;
        let generation = {
            let mut st = self.state.borrow_mut();
            st.files_generation += 1;
            st.files_generation
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let t = tree.clone();
            let result = gio::spawn_blocking(move || t.top_files(&query, mode, FILES_LIMIT)).await;
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().files_generation != generation {
                return;
            }
            let Ok((ids, total)) = result else { return };
            let objs: Vec<NodeObject> = ids.iter().map(|&id| NodeObject::new(&tree, id)).collect();
            this.files_store.splice(0, this.files_store.n_items(), &objs);
            let text = if total == 0 {
                if input.trim().is_empty() {
                    "No files".to_string()
                } else {
                    "No matching files".to_string()
                }
            } else if total as usize > ids.len() {
                format!("Largest {} of {} files", format::count(ids.len() as u64), format::count(total))
            } else {
                format::plural(total, "file", "files")
            };
            this.files_info.set_text(&text);
        });
    }

    // ── Context menu and item actions ───────────────────────────────────

    fn show_context_menu(&self, widget: &gtk::Widget, x: f64, y: f64, node: NodeId) {
        let Some(tree) = self.tree() else { return };
        self.state.borrow_mut().context = Some(node);
        self.update_item_actions();
        let menu = gio::Menu::new();
        let open = gio::Menu::new();
        open.append(Some("_Open"), Some("item.open"));
        open.append(Some("Open _Containing Folder"), Some("item.show"));
        open.append(Some("_Copy Path"), Some("item.copy"));
        menu.append_section(None, &open);
        if tree.is_dir(node) {
            let zoom = gio::Menu::new();
            zoom.append(Some("_Zoom Treemap Here"), Some("item.zoom"));
            menu.append_section(None, &zoom);
        }
        let delete = gio::Menu::new();
        delete.append(Some("Move to _Trash"), Some("item.trash"));
        delete.append(Some("_Delete Permanently…"), Some("item.delete"));
        menu.append_section(None, &delete);

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(widget);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            // Unparent later so the chosen action can still be found.
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn update_item_actions(&self) {
        let (tree, node) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.context)
        };
        let valid = |id: NodeId, t: &Tree| !t.is_deleted(id);
        for name in ["open", "show", "copy", "trash", "delete", "zoom"] {
            let enabled = match (&tree, node) {
                (Some(t), Some(id)) if valid(id, t) => match name {
                    "trash" | "delete" => id != ROOT && !t.has_flag(id, flag::MOUNT | flag::VIRTUAL),
                    "zoom" => t.is_dir(id) && t.has_live_children(id),
                    "show" => id != ROOT || t.root_path.parent().is_some(),
                    _ => true,
                },
                _ => false,
            };
            if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(enabled);
            }
        }
    }

    fn run_item_action(self: &Rc<Self>, name: &str) {
        let (tree, node) = {
            let st = self.state.borrow();
            (st.tree.clone(), st.context)
        };
        let (Some(tree), Some(node)) = (tree, node) else { return };
        if tree.is_deleted(node) {
            return;
        }
        let path = tree.path(node);
        let window = self.window.upgrade();
        match name {
            "open" => {
                let weak = Rc::downgrade(self);
                gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                    window.as_ref(),
                    None::<&gio::Cancellable>,
                    move |res| {
                        if let (Err(e), Some(this)) = (res, weak.upgrade()) {
                            this.toast(&format!("Could not open: {}", e.message()));
                        }
                    },
                );
            }
            "show" => {
                let weak = Rc::downgrade(self);
                gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).open_containing_folder(
                    window.as_ref(),
                    None::<&gio::Cancellable>,
                    move |res| {
                        if let (Err(e), Some(this)) = (res, weak.upgrade()) {
                            this.toast(&format!("Could not open the folder: {}", e.message()));
                        }
                    },
                );
            }
            "copy" => {
                if let Some(w) = &window {
                    w.clipboard().set_text(&path.to_string_lossy());
                    self.toast("Path copied");
                }
            }
            "zoom" => {
                if tree.is_dir(node) {
                    self.treemap.set_root(node);
                }
            }
            "trash" => self.trash(tree, node),
            "delete" => self.confirm_delete(tree, node),
            _ => {}
        }
    }

    /// Refuses to remove anything that has another filesystem mounted on or
    /// inside it; deleting it would wipe that filesystem too.
    fn blocked_by_mount(&self, tree: &Tree, node: NodeId) -> bool {
        let path = tree.path(node);
        let mounts = mounts::read_mounts();
        let Some(m) = mounts::mount_at_or_below(&mounts, &path) else { return false };
        let name = tree.display_name(node);
        self.toast(&format!(
            "“{name}” was left alone because a drive is mounted at {}. Unmount it first.",
            m.mount_point.display()
        ));
        true
    }

    /// " (12 MiB)" for the space an item frees, or a note when other hard
    /// links keep the data.
    fn freed(tree: &Tree, node: NodeId) -> String {
        if !tree.is_dir(node) && tree.has_flag(node, flag::LINKED) {
            ". Other hard links still use its data".to_string()
        } else {
            format!(" ({})", format::size(tree.alloc_size(node)))
        }
    }

    fn trash(self: &Rc<Self>, tree: Arc<Tree>, node: NodeId) {
        if self.blocked_by_mount(&tree, node) {
            return;
        }
        let path = tree.path(node);
        let name = tree.display_name(node);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gio::File::for_path(&path).trash_future(glib::Priority::DEFAULT).await;
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(()) => {
                    let freed = Self::freed(&tree, node);
                    // A new scan may have replaced the tree in the meantime.
                    if this.is_current(&tree) {
                        this.on_deleted(&tree, node);
                    }
                    this.toast(&format!("Moved “{name}” to the trash{freed}"));
                }
                Err(e) if e.matches(gio::IOErrorEnum::NotSupported) => {
                    let toast = adw::Toast::new(&glib::markup_escape_text(&format!(
                        "“{name}” cannot be moved to the trash on this drive"
                    )));
                    toast.set_button_label(Some("Delete…"));
                    let weak = Rc::downgrade(&this);
                    toast.connect_button_clicked(move |_| {
                        if let Some(this) = weak.upgrade() {
                            if this.is_current(&tree) {
                                this.confirm_delete(tree.clone(), node);
                            }
                        }
                    });
                    this.toasts.add_toast(toast);
                }
                Err(e) => this.toast(&format!("Could not move “{name}” to the trash: {}", e.message())),
            }
        });
    }

    fn confirm_delete(self: &Rc<Self>, tree: Arc<Tree>, node: NodeId) {
        let Some(window) = self.window.upgrade() else { return };
        if self.blocked_by_mount(&tree, node) {
            return;
        }
        let name = tree.display_name(node);
        let body = if tree.is_dir(node) {
            format!(
                "The folder and everything in it ({}, {}) will be deleted. This cannot be undone.",
                format::plural(tree.file_count(node), "file", "files"),
                format::size(tree.alloc_size(node))
            )
        } else {
            format!("{} will be freed. This cannot be undone.", format::size(tree.alloc_size(node)))
        };
        let dialog = adw::AlertDialog::new(Some(&format!("Permanently Delete “{name}”?")), Some(&body));
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("delete", "_Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some(this) = weak.upgrade() else { return };
            if this.is_current(&tree) {
                this.delete(tree.clone(), node);
            }
        });
        dialog.present(Some(&window));
    }

    fn delete(self: &Rc<Self>, tree: Arc<Tree>, node: NodeId) {
        let path = tree.path(node);
        let name = tree.display_name(node);
        let is_dir = tree.is_dir(node);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let p = path.clone();
            let result = gio::spawn_blocking(move || crate::remove::remove_all(&p)).await;
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(Ok(())) => {
                    let freed = Self::freed(&tree, node);
                    // A new scan may have replaced the tree in the meantime.
                    if this.is_current(&tree) {
                        this.on_deleted(&tree, node);
                    }
                    this.toast(&format!("Deleted “{name}”{freed}"));
                }
                Ok(Err(e)) => {
                    let partial = is_dir && path.exists();
                    let mut msg = format!("Could not delete “{name}”: {e}");
                    if partial {
                        msg.push_str(". Some items may have been removed; scan again for exact numbers.");
                    }
                    this.toast(&msg);
                }
                Err(_) => this.toast(&format!("Could not delete “{name}”")),
            }
        });
    }

    /// Updates every view after `node` was removed from disk.
    fn on_deleted(self: &Rc<Self>, tree: &Arc<Tree>, node: NodeId) {
        if tree.mark_deleted(node).is_none() {
            return;
        }
        let parent = tree.parent(node);
        let model = parent.and_then(|p| self.state.borrow().stores.get(&p).and_then(|w| w.upgrade()));
        if let Some(model) = model {
            model.remove_node(node);
        }
        // Ancestors changed size, which also changes their children's
        // percentages, so refresh every row that is currently shown.
        self.refresh_rows();
        {
            let mut st = self.state.borrow_mut();
            if st.selected == Some(node) {
                st.selected = None;
            }
            if st.hovered.is_some_and(|h| tree.is_deleted(h)) {
                st.hovered = None;
            }
        }
        // Zoomed into something that no longer exists: go to its parent.
        let mut root = self.treemap.root();
        while tree.is_deleted(root) {
            root = tree.parent(root).unwrap_or(ROOT);
        }
        self.treemap.set_root(root);
        self.treemap.set_selected(self.state.borrow().selected);
        self.treemap.refresh();
        self.run_files_query();
        self.refresh_ext_stats();
        self.update_summary();
        self.update_status();
    }

    /// Re-binds every row of the folder tree.
    fn refresh_rows(&self) {
        let live: Vec<NodeObject> = {
            let mut st = self.state.borrow_mut();
            st.stores.retain(|_, w| w.upgrade().is_some());
            let mut live: Vec<NodeObject> = st.root_obj.iter().cloned().collect();
            for model in st.stores.values().filter_map(|w| w.upgrade()) {
                live.extend(model.created_objects());
            }
            live
        };
        for o in live {
            o.emit_changed();
        }
    }

    fn refresh_ext_stats(self: &Rc<Self>) {
        let Some(tree) = self.tree() else { return };
        let mode = settings::get().size_mode;
        let generation = {
            let mut st = self.state.borrow_mut();
            st.stats_generation += 1;
            st.stats_generation
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let t = tree.clone();
            let Ok(stats) = gio::spawn_blocking(move || t.ext_stats(mode)).await else { return };
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().stats_generation != generation {
                return;
            }
            let by_ext: HashMap<u16, ExtStat> = stats.iter().map(|s| (s.ext, *s)).collect();
            // Update in place so the selection (highlight) survives.
            let mut i = 0;
            while i < this.ext_store.n_items() {
                let Some(obj) = this.ext_store.item(i).and_downcast::<ExtObject>() else { break };
                match by_ext.get(&obj.stat().ext) {
                    Some(s) => {
                        obj.set_stat(*s);
                        i += 1;
                    }
                    None => this.ext_store.remove(i),
                }
            }
        });
    }

    // ── Settings ────────────────────────────────────────────────────────

    /// Applies changed preferences to the current results.
    pub fn apply_settings(self: &Rc<Self>) {
        let s = settings::get();
        self.treemap.set_style(s.treemap_style);
        let Some(tree) = self.tree() else { return };
        let mode_changed = self.state.borrow().mode != s.size_mode;
        if mode_changed {
            self.state.borrow_mut().mode = s.size_mode;
            let stats = tree.ext_stats(s.size_mode);
            let colors = Arc::new(treemap::ext_colors(&stats, tree.ext_count()));
            self.state.borrow_mut().colors = colors.clone();
            self.fill_ext(&tree, &stats, &colors);
            self.treemap.set_mode(s.size_mode);
            self.treemap.set_colors(colors);
            self.sort_by_mode();
            self.run_files_query();
        }
        // Units or mode changed: refresh every visible cell.
        self.refresh_rows();
        for i in 0..self.files_store.n_items() {
            if let Some(o) = self.files_store.item(i).and_downcast::<NodeObject>() {
                o.emit_changed();
            }
        }
        for i in 0..self.ext_store.n_items() {
            if let Some(o) = self.ext_store.item(i).and_downcast::<ExtObject>() {
                o.set_stat(o.stat());
            }
        }
        self.update_summary();
        self.update_status();
    }
}

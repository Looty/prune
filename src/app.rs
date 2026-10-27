//! Main window: sidebar (Overview / History / Settings), Overview = big number,
//! treemap of caches, selectable manager groups, action bar.

use crate::platform::{self, WINDOW_TITLE};
use crate::theme::{self, icon_font, semibold, Icons, Palette};
use devsweep_core::fsutil::human_bytes as hb;
use devsweep_core::settings::{IntervalUnit, Settings, State, Theme};
use devsweep_core::{clean, providers, scan, timefmt, Kind, Package, ProviderReport, Target};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin, Pos2, Rect, Response, RichText, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// `--screenshot`: render after the scan, save a PNG, exit (dev tooling).
pub struct Shot {
    pub path: String,
    /// Comma-separated presets: settings, history, dark, light, select, confirm, or a provider id to expand.
    pub state: String,
}

pub fn run(shot: Option<Shot>, page: Option<String>) {
    // Screenshot runs are dev tooling: they may run next to an open window.
    let instance = if shot.is_some() { Some(None) } else { platform::single_instance(platform::GUI_MUTEX).map(Some) };
    let Some(_instance) = instance else {
        if let Some(p) = &page {
            platform::request_page(p);
        }
        platform::focus_window();
        return;
    };
    let settings = Settings::load();
    if shot.is_none() {
        platform::migrate_autostart();
    }
    if shot.is_none() && (settings.run_in_tray || settings.auto_clean) {
        platform::ensure_tray();
    }
    let icon = egui::IconData { rgba: crate::icon::rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([920.0, 600.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    let _ = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc, settings);
            app.shot = shot;
            if let Some(p) = page {
                app.open_page(&p);
            }
            Ok(Box::new(app))
        }),
    );
}

enum Msg {
    Report(ProviderReport),
    ProviderDone(&'static str),
    ScanFinished(u128, bool),
    CleanProgress(usize, usize),
    CleanDone(clean::Outcome, Vec<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    History,
    Settings,
}

#[derive(Clone, Copy, PartialEq)]
enum Tri {
    Off,
    Some,
    On,
}

type Key = (&'static str, String);

const ROWS_STEP: usize = 50;

struct App {
    p: Palette,
    icons: Icons,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,

    reports: Vec<ProviderReport>,
    order: Vec<&'static str>,
    scanning: bool,
    scan_ms: Option<u128>,
    /// Managers still being read in the current scan: (id, display name).
    pending: Vec<(&'static str, &'static str)>,
    scan_total: usize,
    scan_started: Instant,
    trim_after_scan: bool,

    page: Page,
    selected: HashSet<Key>,
    expanded: HashSet<&'static str>,
    shown: HashMap<&'static str, usize>,
    search: String,

    settings: Settings,
    state: State,
    autostart: bool,
    history: Vec<String>,

    confirm: Option<clean::Plan>,
    cleaning: Option<(usize, usize)>,
    /// What the running cleanup covers: (bytes, "npm, NuGet and Go").
    clean_what: (u64, String),
    clean_started: Instant,
    /// Recent (time, steps done) points; the time-left guess uses the pace
    /// over these, so a slow stretch earlier doesn't skew it.
    clean_samples: std::collections::VecDeque<(Instant, usize)>,
    toast: Option<(String, Instant, bool)>,

    shot: Option<Shot>,
    shot_frames: u32,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, settings: Settings) -> Self {
        let ctx = cc.egui_ctx.clone();
        let icons = theme::install_fonts(&ctx);
        let p = theme::palette(resolve_dark(settings.theme));
        theme::apply(&ctx, &p);
        let (tx, rx) = channel();
        let mut app = Self {
            p,
            icons,
            tx,
            rx,
            ctx,
            reports: Vec::new(),
            order: providers::all().iter().map(|p| p.id()).collect(),
            scanning: false,
            scan_ms: None,
            pending: Vec::new(),
            scan_total: 0,
            scan_started: Instant::now(),
            trim_after_scan: false,
            page: Page::Overview,
            selected: HashSet::new(),
            expanded: HashSet::new(),
            shown: HashMap::new(),
            search: String::new(),
            autostart: platform::autostart_enabled(),
            state: State::load(),
            settings,
            history: Vec::new(),
            confirm: None,
            cleaning: None,
            clean_what: (0, String::new()),
            clean_started: Instant::now(),
            clean_samples: Default::default(),
            toast: None,
            shot: None,
            shot_frames: 0,
        };
        app.start_scan(None);
        app
    }

    fn open_page(&mut self, name: &str) {
        self.page = match name {
            "settings" => Page::Settings,
            "history" => Page::History,
            _ => Page::Overview,
        };
        if self.page == Page::History {
            self.history = load_history();
        }
    }

    fn set_theme(&mut self) {
        self.p = theme::palette(resolve_dark(self.settings.theme));
        theme::apply(&self.ctx, &self.p);
    }

    // ---------- data ----------

    fn start_scan(&mut self, only: Option<Vec<String>>) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.pending = providers::all()
            .iter()
            .filter(|p| only.as_ref().is_none_or(|o| o.iter().any(|id| id == p.id())))
            .map(|p| (p.id(), p.name()))
            .collect();
        self.scan_total = self.pending.len();
        self.scan_started = Instant::now();
        let full = only.is_none();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            scan::scan_all_progress(
                only.as_deref(),
                |r| {
                    let _ = tx.send(Msg::Report(r));
                    ctx.request_repaint();
                },
                |id| {
                    let _ = tx.send(Msg::ProviderDone(id));
                    ctx.request_repaint();
                },
            );
            let _ = tx.send(Msg::ScanFinished(t.elapsed().as_millis(), full));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Report(r) => {
                    if let Some(i) = self.reports.iter().position(|x| x.id == r.id) {
                        self.reports[i] = r;
                    } else {
                        self.reports.push(r);
                    }
                    // Biggest cache first, like the treemap.
                    let order = &self.order;
                    self.reports.sort_by(|a, b| {
                        b.disk_size.cmp(&a.disk_size).then_with(|| {
                            let pos = |id: &str| order.iter().position(|o| *o == id).unwrap_or(usize::MAX);
                            pos(a.id).cmp(&pos(b.id))
                        })
                    });
                }
                Msg::ProviderDone(id) => self.pending.retain(|(p, _)| *p != id),
                Msg::ScanFinished(ms, full) => {
                    self.scanning = false;
                    self.pending.clear();
                    self.trim_after_scan = true;
                    self.scan_ms = Some(ms);
                    let live: HashSet<Key> =
                        self.reports.iter().flat_map(|r| r.packages.iter().map(move |p| (r.id, p.name.clone()))).collect();
                    self.selected.retain(|k| live.contains(k));
                    if full && self.shot.is_none() {
                        let mut st = State::load();
                        st.last_reclaimable = Some(self.reports.iter().map(|r| r.reclaimable()).sum());
                        let _ = st.save();
                        self.state = st;
                    }
                }
                Msg::CleanProgress(d, t) => {
                    self.cleaning = Some((d, t));
                    let now = Instant::now();
                    self.clean_samples.push_back((now, d));
                    // Keep the last ~10 s. Right after a long stall only the
                    // newest point is left, so it shows "estimating" briefly.
                    while self.clean_samples.len() > 1 && now.duration_since(self.clean_samples[0].0) > Duration::from_secs(10) {
                        self.clean_samples.pop_front();
                    }
                }
                Msg::CleanDone(out, ids) => {
                    self.cleaning = None;
                    let ok = out.errors.is_empty();
                    let uv_busy = out.errors.iter().any(|e| e == clean::UV_BUSY_MSG);
                    let msg = if ok {
                        format!("Freed about {} in {:.1} s", hb(out.freed_estimate), out.ms as f64 / 1000.0)
                    } else if uv_busy && out.errors.len() == 1 {
                        format!("Everything else is cleaned. {}", clean::UV_BUSY_MSG)
                    } else {
                        format!("Done with {} errors. First: {}", out.errors.len(), out.errors[0])
                    };
                    self.toast = Some((msg, Instant::now(), ok));
                    self.selected.clear();
                    self.start_scan(Some(ids));
                }
            }
        }
    }

    fn tri(&self, r: &ProviderReport) -> Tri {
        let n = r.packages.iter().filter(|p| self.selected.contains(&(r.id, p.name.clone()))).count();
        if n == 0 {
            Tri::Off
        } else if n == r.packages.len() {
            Tri::On
        } else {
            Tri::Some
        }
    }

    fn set_provider(&mut self, idx: usize, on: bool) {
        let r = &self.reports[idx];
        for pk in &r.packages {
            let k = (r.id, pk.name.clone());
            if on {
                self.selected.insert(k);
            } else {
                self.selected.remove(&k);
            }
        }
    }

    fn selected_items(&self) -> Vec<(&'static str, &Package)> {
        let mut v = Vec::new();
        for r in &self.reports {
            for p in &r.packages {
                if self.selected.contains(&(r.id, p.name.clone())) {
                    v.push((r.id, p));
                }
            }
        }
        v
    }

    fn save_settings(&mut self) {
        if self.settings.auto_clean {
            self.settings.run_in_tray = true;
        }
        let _ = self.settings.save();
        if self.shot.is_none() && (self.settings.auto_clean || self.settings.run_in_tray) {
            platform::ensure_tray();
        }
    }

    fn begin_clean(&mut self, plan: clean::Plan) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let mut ids: Vec<String> = plan.packages.iter().map(|(p, _)| p.clone()).collect();
        ids.sort();
        ids.dedup();
        let mut names: Vec<&str> = Vec::new();
        for (id, _) in &plan.packages {
            if let Some(r) = self.reports.iter().find(|r| r.id == id.as_str()) {
                if !names.contains(&r.name) {
                    names.push(r.name);
                }
            }
        }
        self.clean_what = (plan.bytes, join_names(&names));
        self.cleaning = Some((0, plan.actions.len()));
        self.clean_started = Instant::now();
        self.clean_samples = [(self.clean_started, 0)].into();
        std::thread::spawn(move || {
            let out = clean::execute(&plan, |d, t| {
                let _ = tx.send(Msg::CleanProgress(d, t));
                ctx.request_repaint();
            });
            let _ = tx.send(Msg::CleanDone(out, ids));
            ctx.request_repaint();
        });
    }

    // ---------- sidebar ----------

    fn sidebar(&mut self, ui: &mut Ui) {
        let p = self.p;
        let items = [(Page::Overview, "overview", "Overview"), (Page::History, "history", "History")];
        for (page, icon, label) in items {
            if nav_item(ui, &p, &self.icons, self.page == page, icon, label).clicked() {
                self.open_page(label.to_lowercase().as_str());
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(8.0);
            if nav_item(ui, &p, &self.icons, self.page == Page::Settings, "settings", "Settings").clicked() {
                self.open_page("settings");
            }
            ui.add_space(6.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 16.0), Sense::hover());
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                format!("Prune {}", env!("CARGO_PKG_VERSION")),
                FontId::proportional(11.5),
                p.sub,
            );
        });
    }

    // ---------- overview ----------

    fn overview(&mut self, ui: &mut Ui) {
        let p = self.p;
        egui::Panel::bottom("actions")
            .exact_size(60.0)
            .frame(Frame::new().fill(p.layer).inner_margin(Margin { left: 0, right: 0, top: 0, bottom: 0 }))
            .show(ui, |ui| self.action_bar(ui));

        self.header(ui);
        ui.add_space(14.0);
        if self.reports.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                if self.scanning {
                    ui.add(egui::Spinner::new().size(22.0).color(p.accent));
                    ui.label(RichText::new("Looking for package managers…").color(p.sub));
                } else {
                    ui.label(RichText::new("No package manager caches found").color(p.sub));
                }
            });
            return;
        }
        self.treemap(ui);
        ui.add_space(14.0);
        self.groups(ui);
    }

    fn header(&mut self, ui: &mut Ui) {
        let p = self.p;
        let freeable: u64 = self.reports.iter().map(|r| r.reclaimable()).sum();
        let cached: u64 = self.reports.iter().map(|r| r.disk_size).sum();
        let cutoff = SystemTime::now() - Duration::from_secs(self.settings.min_age_days as u64 * 86400);
        let stale: u64 = self
            .reports
            .iter()
            .flat_map(|r| r.packages.iter())
            .filter(|p| p.fetched().is_some_and(|f| f <= cutoff))
            .map(|p| p.reclaimable())
            .sum();
        let pkgs: usize = self.reports.iter().map(|r| r.packages.iter().filter(|p| p.kind == Kind::Package).count()).sum();

        let top = ui.cursor().min;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Can be freed").size(12.0).color(p.sub));
                let (num, unit) = split_size(freeable);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(num).font(semibold(56.0)));
                    ui.add_space(2.0);
                    ui.vertical(|ui| {
                        ui.add_space(26.0);
                        ui.label(RichText::new(unit).font(semibold(22.0)));
                    });
                });
            });
            ui.add_space(28.0);
            let stat = |ui: &mut Ui, label: &str, value: String| {
                ui.vertical(|ui| {
                    ui.add_space(22.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(label).size(12.0).color(p.sub));
                    ui.label(RichText::new(value).font(semibold(19.0)));
                });
                ui.add_space(22.0);
            };
            stat(ui, "Cached", hb(cached));
            stat(ui, &format!("Not fetched in {} days", self.settings.min_age_days), hb(stale));
            stat(ui, "Packages", group_digits(pkgs));
            ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                let label = format!("{}  Rescan", self.icons.get("refresh"));
                let busy = self.scanning || self.cleaning.is_some();
                if ui.add_enabled(!busy, std_button(&p, &label)).clicked() {
                    self.start_scan(None);
                }
                let edit = egui::TextEdit::singleline(&mut self.search)
                    .hint_text(format!("{}  Search packages", self.icons.get("search")))
                    .desired_width(220.0)
                    .margin(Margin::symmetric(10, 6));
                ui.add(edit);
            });
        });
        let y = ui.cursor().min.y + 4.0;
        let line = top.x..=ui.max_rect().right();
        if self.scanning {
            self.scan_progress(ui, line, y);
        } else {
            ui.painter().hline(line, y, Stroke::new(1.0, p.rule));
            ui.add_space(6.0);
        }
    }

    /// While scanning: the header rule becomes a progress bar, with a status
    /// line naming the managers still being read.
    fn scan_progress(&mut self, ui: &mut Ui, line: std::ops::RangeInclusive<f32>, y: f32) {
        let p = self.p;
        let total = self.scan_total.max(1);
        let done = total.saturating_sub(self.pending.len());
        let target = done as f32 / total as f32;
        let frac = ui.ctx().animate_value_with_time(ui.scope_id().with("scan-frac"), target, 0.3);

        let track = Rect::from_x_y_ranges(line, (y - 1.5)..=(y + 1.5));
        let painter = ui.painter();
        painter.rect_filled(track, CornerRadius::same(2), p.stroke);
        let fill = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * frac, track.bottom()));
        painter.rect_filled(fill, CornerRadius::same(2), p.accent);

        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(egui::Spinner::new().size(14.0).color(p.accent));
            let names: Vec<&str> = self.pending.iter().map(|(_, n)| *n).collect();
            let reading = match names.len() {
                0 => "Finishing up".to_string(),
                1..=3 => format!("Reading {}", join_and(&names)),
                n => format!("Reading {} and {} more", names[..2].join(", "), n - 2),
            };
            ui.label(RichText::new(reading).font(semibold(13.0)).color(p.text));
            let secs = self.scan_started.elapsed().as_secs();
            ui.label(RichText::new(format!("{done} of {total} managers done · {secs} s")).size(13.0).color(p.sub));
        });
    }

    fn treemap(&mut self, ui: &mut Ui) {
        let p = self.p;
        let (area, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 196.0), Sense::hover());
        let items: Vec<(usize, f64)> =
            self.reports.iter().enumerate().filter(|(_, r)| r.disk_size > 0).map(|(i, r)| (i, r.disk_size as f64)).collect();
        let rects = squarify(&items.iter().map(|x| x.1).collect::<Vec<_>>(), area);
        let mut toggle: Option<(usize, bool)> = None;
        let mut expand: Option<&'static str> = None;
        for ((idx, _), rect) in items.iter().zip(rects) {
            let r = &self.reports[*idx];
            let rect = rect.shrink(2.0);
            let (strong, tint, dtint) = theme::provider_colors(r.id);
            let fill = if p.dark { dtint } else { tint };
            let resp = ui.interact(rect, Id::unique(("tm", r.id)), Sense::click());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, CornerRadius::same(6), fill);
            // Hatch caches that are mostly hard-linked (deleting them frees little).
            if r.reclaimable() * 2 < r.disk_size {
                let c = if p.dark { Color32::from_white_alpha(22) } else { Color32::from_black_alpha(20) };
                let mut x = rect.left() - rect.height();
                while x < rect.right() {
                    painter.line_segment([Pos2::new(x, rect.bottom()), Pos2::new(x + rect.height(), rect.top())], Stroke::new(2.0, c));
                    x += 9.0;
                }
            }
            let tri = self.tri(r);
            let outline = if p.dark { Color32::WHITE } else { strong };
            let ring = match tri {
                Tri::On => Some(Stroke::new(2.0, outline)),
                Tri::Some => Some(Stroke::new(1.0, outline)),
                Tri::Off if resp.hovered() => Some(Stroke::new(1.0, p.text.gamma_multiply(0.5))),
                Tri::Off => None,
            };
            if let Some(s) = ring {
                painter.rect_stroke(rect, CornerRadius::same(6), s, egui::StrokeKind::Inside);
            }
            // Labels, as many lines as fit.
            let pad = Vec2::new(10.0, 7.0);
            let mut y = rect.top() + pad.y;
            let x = rect.left() + pad.x;
            if rect.width() > 46.0 && rect.height() > 22.0 {
                painter.text(Pos2::new(x, y), Align2::LEFT_TOP, r.name, semibold(13.0), p.text);
                if tri == Tri::On && rect.width() > 70.0 {
                    painter.text(Pos2::new(rect.right() - pad.x, y), Align2::RIGHT_TOP, self.icons.get("check"), icon_font(12.0), p.text);
                }
                y += 17.0;
            }
            if rect.width() > 46.0 && rect.height() > 42.0 {
                painter.text(Pos2::new(x, y), Align2::LEFT_TOP, hb(r.disk_size), FontId::proportional(12.0), p.text);
                y += 16.0;
            }
            if rect.width() > 96.0 && rect.height() > 60.0 {
                painter.text(
                    Pos2::new(x, y),
                    Align2::LEFT_TOP,
                    format!("frees {}", hb(r.reclaimable())),
                    FontId::proportional(11.0),
                    p.text.gamma_multiply(0.75),
                );
            }
            let n = r.packages.iter().filter(|p| p.kind == Kind::Package).count();
            let resp = resp.on_hover_text(format!(
                "{} · {} cached · frees {} · {} packages\nClick to select · double-click to show packages",
                r.name,
                hb(r.disk_size),
                hb(r.reclaimable()),
                group_digits(n)
            ));
            if resp.double_clicked() {
                expand = Some(r.id);
            } else if resp.clicked() {
                toggle = Some((*idx, tri != Tri::On));
            }
        }
        if let Some(id) = expand {
            self.expanded.insert(id);
        } else if let Some((i, on)) = toggle {
            self.set_provider(i, on);
        }
    }

    fn groups(&mut self, ui: &mut Ui) {
        let p = self.p;
        let q = self.search.trim().to_lowercase();
        let mut toggle_group: Option<(usize, bool)> = None;
        let mut toggle_expand: Option<&'static str> = None;
        let mut rule: Option<(&'static str, bool)> = None;
        let mut toggle_pkg: Vec<Key> = Vec::new();
        let mut more: Option<(&'static str, usize)> = None;
        let mut reveal: Option<std::path::PathBuf> = None;

        // Select all, lined up with the group checkboxes. With a search it
        // applies only to the matching packages.
        let visible: Vec<Key> = self
            .reports
            .iter()
            .flat_map(|r| {
                r.packages.iter().filter(|pk| q.is_empty() || pk.name.to_lowercase().contains(&q)).map(move |pk| (r.id, pk.name.clone()))
            })
            .collect();
        let n_sel = visible.iter().filter(|k| self.selected.contains(*k)).count();
        let all_tri = if n_sel == 0 {
            Tri::Off
        } else if n_sel == visible.len() {
            Tri::On
        } else {
            Tri::Some
        };
        let mut select_all: Option<bool> = None;
        ui.horizontal(|ui| {
            ui.add_space(17.0);
            let label = if q.is_empty() { "Select all" } else { "Select all matching" };
            let c = check(ui, &p, all_tri);
            ui.add_space(4.0);
            let l = ui.add(egui::Label::new(RichText::new(label).size(13.0)).sense(Sense::click()));
            if c.clicked() || l.clicked() {
                select_all = Some(all_tri != Tri::On);
            }
            if n_sel > 0 {
                ui.label(
                    RichText::new(format!("{} of {} items selected", group_digits(n_sel), group_digits(visible.len())))
                        .size(12.0)
                        .color(p.sub),
                );
            }
        });
        ui.add_space(6.0);
        if let Some(on) = select_all {
            for k in visible {
                if on {
                    self.selected.insert(k);
                } else {
                    self.selected.remove(&k);
                }
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (ri, r) in self.reports.iter().enumerate() {
                let matches: Vec<&Package> = r.packages.iter().filter(|pk| q.is_empty() || pk.name.to_lowercase().contains(&q)).collect();
                if !q.is_empty() && matches.is_empty() {
                    continue;
                }
                // Installed but nothing cached: a slim, quiet row (no checkbox,
                // nothing to expand) so the manager still shows up.
                if r.packages.is_empty() {
                    let (strong, _, _) = theme::provider_colors(r.id);
                    Frame::new().stroke(Stroke::new(1.0, p.stroke)).corner_radius(6).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::hover());
                        let mut cui = ui.new_child(
                            UiBuilder::new().max_rect(row.shrink2(Vec2::new(16.0, 0.0))).layout(Layout::left_to_right(Align::Center)),
                        );
                        cui.add_space(28.0); // where the checkbox sits on other rows
                        swatch(&mut cui, strong.gamma_multiply(0.5));
                        cui.add_space(2.0);
                        cui.label(RichText::new(r.name).font(semibold(14.0)).color(p.sub));
                        cui.add_space(6.0);
                        cui.label(RichText::new("Installed · cache is empty").size(12.0).color(p.sub));
                        cui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(25.0); // lines the switch up with the chevron rows
                            let mut on = self.settings.has_rule(r.id, None);
                            if toggle(ui, &p, &mut on).on_hover_text("Include in scheduled auto-clean").changed() {
                                rule = Some((r.id, on));
                            }
                            ui.label(RichText::new("Auto").size(12.0).color(p.sub));
                        });
                    });
                    ui.add_space(6.0);
                    continue;
                }
                let open = self.expanded.contains(r.id) || !q.is_empty();
                let tri = self.tri(r);
                let (strong, _, _) = theme::provider_colors(r.id);
                Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.stroke)).corner_radius(6).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // Header row: the whole row toggles expansion; widgets on top of it win.
                    let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 50.0), Sense::hover());
                    let bg = ui.interact(row, Id::unique(("grp", r.id)), Sense::click());
                    if bg.hovered() {
                        ui.painter().rect_filled(row, CornerRadius::same(6), p.hover);
                    }
                    if bg.clicked() {
                        toggle_expand = Some(r.id);
                    }
                    let mut cui = ui.new_child(
                        UiBuilder::new().max_rect(row.shrink2(Vec2::new(16.0, 0.0))).layout(Layout::left_to_right(Align::Center)),
                    );
                    if check(&mut cui, &p, tri).on_hover_text(format!("Select all of {}", r.name)).clicked() {
                        toggle_group = Some((ri, tri != Tri::On));
                    }
                    cui.add_space(4.0);
                    swatch(&mut cui, strong);
                    cui.add_space(2.0);
                    cui.vertical(|ui| {
                        ui.add_space(7.0);
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(r.name).font(semibold(14.0)));
                        ui.label(RichText::new(group_meta(r, &self.selected)).size(12.0).color(p.sub));
                    });
                    cui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let chev = self.icons.get(if open { "chevron_up" } else { "chevron_down" });
                        ui.label(RichText::new(chev).font(icon_font(11.0)).color(p.sub));
                        ui.add_space(6.0);
                        let mut on = self.settings.has_rule(r.id, None);
                        if toggle(ui, &p, &mut on).on_hover_text("Include in scheduled auto-clean").changed() {
                            rule = Some((r.id, on));
                        }
                        ui.label(RichText::new("Auto").size(12.0).color(p.sub));
                        ui.add_space(14.0);
                        ui.add_sized(
                            [96.0, 20.0],
                            egui::Label::new(RichText::new(format!("frees {}", hb(r.reclaimable()))).size(12.0).color(p.sub)),
                        );
                        ui.add_sized([74.0, 20.0], egui::Label::new(RichText::new(hb(r.disk_size)).size(14.0)));
                    });
                    if open {
                        ui.painter().hline(row.left()..=row.right(), row.bottom(), Stroke::new(1.0, p.hair));
                        let limit = *self.shown.get(r.id).unwrap_or(&ROWS_STEP);
                        Frame::new().inner_margin(Margin { left: 46, right: 16, top: 2, bottom: 10 }).show(ui, |ui| {
                            pkg_header(ui, &p);
                            for pk in matches.iter().take(limit) {
                                let key: Key = (r.id, pk.name.clone());
                                let sel = self.selected.contains(&key);
                                let resp = pkg_row(ui, &p, pk, sel);
                                if resp.clicked() {
                                    toggle_pkg.push(key.clone());
                                }
                                let first = pk.versions.iter().flat_map(|v| v.targets.iter()).find_map(|t| match t {
                                    Target::Path(x) | Target::Shared { path: x, .. } => Some(x.clone()),
                                    _ => None,
                                });
                                resp.context_menu(|ui| {
                                    if ui.button(if sel { "Deselect" } else { "Select" }).clicked() {
                                        toggle_pkg.push(key.clone());
                                        ui.close();
                                    }
                                    if let Some(path) = &first {
                                        if ui.button("Show in Explorer").clicked() {
                                            reveal = Some(path.clone());
                                            ui.close();
                                        }
                                    }
                                });
                            }
                            if matches.len() > limit {
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    let rest = matches.len() - limit;
                                    if ui.link(format!("Show {} more", rest.min(ROWS_STEP * 4))).clicked() {
                                        more = Some((r.id, limit + ROWS_STEP * 4));
                                    }
                                    if ui.link(format!("Show all {}", group_digits(matches.len()))).clicked() {
                                        more = Some((r.id, usize::MAX));
                                    }
                                });
                            }
                        });
                    }
                });
                ui.add_space(6.0);
            }
        });

        if let Some((i, on)) = toggle_group {
            self.set_provider(i, on);
        } else if let Some(id) = toggle_expand {
            if !self.expanded.remove(id) {
                self.expanded.insert(id);
            }
        }
        for k in toggle_pkg {
            if !self.selected.remove(&k) {
                self.selected.insert(k);
            }
        }
        if let Some((id, on)) = rule {
            self.settings.set_rule(id, None, on);
            self.save_settings();
        }
        if let Some((id, n)) = more {
            self.shown.insert(id, n);
        }
        if let Some(path) = reveal {
            platform::reveal(&path);
        }
    }

    fn action_bar(&mut self, ui: &mut Ui) {
        let p = self.p;
        let y = ui.max_rect().top();
        ui.painter().hline(ui.max_rect().left()..=ui.max_rect().right(), y, Stroke::new(1.0, p.hair));
        let sel = self.selected_items();
        let bytes: u64 = sel.iter().map(|(_, p)| p.reclaimable()).sum();
        let mut names: Vec<&str> = Vec::new();
        for (id, _) in &sel {
            if let Some(r) = self.reports.iter().find(|r| r.id == *id) {
                if !names.contains(&r.name) {
                    names.push(r.name);
                }
            }
        }
        let (mut go, mut clear) = (false, false);
        ui.horizontal_centered(|ui| {
            let text = if sel.is_empty() {
                "Select managers or packages to clean. They download again when a project needs them.".to_string()
            } else {
                format!("{} selected · {} items", join_names(&names), group_digits(sel.len()))
            };
            ui.label(RichText::new(text).size(13.0).color(p.sub));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !sel.is_empty() && self.cleaning.is_none() && !self.scanning;
                let label = if sel.is_empty() { "Clean up".to_string() } else { format!("Clean up {}", hb(bytes)) };
                go = ui.add_enabled(enabled, accent_button(&p, &label)).clicked();
                if !sel.is_empty() {
                    clear = ui.add(std_button(&p, "Clear")).clicked();
                }
            });
        });
        if go {
            let plan = clean::plan(&sel);
            self.confirm = Some(plan);
        } else if clear {
            self.selected.clear();
        }
    }

    // ---------- history ----------

    fn history_page(&mut self, ui: &mut Ui) {
        let p = self.p;
        ui.label(RichText::new("History").font(semibold(28.0)));
        ui.add_space(10.0);
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                let last =
                    self.state.last_auto_run.map(|u| timefmt::relative(Some(timefmt::from_unix(u)))).unwrap_or_else(|| "never".into());
                kv(ui, &p, "Freed so far", hb(self.state.total_freed));
                ui.add_space(30.0);
                kv(ui, &p, "Last auto-clean", last);
                ui.add_space(30.0);
                kv(ui, &p, "Last run freed", hb(self.state.last_freed));
            });
        });
        ui.add_space(14.0);
        if self.history.is_empty() {
            ui.label(RichText::new("No cleanups yet. Cleanups and auto-clean runs are listed here.").color(p.sub));
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            card(ui, &p, |ui| {
                for (i, line) in self.history.iter().enumerate() {
                    let (when, what) = line.split_once(' ').unwrap_or(("", line.as_str()));
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [170.0, 24.0],
                            egui::Label::new(RichText::new(when.replace('T', "  ").trim_end_matches('Z')).size(12.0).color(p.sub)),
                        );
                        ui.label(what);
                    });
                    if i + 1 < self.history.len() {
                        let y = ui.cursor().min.y;
                        ui.painter().hline(ui.max_rect().left()..=ui.max_rect().right(), y, Stroke::new(1.0, p.hair));
                        ui.add_space(4.0);
                    }
                }
            });
        });
    }

    // ---------- settings ----------

    fn settings_page(&mut self, ui: &mut Ui) {
        let p = self.p;
        let mut changed = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("Settings").font(semibold(28.0)));
            ui.add_space(10.0);

            section_title(ui, "Auto-clean");
            card(ui, &p, |ui| {
                setting_row(ui, &p, "Clean up automatically", Some("Runs from the tray, even when this window is closed"), |ui| {
                    changed |= toggle(ui, &p, &mut self.settings.auto_clean).changed();
                    ui.label(RichText::new(if self.settings.auto_clean { "On" } else { "Off" }).size(13.0).color(p.sub));
                });
                divider(ui, &p);
                ui.add_enabled_ui(self.settings.auto_clean, |ui| {
                    setting_row(ui, &p, "Run every", None, |ui| {
                        egui::ComboBox::from_id_salt("unit").width(110.0).selected_text(self.settings.interval_unit.label()).show_ui(
                            ui,
                            |ui| {
                                for u in IntervalUnit::ALL {
                                    changed |= ui.selectable_value(&mut self.settings.interval_unit, u, u.label()).changed();
                                }
                            },
                        );
                        changed |= ui.add(egui::DragValue::new(&mut self.settings.interval_value).range(1..=999)).changed();
                    });
                    divider(ui, &p);
                    setting_row(ui, &p, "Only packages not fetched in the last", Some("Recently used packages stay cached"), |ui| {
                        ui.label("days");
                        changed |= ui.add(egui::DragValue::new(&mut self.settings.min_age_days).range(0..=3650)).changed();
                    });
                    divider(ui, &p);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label("Applies to");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let sel = clean::auto_selection(&self.reports, &self.settings);
                            let plan = clean::plan(&sel);
                            ui.label(
                                RichText::new(format!(
                                    "If it ran now: {} packages · about {}",
                                    group_digits(plan.packages.len()),
                                    hb(plan.bytes)
                                ))
                                .size(12.0)
                                .color(p.sub),
                            );
                        });
                    });
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(4.0);
                        let ids: Vec<(&'static str, &'static str)> = self.reports.iter().map(|r| (r.id, r.name)).collect();
                        for (id, name) in ids {
                            let mut on = self.settings.has_rule(id, None);
                            if chip(ui, &p, name, &mut on).clicked() {
                                self.settings.set_rule(id, None, on);
                                changed = true;
                            }
                        }
                    });
                    ui.add_space(8.0);
                });
            });

            ui.add_space(14.0);
            section_title(ui, "Background");
            card(ui, &p, |ui| {
                setting_row(ui, &p, "Keep running in the tray", Some("Needed for auto-clean. Uses about 2 MB of RAM."), |ui| {
                    let mut v = self.settings.run_in_tray || self.settings.auto_clean;
                    let r = ui.add_enabled_ui(!self.settings.auto_clean, |ui| toggle(ui, &p, &mut v)).inner;
                    if r.changed() {
                        self.settings.run_in_tray = v;
                        changed = true;
                    }
                });
                divider(ui, &p);
                setting_row(ui, &p, "Start with Windows", Some("Starts the tray only, no window"), |ui| {
                    let mut v = self.autostart;
                    if toggle(ui, &p, &mut v).changed() && platform::set_autostart(v) {
                        self.autostart = v;
                        self.settings.start_with_windows = v;
                        changed = true;
                    }
                });
            });

            ui.add_space(14.0);
            section_title(ui, "Appearance");
            card(ui, &p, |ui| {
                setting_row(ui, &p, "Theme", None, |ui| {
                    let before = self.settings.theme;
                    egui::ComboBox::from_id_salt("theme").width(170.0).selected_text(self.settings.theme.label()).show_ui(ui, |ui| {
                        for t in Theme::ALL {
                            ui.selectable_value(&mut self.settings.theme, t, t.label());
                        }
                    });
                    if self.settings.theme != before {
                        changed = true;
                    }
                });
            });

            ui.add_space(14.0);
            section_title(ui, "Data");
            card(ui, &p, |ui| {
                let dir = devsweep_core::settings::data_dir();
                let shown = dir.display().to_string().replace(&devsweep_core::fsutil::app_data().display().to_string(), "%APPDATA%");
                let desc = format!("{shown} · settings, history.log, debug-report.txt");
                setting_row(ui, &p, "Settings and history", Some(desc.as_str()), |ui| {
                    if ui.add(std_button(&p, &format!("{}  Open folder", self.icons.get("folder")))).clicked() {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    }
                });
            });
        });
        if changed {
            self.save_settings();
            self.set_theme();
        }
    }

    // ---------- dialogs ----------

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let p = self.p;
        let Some(plan) = self.confirm.clone() else { return };
        let mut go = false;
        let mut cancel = false;
        // Per manager: (id, name, items, bytes), biggest first.
        let mut by: Vec<(&'static str, &'static str, usize, u64)> = Vec::new();
        for (prov, name) in &plan.packages {
            let Some(r) = self.reports.iter().find(|r| r.id == prov.as_str()) else { continue };
            let bytes = r.packages.iter().find(|pk| &pk.name == name).map_or(0, |pk| pk.reclaimable());
            match by.iter_mut().find(|e| e.0 == r.id) {
                Some(e) => {
                    e.2 += 1;
                    e.3 += bytes;
                }
                None => by.push((r.id, r.name, 1, bytes)),
            }
        }
        by.sort_by_key(|e| std::cmp::Reverse(e.3));
        let items = plan.packages.len();

        // Windows 11 dialog: content area, then a footer strip with the buttons.
        let dialog = Frame::new().fill(p.layer).stroke(Stroke::new(1.0, p.stroke)).corner_radius(8).shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 32,
            spread: 0,
            color: Color32::from_black_alpha(90),
        });
        let resp = egui::Modal::new(Id::unique("confirm")).frame(dialog).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            Frame::new().inner_margin(Margin { left: 24, right: 24, top: 22, bottom: 20 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("Clean up {}?", hb(plan.bytes))).font(semibold(20.0)));
                ui.add_space(10.0);
                let what = match by.as_slice() {
                    [one] => format!("{} {} from the {} cache will be deleted.", group_digits(items), plural(items, "item"), one.1),
                    _ => format!("{} {} from {} caches will be deleted.", group_digits(items), plural(items, "item"), by.len()),
                };
                ui.label(RichText::new(what).size(14.0).color(p.text));
                ui.add_space(16.0);

                // Breakdown table.
                Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.stroke)).corner_radius(6).inner_margin(Margin::symmetric(14, 4)).show(
                    ui,
                    |ui| {
                        ui.set_width(ui.available_width());
                        let rows = by.len().min(6);
                        for (i, (id, name, n, bytes)) in by.iter().take(rows).enumerate() {
                            if i > 0 {
                                let y = ui.cursor().min.y;
                                ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.hair));
                            }
                            let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
                            let painter = ui.painter();
                            let (strong, _, _) = theme::provider_colors(id);
                            let cy = row.center().y;
                            painter.rect_filled(
                                Rect::from_center_size(Pos2::new(row.left() + 5.0, cy), Vec2::splat(10.0)),
                                CornerRadius::same(3),
                                strong,
                            );
                            painter.text(Pos2::new(row.left() + 20.0, cy), Align2::LEFT_CENTER, *name, semibold(13.5), p.text);
                            painter.text(
                                Pos2::new(row.right() - 96.0, cy),
                                Align2::RIGHT_CENTER,
                                format!("{} {}", group_digits(*n), plural(*n, "item")),
                                FontId::proportional(13.0),
                                p.sub,
                            );
                            painter.text(Pos2::new(row.right(), cy), Align2::RIGHT_CENTER, hb(*bytes), semibold(13.5), p.text);
                        }
                        if by.len() > rows {
                            let rest: usize = by[rows..].iter().map(|e| e.2).sum();
                            let y = ui.cursor().min.y;
                            ui.painter().hline(ui.max_rect().x_range(), y, Stroke::new(1.0, p.hair));
                            let (row, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::hover());
                            ui.painter().text(
                                Pos2::new(row.left() + 20.0, row.center().y),
                                Align2::LEFT_CENTER,
                                format!("{} more managers · {} items", by.len() - rows, group_digits(rest)),
                                FontId::proportional(13.0),
                                p.sub,
                            );
                        }
                    },
                );

                ui.add_space(14.0);
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(self.icons.get("info")).font(icon_font(13.0)).color(p.sub));
                    ui.add(
                        egui::Label::new(
                            RichText::new("Your projects keep working. Package managers download what they need again.")
                                .size(12.5)
                                .color(p.sub),
                        )
                        .wrap(),
                    );
                });
            });

            // Footer strip.
            let (footer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 80.0), Sense::hover());
            ui.painter().rect_filled(footer, CornerRadius { nw: 0, ne: 0, sw: 8, se: 8 }, p.bg);
            ui.painter().hline(footer.x_range(), footer.top(), Stroke::new(1.0, p.stroke));
            let inner = footer.shrink2(Vec2::new(24.0, 24.0));
            let mut fui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
            fui.spacing_mut().item_spacing.x = 8.0;
            let w = (inner.width() - 8.0) / 2.0;
            if fui.add_sized([w, 32.0], accent_button(&p, "Clean up")).clicked() {
                go = true;
            }
            if fui.add_sized([w, 32.0], std_button(&p, "Cancel")).clicked() {
                cancel = true;
            }
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
        if go {
            self.confirm = None;
            self.begin_clean(plan);
        } else if cancel || resp.should_close() {
            self.confirm = None;
        }
    }

    fn progress_modal(&mut self, ctx: &egui::Context) {
        let Some((d, t)) = self.cleaning else { return };
        let p = self.p;
        let (bytes, names) = self.clean_what.clone();
        let target = if t == 0 { 0.0 } else { d as f32 / t as f32 };
        let frac = ctx.animate_value_with_time(Id::unique("clean-frac"), target, 0.2);
        let dialog = Frame::new()
            .fill(p.layer)
            .stroke(Stroke::new(1.0, p.stroke))
            .corner_radius(8)
            .inner_margin(Margin { left: 24, right: 24, top: 22, bottom: 24 })
            .shadow(egui::epaint::Shadow { offset: [0, 8], blur: 32, spread: 0, color: Color32::from_black_alpha(90) });
        egui::Modal::new(Id::unique("cleaning")).frame(dialog).show(ctx, |ui| {
            ui.set_width(412.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(format!("Cleaning up {}", hb(bytes))).font(semibold(20.0)));
            ui.add_space(6.0);
            if !names.is_empty() {
                ui.label(RichText::new(names).size(13.0).color(p.sub));
            }
            ui.add_space(20.0);

            // Progress row: step count on the left, percent on the right.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                ui.label(RichText::new(format!("{} of {} steps", group_digits(d), group_digits(t))).size(13.0).color(p.text));
                let eta = eta_text(d, t, self.clean_started.elapsed(), self.clean_samples.front().copied());
                ui.label(RichText::new(format!("· {eta}")).size(13.0).color(p.sub));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(format!("{:.0}%", target * 100.0)).font(semibold(13.0)).color(p.text));
                });
            });
            ui.add_space(10.0);
            let (track, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
            ui.painter().rect_filled(track, CornerRadius::same(2), p.stroke);
            let fill = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * frac, track.bottom()));
            ui.painter().rect_filled(fill, CornerRadius::same(2), p.accent);
            ui.add_space(16.0);
            ui.label(RichText::new("Keep Prune open until this finishes.").size(12.5).color(p.sub));
        });
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, at, ok)) = self.toast.clone() else { return };
        // Problems stay up longer so there's time to read them.
        if at.elapsed() > Duration::from_secs(if ok { 6 } else { 15 }) {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let p = self.p;
        egui::Area::new(Id::unique("toast")).anchor(Align2::RIGHT_BOTTOM, Vec2::new(-24.0, -76.0)).show(ctx, |ui| {
            Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.stroke)).corner_radius(6).inner_margin(Margin::symmetric(14, 10)).show(
                ui,
                |ui| {
                    ui.set_max_width(440.0);
                    ui.horizontal_top(|ui| {
                        let (ic, c) = if ok { (self.icons.get("check"), p.ok) } else { (self.icons.get("warning"), p.danger) };
                        ui.label(RichText::new(ic).font(icon_font(14.0)).color(c));
                        ui.add(egui::Label::new(msg).wrap());
                    });
                },
            );
        });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.p.bg.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll();
        let ctx = ui.ctx().clone();
        // The tray asked for a page, or Windows switched theme while we were away.
        if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(true)))) {
            if let Some(page) = platform::take_page_request() {
                self.open_page(&page);
            }
            if self.settings.theme == Theme::System {
                self.set_theme();
            }
        }
        // In the background (or minimized): hand idle memory back to Windows.
        // Also right after a scan that finished while we were in the background.
        let lost_focus = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::WindowFocused(false))));
        let busy = self.scanning || self.cleaning.is_some();
        if !busy && (lost_focus || std::mem::take(&mut self.trim_after_scan)) && !ctx.input(|i| i.focused) {
            platform::trim_memory();
        }
        let p = self.p;

        egui::Panel::left("nav")
            .exact_size(184.0)
            .resizable(false)
            // top: 0 so the first nav item lines up with the body's top edge.
            .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 8, right: 8, top: 0, bottom: 6 }))
            .show(ui, |ui| self.sidebar(ui));

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(p.layer)
                    .stroke(Stroke::new(1.0, p.stroke))
                    .corner_radius(CornerRadius { nw: 8, ne: 0, sw: 0, se: 0 })
                    .inner_margin(Margin { left: 28, right: 28, top: 20, bottom: 0 }),
            )
            .show(ui, |ui| match self.page {
                Page::Overview => self.overview(ui),
                Page::History => self.history_page(ui),
                Page::Settings => self.settings_page(ui),
            });

        self.confirm_modal(&ctx);
        self.progress_modal(&ctx);
        self.toast_ui(&ctx);
        if self.scanning {
            // Keeps the spinner and timer moving; only for the few seconds a scan takes.
            let ms = if self.page == Page::Overview { 50 } else { 250 };
            ctx.request_repaint_after(Duration::from_millis(ms));
        }
        self.screenshot_step(&ctx);
    }
}

impl App {
    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some((state, path)) = self.shot.as_ref().map(|s| (s.state.clone(), s.path.clone())) else { return };
        ctx.request_repaint();
        // "scanning" captures the window mid-scan instead of after it.
        let mid = state.split(',').any(|s| s == "scanning");
        let ready = if mid {
            self.scanning && self.scan_started.elapsed() > Duration::from_millis(1500)
        } else {
            !self.scanning && self.scan_ms.is_some()
        };
        if self.shot_frames == 0 && !ready {
            return;
        }
        self.shot_frames += 1;
        if self.shot_frames == 1 {
            for part in state.split(',') {
                match part {
                    "settings" | "history" => self.open_page(part),
                    "dark" | "light" => {
                        self.settings.theme = if part == "dark" { Theme::Dark } else { Theme::Light };
                        self.set_theme();
                    }
                    // Shows the progress dialog only; nothing is executed.
                    "cleaning" => {
                        self.clean_what = (6_120_000_000, "npm and NuGet".into());
                        self.cleaning = Some((87, 266));
                    }
                    "select" | "confirm" => {
                        for i in 0..self.reports.len().min(5) {
                            if matches!(self.reports[i].id, "npm" | "nuget" | "go") {
                                self.set_provider(i, true);
                            }
                        }
                        if part == "confirm" {
                            self.confirm = Some(clean::plan(&self.selected_items()));
                        }
                    }
                    id if !id.is_empty() => {
                        if let Some(r) = self.reports.iter().find(|r| r.id == id) {
                            self.expanded.insert(r.id);
                        }
                    }
                    _ => {}
                }
            }
        }
        if self.shot_frames == 40 {
            let c = ctx.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let [w, h] = img.size;
                let _ = std::fs::write(&path, crate::png::encode_rgba(w as u32, h as u32, img.as_raw()));
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            })));
        }
    }
}

// ---------- helpers ----------

fn resolve_dark(t: Theme) -> bool {
    match t {
        Theme::System => platform::system_dark(),
        Theme::Light => false,
        Theme::Dark => true,
    }
}

/// Time left, guessed from the pace since `since` (a point ~10 s back), so a
/// step that stalled earlier doesn't skew it. Waits for a couple of seconds
/// of progress first, and rounds so the text doesn't jitter.
fn eta_text(done: usize, total: usize, elapsed: Duration, since: Option<(Instant, usize)>) -> String {
    let Some((t0, d0)) = since else { return "estimating time left…".into() };
    let secs = t0.elapsed().as_secs_f64();
    let steps = done.saturating_sub(d0);
    if steps == 0 || secs < 2.0 || elapsed.as_secs_f64() < 2.0 {
        return "estimating time left…".into();
    }
    let left = (total - done.min(total)) as f64 * secs / steps as f64;
    match left as u64 {
        0..=4 => "almost done".into(),
        s @ 5..=59 => format!("about {} s left", s.div_ceil(5) * 5),
        s @ 60..=5399 => {
            let m = (s + 30) / 60;
            format!("about {m} min left")
        }
        s => format!("about {:.1} h left", s as f64 / 3600.0),
    }
}

/// ["uv", "npm", "Bun"] -> "uv, npm and Bun"
fn join_and(v: &[&str]) -> String {
    match v {
        [] => String::new(),
        [a] => a.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn load_history() -> Vec<String> {
    let path = devsweep_core::settings::data_dir().join("history.log");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().rev().take(300).map(str::to_string).collect()
}

/// "22.1 GB" -> ("22.1", "GB")
fn split_size(b: u64) -> (String, String) {
    let s = hb(b);
    match s.split_once(' ') {
        Some((n, u)) => (n.to_string(), u.to_string()),
        None => (s, String::new()),
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn join_names(names: &[&str]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].to_string(),
        n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
    }
}

fn group_meta(r: &ProviderReport, selected: &HashSet<Key>) -> String {
    let n = r.packages.iter().filter(|p| p.kind == Kind::Package).count();
    let sel = r.packages.iter().filter(|p| selected.contains(&(r.id, p.name.clone()))).count();
    let extras = r.packages.len() - n;
    let mut s = if n == 0 {
        // Only logs, indexes, temp files and the like are left.
        format!("No packages · {} other {}", group_digits(extras), plural(extras, "item"))
    } else {
        format!("{} {}", group_digits(n), plural(n, "package"))
    };
    if sel == r.packages.len() && sel > 0 {
        s.push_str(" · all selected");
    } else if sel > 0 {
        s.push_str(&format!(" · {} selected", group_digits(sel)));
    }
    if n > 0 && r.reclaimable() * 2 < r.disk_size {
        s.push_str(" · mostly hard-linked, frees little");
    }
    s
}

/// Squarified treemap (Bruls et al.). `values` sorted descending.
fn squarify(values: &[f64], rect: Rect) -> Vec<Rect> {
    let n = values.len();
    let mut out = vec![Rect::NOTHING; n];
    let total: f64 = values.iter().sum();
    if n == 0 || total <= 0.0 {
        return out;
    }
    let scale = (rect.width() as f64 * rect.height() as f64) / total;
    let a: Vec<f64> = values.iter().map(|v| v * scale).collect();
    let worst = |row: &[f64], side: f64| {
        let s: f64 = row.iter().sum();
        let (mx, mn) = row.iter().fold((0.0f64, f64::MAX), |(mx, mn), &v| (mx.max(v), mn.min(v)));
        ((side * side * mx) / (s * s)).max((s * s) / (side * side * mn))
    };
    let mut r = rect;
    let mut i = 0;
    while i < n {
        let side = r.width().min(r.height()) as f64;
        let mut j = i + 1;
        while j < n && worst(&a[i..=j], side) <= worst(&a[i..j], side) {
            j += 1;
        }
        let s: f64 = a[i..j].iter().sum();
        if r.width() >= r.height() {
            let w = (s / r.height() as f64) as f32;
            let mut y = r.top();
            for k in i..j {
                let h = (a[k] / w as f64) as f32;
                out[k] = Rect::from_min_size(Pos2::new(r.left(), y), Vec2::new(w, h));
                y += h;
            }
            r.min.x += w;
        } else {
            let h = (s / r.width() as f64) as f32;
            let mut x = r.left();
            for k in i..j {
                let w = (a[k] / h as f64) as f32;
                out[k] = Rect::from_min_size(Pos2::new(x, r.top()), Vec2::new(w, h));
                x += w;
            }
            r.min.y += h;
        }
        i = j;
    }
    out
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn swatch(ui: &mut Ui, c: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    ui.painter().rect_filled(rect.shrink(1.0), CornerRadius::same(3), c);
}

fn accent_button(p: &Palette, label: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label.to_string()).font(semibold(14.0)).color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(4)
        .min_size(Vec2::new(0.0, 32.0))
}

fn std_button(p: &Palette, label: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label.to_string()).size(14.0).color(p.text))
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.stroke))
        .corner_radius(4)
        .min_size(Vec2::new(0.0, 32.0))
}

fn nav_item(ui: &mut Ui, p: &Palette, icons: &Icons, active: bool, icon: &str, label: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let painter = ui.painter();
    if active || resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(4), if active { p.nav_sel } else { p.hover });
    }
    if active {
        let bar = Rect::from_center_size(Pos2::new(rect.left() + 1.5, rect.center().y), Vec2::new(3.0, 16.0));
        painter.rect_filled(bar, CornerRadius::same(2), p.accent);
    }
    painter.text(Pos2::new(rect.left() + 14.0, rect.center().y), Align2::LEFT_CENTER, icons.get(icon), icon_font(15.0), p.text);
    painter.text(Pos2::new(rect.left() + 42.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.0), p.text);
    resp
}

/// Fluent switch.
fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(40.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.1);
        let enabled = ui.is_enabled();
        let painter = ui.painter();
        let fade = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
        if t > 0.5 {
            painter.rect_filled(rect, CornerRadius::same(10), fade(p.accent));
        } else {
            painter.rect(
                rect.shrink(0.5),
                CornerRadius::same(10),
                Color32::TRANSPARENT,
                Stroke::new(1.0, fade(p.sub)),
                egui::StrokeKind::Inside,
            );
        }
        let x = egui::lerp((rect.left() + 10.0)..=(rect.right() - 10.0), t);
        let r = if resp.hovered() && enabled { 7.0 } else { 6.0 };
        let knob = if t > 0.5 { p.on_accent } else { p.sub };
        painter.circle_filled(Pos2::new(x, rect.center().y), r, fade(knob));
    }
    resp
}

/// Fluent checkbox with an indeterminate state.
fn check(ui: &mut Ui, p: &Palette, state: Tri) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let b = rect.shrink(1.0);
        let painter = ui.painter();
        if state == Tri::Off {
            let fill = if resp.hovered() { p.hover } else { p.card };
            painter.rect(b, CornerRadius::same(4), fill, Stroke::new(1.0, p.sub), egui::StrokeKind::Inside);
        } else {
            painter.rect_filled(b, CornerRadius::same(4), p.accent);
            let c = b.center();
            let ink = Stroke::new(1.6, p.on_accent);
            if state == Tri::On {
                painter.line(vec![Pos2::new(c.x - 4.5, c.y), Pos2::new(c.x - 1.5, c.y + 3.0), Pos2::new(c.x + 4.5, c.y - 3.5)], ink);
            } else {
                painter.line_segment([Pos2::new(c.x - 4.0, c.y), Pos2::new(c.x + 4.0, c.y)], ink);
            }
        }
    }
    resp
}

fn chip(ui: &mut Ui, p: &Palette, label: &str, on: &mut bool) -> Response {
    let text = if *on { format!("✓ {label}") } else { label.to_string() };
    let galley = ui.painter().layout_no_wrap(text, FontId::proportional(13.0), if *on { p.on_accent } else { p.text });
    let size = galley.size() + Vec2::new(24.0, 12.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let painter = ui.painter();
    if *on {
        painter.rect_filled(rect, CornerRadius::same(15), p.accent);
    } else {
        let fill = if resp.hovered() { p.hover } else { p.card };
        painter.rect(rect, CornerRadius::same(15), fill, Stroke::new(1.0, p.stroke), egui::StrokeKind::Inside);
    }
    painter.galley(rect.center() - galley.size() / 2.0, galley, p.text);
    resp
}

fn card(ui: &mut Ui, p: &Palette, add: impl FnOnce(&mut Ui)) {
    Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.stroke)).corner_radius(6).inner_margin(Margin::symmetric(16, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

fn section_title(ui: &mut Ui, t: &str) {
    ui.label(RichText::new(t).font(semibold(14.0)));
    ui.add_space(2.0);
}

fn divider(ui: &mut Ui, p: &Palette) {
    let r = ui.max_rect();
    let y = ui.cursor().min.y;
    ui.painter().hline((r.left() - 16.0)..=(r.right() + 16.0), y, Stroke::new(1.0, p.hair));
    ui.add_space(1.0);
}

fn kv(ui: &mut Ui, p: &Palette, k: &str, v: String) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(k).size(12.0).color(p.sub));
        ui.label(RichText::new(v).font(semibold(18.0)));
    });
}

/// Label + optional description on the left, controls right-aligned.
fn setting_row(ui: &mut Ui, p: &Palette, title: &str, desc: Option<&str>, controls: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(if desc.is_some() { 50.0 } else { 42.0 });
        let text_w = (ui.available_width() - 300.0).max(200.0);
        ui.vertical(|ui| {
            ui.set_max_width(text_w);
            ui.add_space(if desc.is_some() { 7.0 } else { 11.0 });
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(title);
            if let Some(d) = desc {
                ui.add(egui::Label::new(RichText::new(d).size(12.0).color(p.sub)).truncate());
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), controls);
    });
}

const PKG_COLS: [f32; 3] = [70.0, 84.0, 100.0];

fn pkg_header(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let painter = ui.painter();
    let f = FontId::proportional(11.0);
    painter.text(Pos2::new(rect.left() + 30.0, rect.center().y), Align2::LEFT_CENTER, "Package", f.clone(), p.sub);
    let mut x = rect.right();
    for (w, label) in PKG_COLS.iter().rev().zip(["Fetched", "Size", "Versions"]) {
        painter.text(Pos2::new(x, rect.center().y), Align2::RIGHT_CENTER, label, f.clone(), p.sub);
        x -= w;
    }
    painter.hline(rect.left()..=rect.right(), rect.bottom(), Stroke::new(1.0, p.rule));
}

fn pkg_row(ui: &mut Ui, p: &Palette, pk: &Package, sel: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(3), p.hover);
    }
    // Checkbox drawn in place (the whole row is the click target).
    let b = Rect::from_min_size(Pos2::new(rect.left() + 1.0, rect.center().y - 9.0), Vec2::splat(18.0));
    if sel {
        painter.rect_filled(b, CornerRadius::same(4), p.accent);
        let c = b.center();
        painter.line(
            vec![Pos2::new(c.x - 4.0, c.y), Pos2::new(c.x - 1.0, c.y + 3.0), Pos2::new(c.x + 4.0, c.y - 3.0)],
            Stroke::new(1.6, p.on_accent),
        );
    } else {
        painter.rect(b, CornerRadius::same(4), p.card, Stroke::new(1.0, p.sub), egui::StrokeKind::Inside);
    }
    let cols: f32 = PKG_COLS.iter().sum();
    let name_w = (rect.width() - 30.0 - cols - 12.0).max(60.0);
    let mut x = rect.left() + 30.0;
    let clip = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(name_w, rect.height()));
    let tp = painter.with_clip_rect(clip);
    let f = FontId::proportional(13.0);
    if pk.kind == Kind::Other {
        let g = tp.layout_no_wrap("extra".into(), FontId::proportional(10.5), p.sub);
        let pill = Rect::from_min_size(Pos2::new(x, rect.center().y - 8.0), g.size() + Vec2::new(10.0, 3.0));
        tp.rect_filled(pill, CornerRadius::same(8), p.hover);
        tp.galley(pill.center() - g.size() / 2.0, g, p.sub);
        x = pill.right() + 6.0;
    }
    let name = tp.text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, &pk.name, f.clone(), p.text);
    if let Some(note) = &pk.note {
        tp.text(
            Pos2::new(name.right() + 8.0, rect.center().y),
            Align2::LEFT_CENTER,
            note,
            FontId::proportional(12.0),
            mix(p.sub, p.card, 0.15),
        );
    }
    let mut rx = rect.right();
    let vals =
        [timefmt::relative(pk.fetched()), hb(pk.size()), if pk.kind == Kind::Package { pk.versions.len().to_string() } else { "—".into() }];
    for (i, (w, v)) in PKG_COLS.iter().rev().zip(vals).enumerate() {
        let c = if i == 1 { p.text } else { p.sub };
        painter.text(Pos2::new(rx, rect.center().y), Align2::RIGHT_CENTER, v, f.clone(), c);
        rx -= w;
    }
    painter.hline(rect.left()..=rect.right(), rect.bottom(), Stroke::new(1.0, p.hair));
    resp.on_hover_text(format!(
        "{}\n{} on disk · frees {} · fetched {}",
        pk.name,
        hb(pk.size()),
        hb(pk.reclaimable()),
        timefmt::date(pk.fetched())
    ))
}

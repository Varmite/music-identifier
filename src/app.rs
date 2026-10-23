use crate::capture::{self, Source};
use eframe::egui;
use crate::engine::{Engine, Event};
use crate::shazam::{self, Track};
use egui::{
    pos2, vec2, Align, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke,
    TextureHandle, Ui,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- palette

const BG: Color32 = Color32::from_rgb(0x0e, 0x10, 0x16);
const CARD: Color32 = Color32::from_rgb(0x17, 0x1a, 0x23);
const CARD_HI: Color32 = Color32::from_rgb(0x21, 0x25, 0x31);
const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2f, 0x3d);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xf0);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x91, 0xa3);
const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x7c, 0xff);
const ACCENT_2: Color32 = Color32::from_rgb(0x9b, 0x5c, 0xff);
const GOOD: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x8c);
const WARN: Color32 = Color32::from_rgb(0xff, 0x9f, 0x43);

pub fn setup_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(0x0a, 0x0c, 0x11);
    v.faint_bg_color = CARD_HI;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, BORDER);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
        &mut v.widgets.noninteractive,
    ] {
        w.corner_radius = CornerRadius::same(8);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.weak_bg_fill = CARD_HI;
    v.widgets.inactive.bg_fill = CARD_HI;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x30, 0x40);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x30, 0x38, 0x4c);
    v.widgets.open.weak_bg_fill = Color32::from_rgb(0x2a, 0x30, 0x40);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 30.0;
    });
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
}

fn section_label(ui: &mut Ui, s: &str) {
    ui.label(RichText::new(s.to_uppercase()).size(11.0).color(MUTED).strong());
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn ago(t: u64) -> String {
    let d = now_secs().saturating_sub(t);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86_400),
    }
}

// ---------------------------------------------------------------- persistence

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    continuous: bool,
    history: Vec<Track>,
}

fn save_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("music-identifier").join("history.json"))
}

fn load_saved() -> Saved {
    save_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Saved { continuous: true, history: vec![] })
}

// ---------------------------------------------------------------- app

enum Cover {
    Loading,
    Ready(TextureHandle),
    Failed,
}

pub struct App {
    sources: Vec<Source>,
    sources_loading: bool,
    sources_rx: Option<Receiver<Result<Vec<Source>, String>>>,
    last_refresh: Instant,
    selected: Option<Source>,
    filter: String,

    continuous: bool,
    engine: Option<Engine>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    listening_since: Option<Instant>,
    status: String,
    error: Option<String>,

    current: Option<Track>,
    history: Vec<Track>,
    covers: HashMap<String, Cover>,
    cover_tx: Sender<(String, Option<egui::ColorImage>)>,
    cover_rx: Receiver<(String, Option<egui::ColorImage>)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_style(&cc.egui_ctx);
        let saved = load_saved();
        let (tx, rx) = channel();
        let (cover_tx, cover_rx) = channel();
        let mut app = Self {
            sources: vec![],
            sources_loading: false,
            sources_rx: None,
            last_refresh: Instant::now(),
            selected: None,
            filter: String::new(),
            continuous: saved.continuous,
            engine: None,
            tx,
            rx,
            listening_since: None,
            status: format!("Pick a {} that's playing music", capture::SOURCE_KIND),
            error: None,
            current: saved.history.first().cloned(),
            history: saved.history,
            covers: HashMap::new(),
            cover_tx,
            cover_rx,
        };
        app.refresh_sources(&cc.egui_ctx);
        app
    }

    fn persist(&self) {
        let Some(p) = save_path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let s = Saved { continuous: self.continuous, history: self.history.clone() };
        if let Ok(b) = serde_json::to_vec_pretty(&s) {
            let _ = std::fs::write(p, b);
        }
    }

    fn refresh_sources(&mut self, ctx: &egui::Context) {
        if self.sources_loading {
            return;
        }
        self.sources_loading = true;
        self.last_refresh = Instant::now();
        let (tx, rx) = channel();
        self.sources_rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(capture::list_sources());
            ctx.request_repaint();
        });
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(src) = self.selected.clone() else {
            self.error = Some(format!("Select a {} first", capture::SOURCE_KIND));
            return;
        };
        self.error = None;
        let c = ctx.clone();
        match Engine::start(&src, self.continuous, self.tx.clone(), move || c.request_repaint()) {
            Ok(e) => {
                self.engine = Some(e);
                self.listening_since = Some(Instant::now());
                self.status = "Listening…".into();
            }
            Err(e) => {
                self.error = Some(e);
                self.status = "Couldn't start capture".into();
            }
        }
    }

    fn stop(&mut self) {
        self.engine = None;
        self.listening_since = None;
        // Drain stale events from the old worker.
        while self.rx.try_recv().is_ok() {}
        self.status = "Stopped".into();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.sources_rx {
            if let Ok(r) = rx.try_recv() {
                self.sources_loading = false;
                self.sources_rx = None;
                match r {
                    Ok(list) => {
                        // Keep the selection fresh (titles change, e.g. browser tabs).
                        if let Some(sel) = &mut self.selected {
                            if let Some(s) = list.iter().find(|s| s.id == sel.id) {
                                *sel = s.clone();
                            }
                        }
                        self.sources = list;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }
        if self.engine.is_none()
            && !self.sources_loading
            && self.last_refresh.elapsed() > Duration::from_secs(4)
        {
            self.refresh_sources(ctx);
        }

        while let Ok(ev) = self.rx.try_recv() {
            if self.engine.is_none() {
                continue;
            }
            match ev {
                Event::Status(s) => {
                    self.status = s;
                }
                Event::Matched(t) => {
                    self.error = None;
                    self.status = format!("Found: {}", t.title);
                    self.history.retain(|h| h.key != t.key);
                    self.history.insert(0, t.clone());
                    self.history.truncate(200);
                    self.current = Some(t);
                    self.persist();
                }
                Event::NoMatch(n) => {
                    self.status = if n >= 4 {
                        format!("No match yet ({n} tries) — still listening…")
                    } else {
                        "No match yet — listening a bit longer…".into()
                    };
                }
                Event::Error(e) => self.error = Some(e),
                Event::Finished(err) => {
                    self.engine = None;
                    self.listening_since = None;
                    match err {
                        Some(e) => {
                            self.error = Some(e);
                            self.status = "Capture stopped".into();
                        }
                        None => self.status = "Done".into(),
                    }
                }
            }
        }

        while let Ok((url, img)) = self.cover_rx.try_recv() {
            let c = match img {
                Some(img) => Cover::Ready(ctx.load_texture(&url, img, egui::TextureOptions::LINEAR)),
                None => Cover::Failed,
            };
            self.covers.insert(url, c);
        }
    }

    fn cover(&mut self, ctx: &egui::Context, url: &str) -> Option<TextureHandle> {
        match self.covers.get(url) {
            Some(Cover::Ready(t)) => return Some(t.clone()),
            Some(_) => return None,
            None => {}
        }
        self.covers.insert(url.to_string(), Cover::Loading);
        let (url, tx, ctx) = (url.to_string(), self.cover_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let img = shazam::fetch_bytes(&url).ok().and_then(|b| image::load_from_memory(&b).ok()).map(|i| {
                let i = i.resize(400, 400, image::imageops::FilterType::Triangle).to_rgba8();
                egui::ColorImage::from_rgba_unmultiplied([i.width() as _, i.height() as _], &i)
            });
            let _ = tx.send((url, img));
            ctx.request_repaint();
        });
        None
    }

    // ------------------------------------------------------------ UI pieces

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
            eq_icon(ui, r, None, 0.0, ACCENT);
            ui.label(RichText::new("Music Identifier").size(20.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(env!("CARGO_PKG_VERSION")).size(11.0).color(MUTED));
            });
        });
    }

    fn source_picker(&mut self, ui: &mut Ui) {
        let listening = self.engine.is_some();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_label(ui, &format!("Source {}", capture::SOURCE_KIND));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = ui
                        .add_enabled(!listening, egui::Button::new("").min_size(vec2(30.0, 26.0)))
                        .on_hover_text("Refresh list");
                    let col = if listening { MUTED.gamma_multiply(0.5) } else { TEXT };
                    refresh_icon(ui, r.rect.center(), if self.sources_loading { ui.input(|i| i.time) as f32 * 6.0 } else { 0.0 }, col);
                    if r.clicked() {
                        self.refresh_sources(ui.ctx());
                    }
                });
            });

            let label = match &self.selected {
                Some(s) => source_text(s, 52),
                None => RichText::new(format!("Choose a {}…", capture::SOURCE_KIND)).color(MUTED),
            };
            ui.add_enabled_ui(!listening, |ui| {
                egui::ComboBox::from_id_salt("source")
                    .width(ui.available_width())
                    .height(380.0)
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("Filter…")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        let f = self.filter.to_lowercase();
                        let mut shown = 0;
                        for s in &self.sources {
                            if !f.is_empty()
                                && !s.title.to_lowercase().contains(&f)
                                && !s.app.to_lowercase().contains(&f)
                            {
                                continue;
                            }
                            shown += 1;
                            let sel = self.selected.as_ref().map(|x| x.id) == Some(s.id);
                            if ui.selectable_label(sel, source_text(s, 70)).clicked() {
                                self.selected = Some(s.clone());
                            }
                        }
                        if shown == 0 {
                            ui.label(RichText::new(if cfg!(target_os = "linux") {
                                "No audio streams. Start playback, then refresh."
                            } else {
                                "Nothing found."
                            }).color(MUTED));
                        }
                    });
            });

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let r = ui.add_enabled(!listening, egui::Checkbox::new(&mut self.continuous, "Keep listening"));
                if r.changed() {
                    self.persist();
                }
                r.on_hover_text("Keep identifying as songs change, instead of stopping after the first match");
                if let Some(s) = &self.selected {
                    if s.audible {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new("• playing").size(12.0).color(GOOD));
                        });
                    }
                }
            });
        });
    }

    fn listen_area(&mut self, ui: &mut Ui) {
        let t = ui.input(|i| i.time);
        let listening = self.engine.is_some();
        let level = self.engine.as_ref().map(|e| e.level()).unwrap_or(0.0);
        let buffered = self.engine.as_ref().map(|e| e.buffered_secs()).unwrap_or(0.0);

        ui.vertical_centered(|ui| {
            ui.add_space(8.0);
            let size = 150.0;
            let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
            let c = rect.center();
            let p = ui.painter();
            let base_r = 56.0;
            if listening {
                // Pulse rings
                for k in 0..3 {
                    let ph = ((t * 0.6 + k as f64 / 3.0) % 1.0) as f32;
                    let r = base_r + ph * 20.0 + level.min(1.0) * 6.0;
                    p.circle_stroke(c, r, Stroke::new(2.0, ACCENT.gamma_multiply((1.0 - ph) * 0.6)));
                }
            }
            let hover = resp.hovered();
            let fill = if listening {
                lerp_color(ACCENT, ACCENT_2, (t.sin() as f32 + 1.0) * 0.5)
            } else if hover {
                lerp_color(ACCENT, Color32::WHITE, 0.12)
            } else {
                ACCENT
            };
            p.circle_filled(c, base_r + if hover { 2.0 } else { 0.0 }, fill);
            p.circle_stroke(c, base_r, Stroke::new(1.0, Color32::from_white_alpha(40)));
            let icon = egui::Rect::from_center_size(c, vec2(46.0, 40.0));
            eq_icon(ui, icon, listening.then_some(level), t, Color32::WHITE);

            let resp = resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(if listening { "Stop" } else { "Identify" });
            if resp.clicked() {
                if listening {
                    self.stop();
                } else {
                    self.start(ui.ctx());
                }
            }

            ui.label(RichText::new(if listening { "Tap to stop" } else { "Tap to identify" }).size(12.0).color(MUTED));
            ui.add_space(2.0);
            ui.label(RichText::new(&self.status).size(15.0));
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).size(12.0).color(WARN));
            }

            if listening {
                ui.add_space(4.0);
                let w = 220.0;
                meter(ui, w, level.sqrt().min(1.0), GOOD, "level");
                meter(ui, w, (buffered / 12.0).min(1.0), ACCENT, "sample");
                if let Some(s) = self.listening_since {
                    ui.label(RichText::new(format!("{:.0}s", s.elapsed().as_secs_f32())).size(11.0).color(MUTED));
                }
            }
        });
    }

    fn result_card(&mut self, ui: &mut Ui) {
        let Some(t) = self.current.clone() else { return };
        let tex = t.cover_url.as_deref().and_then(|u| self.cover(ui.ctx(), u));
        card(ui, |ui| {
            ui.horizontal_top(|ui| {
                let s = 112.0;
                match tex {
                    Some(tex) => {
                        ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(vec2(s, s)).corner_radius(10));
                    }
                    None => {
                        let (r, _) = ui.allocate_exact_size(vec2(s, s), Sense::hover());
                        ui.painter().rect_filled(r, 10, CARD_HI);
                        eq_icon(ui, r.shrink(36.0), None, 0.0, MUTED);
                    }
                }
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(RichText::new(&t.title).size(19.0).strong()).wrap());
                    ui.add(egui::Label::new(RichText::new(&t.artist).size(15.0).color(ACCENT)).wrap());
                    let mut meta = vec![];
                    if let Some(a) = &t.album {
                        meta.push(a.clone());
                    }
                    if let Some(r) = &t.released {
                        meta.push(r.clone());
                    }
                    if !meta.is_empty() {
                        ui.add(egui::Label::new(RichText::new(meta.join(" · ")).size(12.5).color(MUTED)).wrap());
                    }
                    let mut meta2 = vec![];
                    if let Some(g) = &t.genre {
                        meta2.push(g.clone());
                    }
                    if let Some(l) = &t.label {
                        meta2.push(l.clone());
                    }
                    if !meta2.is_empty() {
                        ui.add(egui::Label::new(RichText::new(meta2.join(" · ")).size(12.0).color(MUTED)).wrap());
                    }
                    ui.label(RichText::new(ago(t.recognised_at)).size(11.0).color(MUTED));
                });
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(u) = &t.shazam_url {
                    link_btn(ui, "Shazam", u);
                }
                if let Some(u) = &t.apple_music_url {
                    link_btn(ui, "Apple Music", u);
                }
                link_btn(ui, "Spotify", &t.spotify_url());
                link_btn(ui, "YouTube", &t.youtube_url());
                if ui.button("Copy").on_hover_text("Copy “Artist – Title”").clicked() {
                    ui.ctx().copy_text(format!("{} – {}", t.artist, t.title));
                }
            });
        });
    }

    fn history_list(&mut self, ui: &mut Ui) {
        if self.history.is_empty() {
            return;
        }
        ui.horizontal(|ui| {
            section_label(ui, &format!("History · {}", self.history.len()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.history.clear();
                    self.current = None;
                    self.persist();
                }
            });
        });
        let items = self.history.clone();
        for t in &items {
            let tex = t.cover_url.as_deref().and_then(|u| self.cover(ui.ctx(), u));
            let selected = self.current.as_ref().map(|c| &c.key) == Some(&t.key);
            let resp = egui::Frame::new()
                .fill(if selected { CARD_HI } else { CARD })
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let s = 38.0;
                        match tex {
                            Some(tex) => {
                                ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(vec2(s, s)).corner_radius(6));
                            }
                            None => {
                                let (r, _) = ui.allocate_exact_size(vec2(s, s), Sense::hover());
                                ui.painter().rect_filled(r, 6, CARD_HI);
                            }
                        }
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.add(egui::Label::new(RichText::new(&t.title).strong()).truncate());
                            ui.horizontal(|ui| {
                                ui.add(egui::Label::new(RichText::new(&t.artist).size(12.0).color(MUTED)).truncate());
                            });
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(ago(t.recognised_at)).size(11.0).color(MUTED));
                        });
                    });
                })
                .response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                self.current = Some(t.clone());
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        if self.engine.is_some() {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(Margin::same(16)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Keep content clear of the floating scrollbar.
                    ui.set_max_width(ui.available_width() - 10.0);
                    self.header(ui);
                    ui.add_space(4.0);
                    self.source_picker(ui);
                    self.listen_area(ui);
                    ui.add_space(6.0);
                    self.result_card(ui);
                    ui.add_space(6.0);
                    self.history_list(ui);
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist();
    }
}

// ---------------------------------------------------------------- helpers

fn source_text(s: &Source, max: usize) -> RichText {
    let mut title = s.title.clone();
    if title.chars().count() > max {
        title = title.chars().take(max - 1).collect::<String>() + "…";
    }
    let txt = if s.app.is_empty() || s.app.eq_ignore_ascii_case(&s.title) {
        title
    } else {
        format!("{title}  —  {}", s.app)
    };
    if s.audible {
        RichText::new(format!("•  {txt}")).color(GOOD)
    } else {
        RichText::new(txt)
    }
}

fn link_btn(ui: &mut Ui, label: &str, url: &str) {
    if ui.button(label).on_hover_text(url).clicked() {
        let _ = open::that_detached(url);
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Five-bar equaliser glyph; animated when `level` is Some.
fn eq_icon(ui: &Ui, rect: egui::Rect, level: Option<f32>, t: f64, col: Color32) {
    let p = ui.painter();
    let n = 5;
    let gap = rect.width() / (n as f32 * 2.0 - 1.0);
    let base = [0.45, 0.8, 1.0, 0.65, 0.35];
    for i in 0..n {
        let h = match level {
            Some(l) => {
                let wob = ((t * 7.0 + i as f64 * 1.7).sin() as f32 + 1.0) * 0.5;
                (0.2 + (l.sqrt() * 1.6).min(1.0) * (0.35 + 0.65 * wob)).min(1.0)
            }
            None => base[i],
        } * rect.height();
        let x = rect.left() + i as f32 * gap * 2.0;
        let r = egui::Rect::from_min_max(pos2(x, rect.center().y - h / 2.0), pos2(x + gap, rect.center().y + h / 2.0));
        p.rect_filled(r, CornerRadius::same((gap / 2.0) as u8), col);
    }
}

/// Circular-arrow refresh glyph, rotated by `angle` radians.
fn refresh_icon(ui: &Ui, c: egui::Pos2, angle: f32, col: Color32) {
    let r = 6.5;
    let (a0, a1) = (angle + 0.6, angle + 0.6 + 5.0);
    let pts: Vec<egui::Pos2> = (0..=24)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / 24.0;
            pos2(c.x + r * a.cos(), c.y + r * a.sin())
        })
        .collect();
    let end = *pts.last().unwrap();
    let p = ui.painter();
    p.add(egui::Shape::line(pts, Stroke::new(1.6, col)));
    // Arrowhead tangent to the arc's end.
    let t = vec2(-a1.sin(), a1.cos());
    let n = vec2(a1.cos(), a1.sin());
    p.add(egui::Shape::convex_polygon(
        vec![end + t * 3.5, end - n * 3.2 - t * 1.0, end + n * 3.2 - t * 1.0],
        col,
        Stroke::NONE,
    ));
}

fn meter(ui: &mut Ui, w: f32, v: f32, col: Color32, label: &str) {
    ui.horizontal(|ui| {
        let total = ui.available_width();
        ui.add_space(((total - w - 60.0) / 2.0).max(0.0));
        ui.add_sized(vec2(52.0, 14.0), egui::Label::new(RichText::new(label).size(10.5).color(MUTED)));
        let (r, _) = ui.allocate_exact_size(vec2(w, 6.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(r, 3, CARD_HI);
        let mut f = r;
        f.set_width(r.width() * v.clamp(0.0, 1.0));
        p.rect_filled(f, 3, col);
    });
}

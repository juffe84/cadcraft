//! Opening drawings without freezing the window (#694). A file is read and decoded on a worker
//! thread (the slow part of OPEN, which needs no session); the UI shows an "Opening ..." card with
//! a spinner and the elapsed time and keeps painting. The finished drawing is added to the session
//! on the UI thread. The web build has no threads and opens synchronously.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use cadcraft_engine::cmd::file::{ReadFile, read_file_with_progress};

use crate::theme::Tokens;

type Outcome = Result<ReadFile, String>;

/// Compare file paths without requiring the file to still exist. Native aliases are resolved so
/// a symlink and its target share a drawing; web paths can only be compared directly.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b)
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

struct Job {
    path: PathBuf,
    started: Instant,
    rx: Receiver<Outcome>,
    progress: Option<Arc<AtomicU8>>,
}

/// The files being read in the background, oldest first.
#[derive(Default)]
pub struct Opening {
    jobs: Vec<Job>,
    #[cfg(not(target_arch = "wasm32"))]
    clouds: VecDeque<PathBuf>,
    #[cfg(not(target_arch = "wasm32"))]
    cloud_prompt: Option<CloudPrompt>,
    #[cfg(not(target_arch = "wasm32"))]
    restore_focus: Option<egui::Id>,
}

#[cfg(not(target_arch = "wasm32"))]
struct CloudPrompt {
    step: u16,
    radius: f64,
    block_points: u32,
    previous_focus: Option<egui::Id>,
    focused: bool,
}

impl Opening {
    /// Start reading `path` on a worker thread.
    pub fn start(&mut self, path: &Path) {
        self.start_with_preview(path, 0);
    }

    fn start_with_preview(&mut self, path: &Path, step: u16) {
        // Repeated events can arrive before the first background read completes.
        if self.jobs.iter().any(|j| same_path(&j.path, path)) {
            return;
        }
        let (tx, rx) = channel();
        let p = path.to_path_buf();
        let progress = (step > 1).then(|| Arc::new(AtomicU8::new(0)));
        let worker_progress = progress.clone();
        let spawned = std::thread::Builder::new().name("cadcraft-open".into()).spawn(move || {
            let outcome = if let Some(progress) = worker_progress {
                read_file_with_progress(
                    &p,
                    step,
                    Some(&|read, total| {
                        let percent = read.min(total) * 100 / total.max(1);
                        progress.store(percent as u8, Ordering::Relaxed);
                    }),
                )
            } else {
                read_file_with_progress(&p, step, None)
            };
            let _ = tx.send(outcome);
        });
        if spawned.is_err() {
            // No thread to read on: report it like any other failed open.
            let (tx, rx2) = channel();
            let _ = tx.send(Err(format!("open: {}: could not start a reader thread", path.display())));
            self.jobs.push(Job { path: path.to_path_buf(), started: Instant::now(), rx: rx2, progress });
            return;
        }
        self.jobs.push(Job { path: path.to_path_buf(), started: Instant::now(), rx, progress });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn queue_cloud(&mut self, path: &Path) {
        self.clouds.push_back(path.to_path_buf());
    }

    pub(crate) fn prompting(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            !self.clouds.is_empty()
        }
        #[cfg(target_arch = "wasm32")]
        false
    }

    /// Whether any file is still being read.
    pub fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// The finished reads, as (path, result). A reader that died without answering is an error.
    pub fn poll(&mut self) -> Vec<(PathBuf, Outcome)> {
        let mut done = Vec::new();
        self.jobs.retain(|j| match j.rx.try_recv() {
            Ok(r) => {
                done.push((j.path.clone(), r));
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                done.push((j.path.clone(), Err(format!("open: {}: the file could not be read", j.path.display()))));
                false
            }
        });
        done
    }

    /// The text of the card: the file and how long it has taken, or how many files are queued.
    fn label(&self) -> Option<String> {
        let first = self.jobs.first()?;
        let name = first.path.file_name().unwrap_or(first.path.as_os_str()).to_string_lossy();
        let secs = first.started.elapsed().as_secs_f32();
        let more = self.jobs.len() - 1;
        Some(if more == 0 { format!("Opening {name}... {secs:.0} s") } else { format!("Opening {name} and {more} more... {secs:.0} s") })
    }

    /// Paint the card in the middle of the window and keep frames coming while files load.
    pub fn show(&self, ctx: &egui::Context) {
        let Some(label) = self.label() else { return };
        let progress = self.jobs.first().and_then(|job| job.progress.as_ref()).map(|p| p.load(Ordering::Relaxed));
        let t = Tokens::get();
        egui::Area::new(egui::Id::unique("cc_opening"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(t.panel)
                    .stroke(egui::Stroke::new(1.0, t.accent))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(18, 12))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().color(t.accent));
                            ui.label(egui::RichText::new(label).color(t.text));
                        });
                        if let Some(percent) = progress {
                            let name = "Point cloud overview";
                            let label = ui.label(crate::i18n::t(name));
                            let bar = ui
                                .add(egui::ProgressBar::new(f32::from(percent) / 100.0).desired_width(220.0).fill(t.accent).show_percentage())
                                .labelled_by(label.id);
                            crate::control::record_widget(ui, &bar, name, "progressbar", None);
                        }
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

pub(crate) fn detail_controls(ui: &mut egui::Ui, radius: &mut f64, block_points: &mut u32) -> bool {
    use cadcraft_doc::point_cloud::DetailOptions;
    let label = "Load radius (×)";
    let radius = ui.add(egui::Slider::new(radius, 0.0..=DetailOptions::MAX_RADIUS_FACTOR).step_by(0.01).text(crate::i18n::t(label)));
    crate::control::record_widget(ui, &radius, label, "slider", None);
    let label = "Loading block size (k points)";
    let max = DetailOptions::MAX_BLOCK_POINTS / 1_000;
    let mut thousands = *block_points / 1_000;
    let block = ui.add(egui::Slider::new(&mut thousands, 0..=max).step_by(50.0).text(crate::i18n::t(label)));
    crate::control::record_widget(ui, &block, label, "slider", None);
    if block.changed() {
        *block_points = thousands * 1_000;
    }
    radius.changed() || block.changed()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn cloud_prompt(app: &mut crate::CadApp, ctx: &egui::Context) {
    use crate::i18n::t;
    let modal_id = egui::Id::unique("cc_point_cloud_open");
    let Some(path) = app.opening.clouds.front() else {
        if let Some(id) = app.opening.restore_focus {
            // The previous frame's modal layer can still disable the opener.
            let layer = ctx.memory(|mem| mem.top_modal_layer());
            if layer.is_none() {
                ctx.memory_mut(|mem| mem.request_focus(id));
                app.opening.restore_focus = None;
            } else if layer == Some(egui::Modal::default_area(modal_id).layer()) {
                ctx.request_repaint();
            }
        }
        return;
    };
    let settings = &app.session.settings;
    let prior_focus = &mut app.opening.restore_focus;
    let prompt = app.opening.cloud_prompt.get_or_insert_with(|| CloudPrompt {
        step: if settings.point_cloud_preview_step == 0 { 100 } else { settings.point_cloud_preview_step },
        radius: settings.point_cloud_radius_factor,
        block_points: settings.point_cloud_block_points,
        previous_focus: prior_focus.take().or_else(|| ctx.memory(|mem| mem.focused())),
        focused: false,
    });
    let mut open = false;
    let mut cancel = false;
    let modal = crate::modal::instant(ctx, modal_id).show(ctx, |ui| {
        ui.set_min_width(340.0);
        ui.heading(t("Open point cloud"));
        ui.label(path.file_name().unwrap_or(path.as_os_str()).to_string_lossy());
        let label = "Preview interval";
        let step = ui.add(egui::Slider::new(&mut prompt.step, 1..=cadcraft_doc::point_cloud::MAX_PREVIEW_STEP).logarithmic(true).text(t(label)));
        crate::control::record_widget(ui, &step, label, "slider", None);
        if !prompt.focused {
            step.request_focus();
            prompt.focused = true;
        }
        ui.label(t("1 loads all points. 1000 keeps every thousandth point in the preview."));
        ui.add_enabled_ui(prompt.step != 1, |ui| {
            detail_controls(ui, &mut prompt.radius, &mut prompt.block_points);
        });
        ui.horizontal(|ui| {
            let button = ui.button(t("Open"));
            crate::control::record_widget(ui, &button, "Open", "button", None);
            open = button.clicked();
            let button = ui.button(t("Cancel"));
            crate::control::record_widget(ui, &button, "Cancel", "button", None);
            cancel = button.clicked();
        });
    });
    if (open || cancel || modal.should_close())
        && let Some(path) = app.opening.clouds.pop_front()
    {
        let choice = (prompt.step, prompt.radius, prompt.block_points);
        app.opening.restore_focus = prompt.previous_focus;
        app.opening.cloud_prompt = None;
        if open {
            let (step, radius, block_points) = choice;
            let params = serde_json::json!({"previewStep":step, "detailRadiusFactor":radius, "blockPoints":block_points});
            match app.run("pointcloud.settings", params) {
                Ok(_) => app.opening.start_with_preview(&path, step),
                Err(error) => app.open_failed(error),
            }
        }
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use cadcraft_engine::Session;

    use crate::{CadApp, Services};

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn point_cloud_open_takes_focus_and_escape_returns_without_reading() {
        use egui_kittest::kittest::Queryable;

        let app = CadApp::new(Session::new(), Services::default());
        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            |ui, app: &mut CadApp| {
                if ui.button("Choose file").clicked() {
                    app.open_path(Path::new("cloud.laz"));
                }
                super::cloud_prompt(app, ui.ctx());
            },
            app,
        );
        let opener = harness.get_by_label("Choose file");
        opener.focus();
        harness.run();
        harness.key_press(egui::Key::Enter);
        harness.run();

        assert!(harness.get_by_role_and_label(egui::Role::Slider, "Preview interval").is_focused());
        assert!(!harness.state().opening.busy(), "the read must wait for a choice");
        harness.key_press(egui::Key::Escape);
        harness.run();
        assert!(harness.state().opening.clouds.is_empty());
        assert!(!harness.state().opening.busy());
        assert!(harness.get_by_label("Choose file").is_focused());
    }

    /// Opening starts in the background (the UI is not blocked), shows the card, and a file that
    /// cannot be read is reported like a failed OPEN without adding a drawing.
    #[test]
    fn open_path_reads_in_the_background_and_reports_failure() {
        let mut app = CadApp::new(Session::new(), Services::default());
        let before = app.session.docs.len();
        let missing = std::env::temp_dir().join(format!("cadcraft-missing-{}.dxf", std::process::id()));
        let p = missing.to_string_lossy().to_string();
        app.open_path(&missing);
        assert!(app.opening.busy() || app.status.is_some(), "the read starts in the background");
        let t = Instant::now();
        while app.opening.busy() && t.elapsed() < Duration::from_secs(10) {
            app.finish_opening();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.finish_opening();
        assert!(!app.opening.busy());
        assert_eq!(app.session.docs.len(), before);
        let (msg, _) = app.status.clone().unwrap_or_default();
        assert!(msg.contains(&p), "status reports the file: {msg}");
        assert!(app.session.log.iter().any(|l| l.contains(&p)));
    }

    #[test]
    fn card_text_names_the_file_and_counts_queued_ones() {
        let mut o = super::Opening::default();
        assert!(o.label().is_none());
        o.start(Path::new("/nonexistent/dir/a.dxf"));
        o.start(Path::new("/nonexistent/dir/b.dxf"));
        let l = o.label().unwrap_or_default();
        assert!(l.starts_with("Opening a.dxf and 1 more"), "{l}");
    }
}

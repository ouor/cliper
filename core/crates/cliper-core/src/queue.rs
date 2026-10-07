//! Batch queue: many VODs / recordings processed one after another, unattended
//! ("paste links, go to sleep, review in the morning").
//!
//! Each entry creates (or reopens) its project and runs, in order, the steps
//! still missing up to `through`: audio → detection → (optionally) clips.
//! Entries run strictly sequentially so the network and GPU are never shared.

use crate::engine::LockExt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::engine::{ApiError, Engine, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum QueueThrough {
    /// audio + song detection (then review in the app)
    #[default]
    Detect,
    /// … + video, separation and mute suggestions for every enabled song
    Clips,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum QueueStatus {
    Waiting,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct QueueEntry {
    pub id: String,
    pub input: String,
    pub through: QueueThrough,
    pub status: QueueStatus,
    /// set once the project exists
    pub project_id: Option<String>,
    /// step being run ("project.create" | "audio.fetch" | "detect.run" | "clips.fetch")
    pub step: Option<String>,
    /// job id of the running step (for job.* progress)
    pub job_id: Option<String>,
    pub error: Option<ApiError>,
    pub added_at: String,
    pub finished_at: Option<String>,
}

impl Engine {
    pub fn queue(&self) -> Vec<QueueEntry> {
        self.queue.lk().clone()
    }

    fn queue_changed(&self) {
        self.emit_event(Event::QueueChanged { queue: self.queue() });
    }

    fn queue_update(&self, id: &str, f: impl FnOnce(&mut QueueEntry)) {
        if let Some(e) = self.queue.lk().iter_mut().find(|e| e.id == id) {
            f(e);
        }
        self.queue_changed();
    }

    /// Append inputs (URLs / ids / local paths) and make sure the worker runs.
    pub fn queue_add(self: &Arc<Self>, inputs: Vec<String>, through: QueueThrough) -> Vec<QueueEntry> {
        let added: Vec<QueueEntry> = inputs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|input| QueueEntry {
                id: crate::project::new_id("q"),
                input,
                through,
                status: QueueStatus::Waiting,
                project_id: None,
                step: None,
                job_id: None,
                error: None,
                added_at: crate::project::now(),
                finished_at: None,
            })
            .collect();
        self.queue.lk().extend(added.iter().cloned());
        self.queue_changed();
        self.queue_wake.notify_one();
        self.ensure_queue_worker();
        added
    }

    /// Cancel a waiting entry, or the running one (cancels its current job).
    pub fn queue_cancel(&self, id: &str) -> bool {
        let mut job = None;
        let found = {
            let mut q = self.queue.lk();
            match q.iter_mut().find(|e| e.id == id) {
                Some(e) if e.status == QueueStatus::Waiting => {
                    e.status = QueueStatus::Cancelled;
                    true
                }
                Some(e) if e.status == QueueStatus::Running => {
                    job = e.job_id.clone();
                    e.status = QueueStatus::Cancelled;
                    true
                }
                _ => false,
            }
        };
        if let Some(j) = job {
            self.cancel(&j);
        }
        if found {
            self.queue_changed();
        }
        found
    }

    /// Remove finished / failed / cancelled entries.
    pub fn queue_clear(&self) {
        self.queue.lk().retain(|e| matches!(e.status, QueueStatus::Waiting | QueueStatus::Running));
        self.queue_changed();
    }

    fn ensure_queue_worker(self: &Arc<Self>) {
        if self.queue_worker.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let eng = self.clone();
        tokio::spawn(async move {
            loop {
                let next = {
                    let q = eng.queue.lk();
                    q.iter().find(|e| e.status == QueueStatus::Waiting).map(|e| (e.id.clone(), e.input.clone(), e.through))
                };
                let Some((qid, input, through)) = next else {
                    eng.queue_wake.notified().await;
                    continue;
                };
                eng.queue_update(&qid, |e| e.status = QueueStatus::Running);
                let res = eng.run_queue_entry(&qid, &input, through).await;
                let cancelled = eng.queue.lk().iter().any(|e| e.id == qid && e.status == QueueStatus::Cancelled);
                eng.queue_update(&qid, |e| {
                    e.step = None;
                    e.job_id = None;
                    e.finished_at = Some(crate::project::now());
                    match (&res, cancelled) {
                        (_, true) => e.status = QueueStatus::Cancelled,
                        (Ok(()), _) => e.status = QueueStatus::Done,
                        (Err(err), _) => {
                            e.status = QueueStatus::Failed;
                            e.error = Some(err.clone());
                        }
                    }
                });
            }
        });
    }

    async fn run_queue_entry(self: &Arc<Self>, qid: &str, input: &str, through: QueueThrough) -> Result<(), ApiError> {
        let is_cancelled = || self.queue.lk().iter().any(|e| e.id == qid && e.status == QueueStatus::Cancelled);
        self.queue_update(qid, |e| e.step = Some("project.create".into()));
        let p = self.create(input).await?;
        self.queue_update(qid, |e| e.project_id = Some(p.id.clone()));
        if is_cancelled() {
            return Ok(());
        }

        // record the job, then re-check: a cancel that arrived while the job was
        // being started has to reach the job
        let step = |name: &str, job: String| {
            let (name, jc) = (name.to_string(), job.clone());
            self.queue_update(qid, move |e| {
                e.step = Some(name);
                e.job_id = Some(jc);
            });
            if is_cancelled() {
                self.cancel(&job);
            }
        };
        if p.audio.is_none() {
            let j = self.job_fetch_audio(&p.id)?;
            step("audio.fetch", j.clone());
            self.wait_job(&j).await?;
        }
        if is_cancelled() {
            return Ok(());
        }
        if self.get(&p.id)?.detection.is_none() {
            let j = self.job_detect(&p.id, None, false)?;
            step("detect.run", j.clone());
            self.wait_job(&j).await?;
        }
        if is_cancelled() || through == QueueThrough::Detect {
            return Ok(());
        }
        let p = self.get(&p.id)?;
        if p.clips.iter().any(|c| c.video.is_none() || c.stems.is_none()) {
            let j = self.job_fetch_clips(&p.id, None, None, false).await?;
            step("clips.fetch", j.clone());
            self.wait_job(&j).await?;
        }
        Ok(())
    }
}

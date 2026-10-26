//! The seam between a transcript and the app it renders for.
//!
//! A transcript reads live facts (devices, upload progress, delivery state)
//! and starts engine-backed loads (blobs, images, visuals, delivery retry).
//! Phase 1 keeps that behind an object-safe host so the surface can later run
//! without `AppState`; [`AppStateHost`] is the current, only implementation.

use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};

use crate::attachments::LoadedAttachmentImage;
use crate::state::{AppState, Indicator};
use crate::tool_images::Loader;
use crate::transcript_view::{TranscriptInput, TranscriptSnapshot};
use crate::visuals::VisualFrame;
use crate::workspace_links::FileLinkRoot;

/// Everything a transcript needs from its app. Object-safe: no generics and
/// no `Self: Sized` methods.
pub(crate) trait TranscriptHost {
    fn link_roots_revision(&self, cx: &App) -> u64;
    fn file_link_roots(&self, chat_id: &str, cx: &App) -> Vec<FileLinkRoot>;
    fn chat_is_local(&self, chat_id: &str, cx: &App) -> bool;
    /// The checkout a chat's file paths resolve against; falls back to the
    /// selected chat when `chat_id` has no row.
    fn workspace_root(&self, chat_id: Option<&str>, cx: &App) -> Option<SharedString>;
    /// Owning device + cwd for tool-chip image previews.
    fn image_host(&self, chat_id: Option<&str>, cx: &App) -> (Option<String>, Option<String>);
    /// Devices that may own a sent message's attachment files: the selected
    /// chat's device plus this device.
    fn attachment_device_ids(&self, cx: &App) -> Vec<String>;
    fn local_device_id(&self, cx: &App) -> Option<String>;
    /// Whether engine-backed loads can run; callers keep their no-engine
    /// fallbacks (placeholder / error thumb) exactly as before.
    fn engine_available(&self, cx: &App) -> bool;
    fn upload_progress_percent(&self, cx: &App) -> Option<u8>;
    fn transfer_percent(&self, upload_id: &str, cx: &App) -> Option<u8>;
    fn send_undelivered(&self, chat_id: &str, now: DateTime<Utc>, cx: &App) -> bool;
    fn pending_send_started(
        &self,
        chat_id: &str,
        now: DateTime<Utc>,
        cx: &App,
    ) -> Option<DateTime<Utc>>;
    fn chat_delivery_degraded(&self, chat_id: &str, cx: &App) -> bool;
    fn session_started_at(&self, chat_id: &str, cx: &App) -> Option<DateTime<Utc>>;
    fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>, cx: &App) -> Indicator;
    /// The trailing subagent-doc entry, if any.
    fn doc_trailer(&self, doc_id: &str, cx: &App) -> Option<DocTrailer>;
    fn last_assistant_id(&self, cx: &App) -> Option<String>;
    /// Materialize the full entry snapshot on demand — compact-mode flips,
    /// fold rebuilds and route exit. Never a normal streaming revision.
    fn snapshot(&self, _doc_override: Option<&str>, _cx: &App) -> Option<TranscriptSnapshot> {
        None
    }
    /// Re-kick every delivery road and restart the failed-send grace clock.
    fn retry_send(&self, chat_id: &str, now: DateTime<Utc>, cx: &mut App);

    /// Fetch a tool sidecar blob's text, `None` when unavailable.
    fn fetch_tool_blob(&self, blob_ref: SharedString, cx: &AsyncApp) -> Task<Option<String>>;
    /// Read one attachment image from its owning device.
    fn read_attachment_image(
        &self,
        device_id: String,
        path: String,
        expected_raster_mime: Option<String>,
        cx: &AsyncApp,
    ) -> Task<Option<LoadedAttachmentImage>>;
    /// Read an image's original bytes from `device` (`(name, mime, bytes)`).
    fn read_attachment_bytes(
        &self,
        device: &str,
        path: &str,
        cx: &AsyncApp,
    ) -> Task<Option<(String, String, Vec<u8>)>>;
    /// Where tool-chip image previews load from.
    fn tool_image_loader(&self, cx: &App) -> Loader;
    /// Build the view for one inline agent visual.
    fn visual_frame(
        &self,
        id: SharedString,
        title: String,
        path: String,
        owner: &str,
        cx: &mut App,
    ) -> Entity<VisualFrame>;

    /// The backing app state, for test fixtures that drive a bare
    /// `TranscriptView` without the shell-facing handle.
    #[cfg(test)]
    fn state_entity(&self) -> Option<&Entity<AppState>> {
        None
    }
}

pub(crate) struct AppStateHost {
    state: Entity<AppState>,
}

/// The facts a subagent doc's working trailer reads off its last entry.
pub(crate) struct DocTrailer {
    pub id: String,
    pub status: Option<zeron_doc::MessageStatus>,
    pub role: zeron_doc::MessageRole,
    pub created_at: i64,
}

impl AppStateHost {
    pub(crate) fn new(state: Entity<AppState>) -> Self {
        Self { state }
    }
}

impl TranscriptHost for AppStateHost {
    fn link_roots_revision(&self, cx: &App) -> u64 {
        self.state.read(cx).link_roots_revision
    }

    fn file_link_roots(&self, chat_id: &str, cx: &App) -> Vec<FileLinkRoot> {
        self.state.read(cx).file_link_roots(chat_id)
    }

    fn chat_is_local(&self, chat_id: &str, cx: &App) -> bool {
        self.state.read(cx).chat_is_local(chat_id)
    }

    fn workspace_root(&self, chat_id: Option<&str>, cx: &App) -> Option<SharedString> {
        let state = self.state.read(cx);
        chat_id
            .and_then(|chat_id| state.chats.iter().find(|chat| chat.id == chat_id))
            .or_else(|| state.selected_chat_row())
            .and_then(|chat| chat.cwd.as_deref())
            .map(SharedString::from)
    }

    fn image_host(&self, chat_id: Option<&str>, cx: &App) -> (Option<String>, Option<String>) {
        let state = self.state.read(cx);
        let chat = chat_id
            .and_then(|id| state.chats.iter().find(|chat| chat.id == id))
            .or_else(|| state.selected_chat_row());
        (
            chat.map(|chat| chat.device_id.clone()),
            chat.and_then(|chat| chat.cwd.clone()),
        )
    }

    fn attachment_device_ids(&self, cx: &App) -> Vec<String> {
        let state = self.state.read(cx);
        let mut ids = Vec::new();
        if let Some(chat) = state.selected_chat_row() {
            ids.push(chat.device_id.clone());
        }
        if let Some(local) = state.local_device_id.clone()
            && !ids.contains(&local)
        {
            ids.push(local);
        }
        ids
    }

    fn local_device_id(&self, cx: &App) -> Option<String> {
        self.state.read(cx).local_device_id.clone()
    }

    fn engine_available(&self, cx: &App) -> bool {
        self.state.read(cx).engine().is_some()
    }

    fn upload_progress_percent(&self, cx: &App) -> Option<u8> {
        self.state.read(cx).upload_progress_percent()
    }

    fn transfer_percent(&self, upload_id: &str, cx: &App) -> Option<u8> {
        self.state.read(cx).transfer_percent(upload_id)
    }

    fn send_undelivered(&self, chat_id: &str, now: DateTime<Utc>, cx: &App) -> bool {
        self.state.read(cx).send_undelivered(chat_id, now)
    }

    fn pending_send_started(
        &self,
        chat_id: &str,
        now: DateTime<Utc>,
        cx: &App,
    ) -> Option<DateTime<Utc>> {
        self.state.read(cx).pending_send_started(chat_id, now)
    }

    fn chat_delivery_degraded(&self, chat_id: &str, cx: &App) -> bool {
        self.state.read(cx).chat_delivery_degraded(chat_id)
    }

    fn session_started_at(&self, chat_id: &str, cx: &App) -> Option<DateTime<Utc>> {
        self.state
            .read(cx)
            .session_for(chat_id)
            .and_then(|session| session.started_at)
    }

    fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>, cx: &App) -> Indicator {
        self.state.read(cx).indicator_for(chat_id, now)
    }

    fn doc_trailer(&self, doc_id: &str, cx: &App) -> Option<DocTrailer> {
        self.state
            .read(cx)
            .sub_transcript(doc_id)
            .last()
            .map(|entry| DocTrailer {
                id: entry.id.clone(),
                status: entry.status,
                role: entry.role,
                created_at: entry.created_at,
            })
    }

    fn last_assistant_id(&self, cx: &App) -> Option<String> {
        self.state
            .read(cx)
            .transcript
            .iter()
            .rev()
            .find(|entry| entry.role == zeron_doc::MessageRole::Assistant)
            .map(|entry| entry.id.clone())
    }

    fn snapshot(&self, doc_override: Option<&str>, cx: &App) -> Option<TranscriptSnapshot> {
        Some(TranscriptInput::new(&self.state.read(cx), doc_override).into_snapshot())
    }

    fn retry_send(&self, chat_id: &str, now: DateTime<Utc>, cx: &mut App) {
        let engine = self.state.read(cx).engine().cloned();
        self.state.update(cx, |state, cx| {
            state.retry_pending_send(chat_id, now);
            cx.notify();
        });
        if let Some(engine) = engine {
            let chat_id = chat_id.to_owned();
            cx.background_executor()
                .spawn(async move {
                    let params = serde_json::json!({ "chatId": chat_id });
                    if let Err(err) = engine
                        .client()
                        .call(zeron_rpc::methods::RETRY_DELIVERY, params)
                        .await
                    {
                        tracing::warn!(error = %err, "delivery retry RPC failed");
                    }
                })
                .detach();
        }
    }

    fn fetch_tool_blob(&self, blob_ref: SharedString, cx: &AsyncApp) -> Task<Option<String>> {
        let engine = self.state.read_with(cx, |state, _| state.engine().cloned());
        let Some(engine) = engine else {
            return Task::ready(None);
        };
        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            crate::attachments::call_with_timeout(
                &engine,
                &executor,
                zeron_rpc::methods::FETCH_TOOL_BLOB,
                serde_json::json!({ "blobRef": blob_ref.as_ref() }),
                Duration::from_secs(20),
            )
            .await
            .ok()
            .map(|value| {
                value
                    .get("text")
                    .and_then(|text| text.as_str())
                    .unwrap_or_default()
                    .to_owned()
            })
        })
    }

    fn read_attachment_image(
        &self,
        device_id: String,
        path: String,
        expected_raster_mime: Option<String>,
        cx: &AsyncApp,
    ) -> Task<Option<LoadedAttachmentImage>> {
        let (engine, local) = self.state.read_with(cx, |state, _| {
            (state.engine().cloned(), state.local_device_id.clone())
        });
        let Some(engine) = engine else {
            return Task::ready(None);
        };
        // Relay-forward only for a genuinely remote owner; the local device's
        // files are served directly.
        let target = (local.as_deref() != Some(device_id.as_str())).then(|| device_id.clone());
        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            crate::attachments::read_attachment_image(
                &engine,
                &executor,
                target.as_deref(),
                &path,
                expected_raster_mime.as_deref(),
            )
            .await
        })
    }

    fn read_attachment_bytes(
        &self,
        device: &str,
        path: &str,
        cx: &AsyncApp,
    ) -> Task<Option<(String, String, Vec<u8>)>> {
        let (engine, local) = self.state.read_with(cx, |state, _| {
            (state.engine().cloned(), state.local_device_id.clone())
        });
        let Some(engine) = engine else {
            return Task::ready(None);
        };
        let target = (local.as_deref() != Some(device)).then(|| device.to_owned());
        let path = path.to_owned();
        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            crate::attachments::read_attachment_bytes(
                &engine,
                &executor,
                target.as_deref(),
                &path,
                None,
            )
            .await
        })
    }

    fn tool_image_loader(&self, cx: &App) -> Loader {
        let state = self.state.read(cx);
        Loader {
            engine: state.engine().cloned(),
            local_device: state.local_device_id.clone(),
        }
    }

    fn visual_frame(
        &self,
        id: SharedString,
        title: String,
        path: String,
        owner: &str,
        cx: &mut App,
    ) -> Entity<VisualFrame> {
        let state = self.state.read(cx);
        let engine = state.engine().cloned();
        // Relay-forward only for a genuinely remote owner.
        let target = (state.local_device_id.as_deref() != Some(owner)).then(|| owner.to_owned());
        cx.new(|cx| VisualFrame::new(id, title, path, engine, target, cx))
    }

    #[cfg(test)]
    fn state_entity(&self) -> Option<&Entity<AppState>> {
        Some(&self.state)
    }
}

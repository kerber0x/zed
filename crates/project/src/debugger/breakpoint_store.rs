//! Module for managing breakpoints in a project.
//!
//! Breakpoints are separate from a session because they're not associated with any particular debug session. They can also be set up without a session running.
use anyhow::{Result, anyhow};
use breakpoints_in_file::BreakpointsInFile;
use collections::BTreeMap;
use dap::client::SessionId;
use gpui::{App, AppContext, AsyncApp, Context, Entity, EventEmitter, Subscription, Task};
use itertools::Itertools;
use language::{Buffer, BufferSnapshot, proto::serialize_anchor as serialize_text_anchor};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self},
};
use std::{hash::Hash, ops::Range, path::Path, sync::Arc};
use text::{Point, PointUtf16};

use crate::{Project, ProjectPath, buffer_store::BufferStore, worktree_store::WorktreeStore};

mod breakpoints_in_file {
    use language::BufferEvent;

    use super::*;

    #[derive(Clone)]
    pub(super) struct BreakpointsInFile {
        pub(super) buffer: Entity<Buffer>,
        // TODO: This is.. less than ideal, as it's O(n) and does not return entries in order. We'll have to change TreeMap to support passing in the context for comparisons
        pub(super) breakpoints: Vec<(text::Anchor, Breakpoint)>,
        _subscription: Arc<Subscription>,
    }

    impl BreakpointsInFile {
        pub(super) fn new(buffer: Entity<Buffer>, cx: &mut Context<BreakpointStore>) -> Self {
            let subscription =
                Arc::from(cx.subscribe(&buffer, |_, buffer, event, cx| match event {
                    BufferEvent::Saved => {
                        if let Some(abs_path) = BreakpointStore::abs_path_from_buffer(&buffer, cx) {
                            cx.emit(BreakpointStoreEvent::BreakpointsUpdated(
                                abs_path,
                                BreakpointUpdatedReason::FileSaved,
                            ));
                        }
                    }
                    _ => {}
                }));

            BreakpointsInFile {
                buffer,
                breakpoints: Vec::new(),
                _subscription: subscription,
            }
        }
    }
}

#[derive(Clone)]
struct RemoteBreakpointStore {
    upstream_client: AnyProtoClient,
    _upstream_project_id: u64,
}

#[derive(Clone)]
struct LocalBreakpointStore {
    worktree_store: Entity<WorktreeStore>,
    buffer_store: Entity<BufferStore>,
}

#[derive(Clone)]
enum BreakpointStoreMode {
    Local(LocalBreakpointStore),
    Remote(RemoteBreakpointStore),
}
pub struct BreakpointStore {
    breakpoints: BTreeMap<Arc<Path>, BreakpointsInFile>,
    downstream_client: Option<(AnyProtoClient, u64)>,
    active_stack_frame: Option<(SessionId, Arc<Path>, text::Anchor)>,
    // E.g ssh
    mode: BreakpointStoreMode,
}

impl BreakpointStore {
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_toggle_breakpoint);
        client.add_entity_message_handler(Self::handle_breakpoints_for_file);
    }
    pub fn local(worktree_store: Entity<WorktreeStore>, buffer_store: Entity<BufferStore>) -> Self {
        BreakpointStore {
            breakpoints: BTreeMap::new(),
            mode: BreakpointStoreMode::Local(LocalBreakpointStore {
                worktree_store,
                buffer_store,
            }),
            downstream_client: None,
            active_stack_frame: Default::default(),
        }
    }

    pub(crate) fn remote(upstream_project_id: u64, upstream_client: AnyProtoClient) -> Self {
        BreakpointStore {
            breakpoints: BTreeMap::new(),
            mode: BreakpointStoreMode::Remote(RemoteBreakpointStore {
                upstream_client,
                _upstream_project_id: upstream_project_id,
            }),
            downstream_client: None,
            active_stack_frame: Default::default(),
        }
    }

    pub(crate) fn shared(&mut self, project_id: u64, downstream_client: AnyProtoClient) {
        self.downstream_client = Some((downstream_client.clone(), project_id));
    }

    pub(crate) fn unshared(&mut self, cx: &mut Context<Self>) {
        self.downstream_client.take();

        cx.notify();
    }

    async fn handle_breakpoints_for_file(
        this: Entity<Project>,
        message: TypedEnvelope<proto::BreakpointsForFile>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let breakpoints = cx.update(|cx| this.read(cx).breakpoint_store())?;
        if message.payload.breakpoints.is_empty() {
            return Ok(());
        }

        let buffer = this
            .update(&mut cx, |this, cx| {
                let path =
                    this.project_path_for_absolute_path(message.payload.path.as_ref(), cx)?;
                Some(this.open_buffer(path, cx))
            })
            .ok()
            .flatten()
            .ok_or_else(|| anyhow!("Invalid project path"))?
            .await?;

        breakpoints.update(&mut cx, move |this, cx| {
            let bps = this
                .breakpoints
                .entry(Arc::<Path>::from(message.payload.path.as_ref()))
                .or_insert_with(|| BreakpointsInFile::new(buffer, cx));

            bps.breakpoints = message
                .payload
                .breakpoints
                .into_iter()
                .filter_map(|breakpoint| {
                    let anchor = language::proto::deserialize_anchor(breakpoint.position.clone()?)?;
                    let breakpoint = Breakpoint::from_proto(breakpoint)?;
                    Some((anchor, breakpoint))
                })
                .collect();

            cx.notify();
        })?;

        Ok(())
    }

    async fn handle_toggle_breakpoint(
        this: Entity<Project>,
        message: TypedEnvelope<proto::ToggleBreakpoint>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let breakpoints = this.update(&mut cx, |this, _| this.breakpoint_store())?;
        let path = this
            .update(&mut cx, |this, cx| {
                this.project_path_for_absolute_path(message.payload.path.as_ref(), cx)
            })?
            .ok_or_else(|| anyhow!("Could not resolve provided abs path"))?;
        let buffer = this
            .update(&mut cx, |this, cx| {
                this.buffer_store().read(cx).get_by_path(&path, cx)
            })?
            .ok_or_else(|| anyhow!("Could not find buffer for a given path"))?;
        let breakpoint = message
            .payload
            .breakpoint
            .ok_or_else(|| anyhow!("Breakpoint not present in RPC payload"))?;
        let anchor = language::proto::deserialize_anchor(
            breakpoint
                .position
                .clone()
                .ok_or_else(|| anyhow!("Anchor not present in RPC payload"))?,
        )
        .ok_or_else(|| anyhow!("Anchor deserialization failed"))?;
        let breakpoint = Breakpoint::from_proto(breakpoint)
            .ok_or_else(|| anyhow!("Could not deserialize breakpoint"))?;

        breakpoints.update(&mut cx, |this, cx| {
            this.toggle_breakpoint(
                buffer,
                (anchor, breakpoint),
                BreakpointEditAction::Toggle,
                cx,
            );
        })?;
        Ok(proto::Ack {})
    }

    pub(crate) fn broadcast(&self) {
        if let Some((client, project_id)) = &self.downstream_client {
            for (path, breakpoint_set) in &self.breakpoints {
                let _ = client.send(proto::BreakpointsForFile {
                    project_id: *project_id,
                    path: path.to_str().map(ToOwned::to_owned).unwrap(),
                    breakpoints: breakpoint_set
                        .breakpoints
                        .iter()
                        .filter_map(|(anchor, bp)| bp.to_proto(&path, anchor))
                        .collect(),
                });
            }
        }
    }

    pub fn abs_path_from_buffer(buffer: &Entity<Buffer>, cx: &App) -> Option<Arc<Path>> {
        worktree::File::from_dyn(buffer.read(cx).file())
            .and_then(|file| file.worktree.read(cx).absolutize(&file.path).ok())
            .map(Arc::<Path>::from)
    }

    pub fn toggle_breakpoint(
        &mut self,
        buffer: Entity<Buffer>,
        mut breakpoint: (text::Anchor, Breakpoint),
        edit_action: BreakpointEditAction,
        cx: &mut Context<Self>,
    ) {
        let Some(abs_path) = Self::abs_path_from_buffer(&buffer, cx) else {
            return;
        };

        let breakpoint_set = self
            .breakpoints
            .entry(abs_path.clone())
            .or_insert_with(|| BreakpointsInFile::new(buffer, cx));

        match edit_action {
            BreakpointEditAction::Toggle => {
                let len_before = breakpoint_set.breakpoints.len();
                breakpoint_set
                    .breakpoints
                    .retain(|value| &breakpoint != value);
                if len_before == breakpoint_set.breakpoints.len() {
                    // We did not remove any breakpoint, hence let's toggle one.
                    breakpoint_set.breakpoints.push(breakpoint.clone());
                }
            }
            BreakpointEditAction::InvertState => {
                if let Some((_, bp)) = breakpoint_set
                    .breakpoints
                    .iter_mut()
                    .find(|value| breakpoint == **value)
                {
                    if bp.is_enabled() {
                        bp.state = BreakpointState::Disabled;
                    } else {
                        bp.state = BreakpointState::Enabled;
                    }
                } else {
                    breakpoint.1.state = BreakpointState::Disabled;
                    breakpoint_set.breakpoints.push(breakpoint.clone());
                }
            }
            BreakpointEditAction::EditLogMessage(log_message) => {
                if !log_message.is_empty() {
                    let found_bp =
                        breakpoint_set
                            .breakpoints
                            .iter_mut()
                            .find_map(|(other_pos, other_bp)| {
                                if breakpoint.0 == *other_pos {
                                    Some(other_bp)
                                } else {
                                    None
                                }
                            });

                    if let Some(found_bp) = found_bp {
                        found_bp.message = Some(log_message.clone());
                    } else {
                        breakpoint.1.message = Some(log_message.clone());
                        // We did not remove any breakpoint, hence let's toggle one.
                        breakpoint_set.breakpoints.push(breakpoint.clone());
                    }
                } else if breakpoint.1.message.is_some() {
                    if let Some(position) = breakpoint_set
                        .breakpoints
                        .iter()
                        .find_position(|(pos, bp)| &breakpoint.0 == pos && bp == &breakpoint.1)
                        .map(|res| res.0)
                    {
                        breakpoint_set.breakpoints.remove(position);
                    } else {
                        log::error!("Failed to find position of breakpoint to delete")
                    }
                }
            }
            BreakpointEditAction::EditHitCondition(hit_condition) => {
                if !hit_condition.is_empty() {
                    let found_bp =
                        breakpoint_set
                            .breakpoints
                            .iter_mut()
                            .find_map(|(other_pos, other_bp)| {
                                if breakpoint.0 == *other_pos {
                                    Some(other_bp)
                                } else {
                                    None
                                }
                            });

                    if let Some(found_bp) = found_bp {
                        found_bp.hit_condition = Some(hit_condition.clone());
                    } else {
                        breakpoint.1.hit_condition = Some(hit_condition.clone());
                        // We did not remove any breakpoint, hence let's toggle one.
                        breakpoint_set.breakpoints.push(breakpoint.clone());
                    }
                } else if breakpoint.1.hit_condition.is_some() {
                    if let Some(position) = breakpoint_set
                        .breakpoints
                        .iter()
                        .find_position(|(pos, bp)| &breakpoint.0 == pos && bp == &breakpoint.1)
                        .map(|res| res.0)
                    {
                        breakpoint_set.breakpoints.remove(position);
                    } else {
                        log::error!("Failed to find position of breakpoint to delete")
                    }
                }
            }
            BreakpointEditAction::EditCondition(condition) => {
                if !condition.is_empty() {
                    let found_bp =
                        breakpoint_set
                            .breakpoints
                            .iter_mut()
                            .find_map(|(other_pos, other_bp)| {
                                if breakpoint.0 == *other_pos {
                                    Some(other_bp)
                                } else {
                                    None
                                }
                            });

                    if let Some(found_bp) = found_bp {
                        found_bp.condition = Some(condition.clone());
                    } else {
                        breakpoint.1.condition = Some(condition.clone());
                        // We did not remove any breakpoint, hence let's toggle one.
                        breakpoint_set.breakpoints.push(breakpoint.clone());
                    }
                } else if breakpoint.1.condition.is_some() {
                    if let Some(position) = breakpoint_set
                        .breakpoints
                        .iter()
                        .find_position(|(pos, bp)| &breakpoint.0 == pos && bp == &breakpoint.1)
                        .map(|res| res.0)
                    {
                        breakpoint_set.breakpoints.remove(position);
                    } else {
                        log::error!("Failed to find position of breakpoint to delete")
                    }
                }
            }
        }

        if breakpoint_set.breakpoints.is_empty() {
            self.breakpoints.remove(&abs_path);
        }
        if let BreakpointStoreMode::Remote(remote) = &self.mode {
            if let Some(breakpoint) = breakpoint.1.to_proto(&abs_path, &breakpoint.0) {
                cx.background_spawn(remote.upstream_client.request(proto::ToggleBreakpoint {
                    project_id: remote._upstream_project_id,
                    path: abs_path.to_str().map(ToOwned::to_owned).unwrap(),
                    breakpoint: Some(breakpoint),
                }))
                .detach();
            }
        } else if let Some((client, project_id)) = &self.downstream_client {
            let breakpoints = self
                .breakpoints
                .get(&abs_path)
                .map(|breakpoint_set| {
                    breakpoint_set
                        .breakpoints
                        .iter()
                        .filter_map(|(anchor, bp)| bp.to_proto(&abs_path, anchor))
                        .collect()
                })
                .unwrap_or_default();

            let _ = client.send(proto::BreakpointsForFile {
                project_id: *project_id,
                path: abs_path.to_str().map(ToOwned::to_owned).unwrap(),
                breakpoints,
            });
        }

        cx.emit(BreakpointStoreEvent::BreakpointsUpdated(
            abs_path,
            BreakpointUpdatedReason::Toggled,
        ));
        cx.notify();
    }

    pub fn on_file_rename(
        &mut self,
        old_path: Arc<Path>,
        new_path: Arc<Path>,
        cx: &mut Context<Self>,
    ) {
        if let Some(breakpoints) = self.breakpoints.remove(&old_path) {
            self.breakpoints.insert(new_path.clone(), breakpoints);

            cx.notify();
        }
    }

    pub fn clear_breakpoints(&mut self, cx: &mut Context<Self>) {
        let breakpoint_paths = self.breakpoints.keys().cloned().collect();
        self.breakpoints.clear();
        cx.emit(BreakpointStoreEvent::BreakpointsCleared(breakpoint_paths));
    }

    pub fn breakpoints<'a>(
        &'a self,
        buffer: &'a Entity<Buffer>,
        range: Option<Range<text::Anchor>>,
        buffer_snapshot: &'a BufferSnapshot,
        cx: &App,
    ) -> impl Iterator<Item = &'a (text::Anchor, Breakpoint)> + 'a {
        let abs_path = Self::abs_path_from_buffer(buffer, cx);
        abs_path
            .and_then(|path| self.breakpoints.get(&path))
            .into_iter()
            .flat_map(move |file_breakpoints| {
                file_breakpoints.breakpoints.iter().filter({
                    let range = range.clone();
                    move |(position, _)| {
                        if let Some(range) = &range {
                            position.cmp(&range.start, buffer_snapshot).is_ge()
                                && position.cmp(&range.end, buffer_snapshot).is_le()
                        } else {
                            true
                        }
                    }
                })
            })
    }

    pub fn active_position(&self) -> Option<&(SessionId, Arc<Path>, text::Anchor)> {
        self.active_stack_frame.as_ref()
    }

    pub fn remove_active_position(
        &mut self,
        session_id: Option<SessionId>,
        cx: &mut Context<Self>,
    ) {
        if let Some(session_id) = session_id {
            self.active_stack_frame
                .take_if(|(id, _, _)| *id == session_id);
        } else {
            self.active_stack_frame.take();
        }

        cx.emit(BreakpointStoreEvent::ActiveDebugLineChanged);
        cx.notify();
    }

    pub fn set_active_position(
        &mut self,
        position: (SessionId, Arc<Path>, text::Anchor),
        cx: &mut Context<Self>,
    ) {
        self.active_stack_frame = Some(position);
        cx.emit(BreakpointStoreEvent::ActiveDebugLineChanged);
        cx.notify();
    }

    pub fn breakpoint_at_row(
        &self,
        path: &Path,
        row: u32,
        cx: &App,
    ) -> Option<(Entity<Buffer>, (text::Anchor, Breakpoint))> {
        self.breakpoints.get(path).and_then(|breakpoints| {
            let snapshot = breakpoints.buffer.read(cx).text_snapshot();

            breakpoints
                .breakpoints
                .iter()
                .find(|(anchor, _)| anchor.summary::<Point>(&snapshot).row == row)
                .map(|breakpoint| (breakpoints.buffer.clone(), breakpoint.clone()))
        })
    }

    pub fn breakpoints_from_path(&self, path: &Arc<Path>, cx: &App) -> Vec<SourceBreakpoint> {
        self.breakpoints
            .get(path)
            .map(|bp| {
                let snapshot = bp.buffer.read(cx).snapshot();
                bp.breakpoints
                    .iter()
                    .map(|(position, breakpoint)| {
                        let position = snapshot.summary_for_anchor::<PointUtf16>(position).row;
                        SourceBreakpoint {
                            row: position,
                            path: path.clone(),
                            state: breakpoint.state,
                            message: breakpoint.message.clone(),
                            condition: breakpoint.condition.clone(),
                            hit_condition: breakpoint.hit_condition.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn all_breakpoints(&self, cx: &App) -> BTreeMap<Arc<Path>, Vec<SourceBreakpoint>> {
        self.breakpoints
            .iter()
            .map(|(path, bp)| {
                let snapshot = bp.buffer.read(cx).snapshot();
                (
                    path.clone(),
                    bp.breakpoints
                        .iter()
                        .map(|(position, breakpoint)| {
                            let position = snapshot.summary_for_anchor::<PointUtf16>(position).row;
                            SourceBreakpoint {
                                row: position,
                                path: path.clone(),
                                message: breakpoint.message.clone(),
                                state: breakpoint.state,
                                hit_condition: breakpoint.hit_condition.clone(),
                                condition: breakpoint.condition.clone(),
                            }
                        })
                        .collect(),
                )
            })
            .collect()
    }

    pub fn with_serialized_breakpoints(
        &self,
        breakpoints: BTreeMap<Arc<Path>, Vec<SourceBreakpoint>>,
        cx: &mut Context<BreakpointStore>,
    ) -> Task<Result<()>> {
        if let BreakpointStoreMode::Local(mode) = &self.mode {
            let mode = mode.clone();
            cx.spawn(async move |this, cx| {
                let mut new_breakpoints = BTreeMap::default();
                for (path, bps) in breakpoints {
                    if bps.is_empty() {
                        continue;
                    }
                    let (worktree, relative_path) = mode
                        .worktree_store
                        .update(cx, |this, cx| {
                            this.find_or_create_worktree(&path, false, cx)
                        })?
                        .await?;
                    let buffer = mode
                        .buffer_store
                        .update(cx, |this, cx| {
                            let path = ProjectPath {
                                worktree_id: worktree.read(cx).id(),
                                path: relative_path.into(),
                            };
                            this.open_buffer(path, cx)
                        })?
                        .await;
                    let Ok(buffer) = buffer else {
                        log::error!("Todo: Serialized breakpoints which do not have buffer (yet)");
                        continue;
                    };
                    let snapshot = buffer.update(cx, |buffer, _| buffer.snapshot())?;

                    let mut breakpoints_for_file =
                        this.update(cx, |_, cx| BreakpointsInFile::new(buffer, cx))?;

                    for bp in bps {
                        let max_point = snapshot.max_point_utf16();
                        let point = PointUtf16::new(bp.row, 0);
                        if point > max_point {
                            log::error!("skipping a deserialized breakpoint that's out of range");
                            continue;
                        }
                        let position = snapshot.anchor_after(point);
                        breakpoints_for_file.breakpoints.push((
                            position,
                            Breakpoint {
                                message: bp.message,
                                state: bp.state,
                                condition: bp.condition,
                                hit_condition: bp.hit_condition,
                            },
                        ))
                    }
                    new_breakpoints.insert(path, breakpoints_for_file);
                }
                this.update(cx, |this, cx| {
                    log::info!("Finish deserializing breakpoints & initializing breakpoint store");
                    for (path, count) in new_breakpoints.iter().map(|(path, bp_in_file)| {
                        (path.to_string_lossy(), bp_in_file.breakpoints.len())
                    }) {
                        let breakpoint_str = if count > 1 {
                            "breakpoints"
                        } else {
                            "breakpoint"
                        };
                        log::info!("Deserialized {count} {breakpoint_str} at path: {path}");
                    }

                    this.breakpoints = new_breakpoints;

                    cx.notify();
                })?;

                Ok(())
            })
        } else {
            Task::ready(Ok(()))
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn breakpoint_paths(&self) -> Vec<Arc<Path>> {
        self.breakpoints.keys().cloned().collect()
    }
}

#[derive(Clone, Copy)]
pub enum BreakpointUpdatedReason {
    Toggled,
    FileSaved,
}

pub enum BreakpointStoreEvent {
    ActiveDebugLineChanged,
    BreakpointsUpdated(Arc<Path>, BreakpointUpdatedReason),
    BreakpointsCleared(Vec<Arc<Path>>),
}

impl EventEmitter<BreakpointStoreEvent> for BreakpointStore {}

type BreakpointMessage = Arc<str>;

#[derive(Clone, Debug)]
pub enum BreakpointEditAction {
    Toggle,
    InvertState,
    EditLogMessage(BreakpointMessage),
    EditCondition(BreakpointMessage),
    EditHitCondition(BreakpointMessage),
}

#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum BreakpointState {
    Enabled,
    Disabled,
}

impl BreakpointState {
    #[inline]
    pub fn is_enabled(&self) -> bool {
        matches!(self, BreakpointState::Enabled)
    }

    #[inline]
    pub fn is_disabled(&self) -> bool {
        matches!(self, BreakpointState::Disabled)
    }

    #[inline]
    pub fn to_int(&self) -> i32 {
        match self {
            BreakpointState::Enabled => 0,
            BreakpointState::Disabled => 1,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Breakpoint {
    pub message: Option<BreakpointMessage>,
    /// How many times do we hit the breakpoint until we actually stop at it e.g. (2 = 2 times of the breakpoint action)
    pub hit_condition: Option<BreakpointMessage>,
    pub condition: Option<BreakpointMessage>,
    pub state: BreakpointState,
}

impl Breakpoint {
    pub fn new_standard() -> Self {
        Self {
            state: BreakpointState::Enabled,
            hit_condition: None,
            condition: None,
            message: None,
        }
    }

    pub fn new_condition(hit_condition: &str) -> Self {
        Self {
            state: BreakpointState::Enabled,
            condition: None,
            hit_condition: Some(hit_condition.into()),
            message: None,
        }
    }

    pub fn new_log(log_message: &str) -> Self {
        Self {
            state: BreakpointState::Enabled,
            hit_condition: None,
            condition: None,
            message: Some(log_message.into()),
        }
    }

    fn to_proto(&self, _path: &Path, position: &text::Anchor) -> Option<client::proto::Breakpoint> {
        Some(client::proto::Breakpoint {
            position: Some(serialize_text_anchor(position)),
            state: match self.state {
                BreakpointState::Enabled => proto::BreakpointState::Enabled.into(),
                BreakpointState::Disabled => proto::BreakpointState::Disabled.into(),
            },
            message: self.message.as_ref().map(|s| String::from(s.as_ref())),
            condition: self.condition.as_ref().map(|s| String::from(s.as_ref())),
            hit_condition: self
                .hit_condition
                .as_ref()
                .map(|s| String::from(s.as_ref())),
        })
    }

    fn from_proto(breakpoint: client::proto::Breakpoint) -> Option<Self> {
        Some(Self {
            state: match proto::BreakpointState::from_i32(breakpoint.state) {
                Some(proto::BreakpointState::Disabled) => BreakpointState::Disabled,
                None | Some(proto::BreakpointState::Enabled) => BreakpointState::Enabled,
            },
            message: breakpoint.message.map(Into::into),
            condition: breakpoint.condition.map(Into::into),
            hit_condition: breakpoint.hit_condition.map(Into::into),
        })
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.state.is_enabled()
    }

    #[inline]
    pub fn is_disabled(&self) -> bool {
        self.state.is_disabled()
    }
}

/// Breakpoint for location within source code.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SourceBreakpoint {
    pub row: u32,
    pub path: Arc<Path>,
    pub message: Option<Arc<str>>,
    pub condition: Option<Arc<str>>,
    pub hit_condition: Option<Arc<str>>,
    pub state: BreakpointState,
}

impl From<SourceBreakpoint> for dap::SourceBreakpoint {
    fn from(bp: SourceBreakpoint) -> Self {
        Self {
            line: bp.row as u64 + 1,
            column: None,
            condition: bp
                .condition
                .map(|condition| String::from(condition.as_ref())),
            hit_condition: bp
                .hit_condition
                .map(|hit_condition| String::from(hit_condition.as_ref())),
            log_message: bp.message.map(|message| String::from(message.as_ref())),
            mode: None,
        }
    }
}

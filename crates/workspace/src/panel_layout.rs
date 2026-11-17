use crate::{Workspace, dock::DockPosition, pane_group::PaneRenderContext};
use gpui::{
    App, Bounds, Context, EntityId, Focusable, IntoElement, Point, Render, SharedString, Window,
    canvas,
};
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{cell::Cell, collections::HashMap, rc::Rc};
use ui::{Tooltip, prelude::*};
use util::ResultExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Region {
    Left,
    Right,
    Bottom,
    Editor,
    Pane(uuid::Uuid),
}

impl From<DockPosition> for Region {
    fn from(position: DockPosition) -> Self {
        match position {
            DockPosition::Left => Self::Left,
            DockPosition::Right => Self::Right,
            DockPosition::Bottom => Self::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn at(bounds: Bounds<Pixels>, point: Point<Pixels>) -> Self {
        let x = ((point.x - bounds.left()) / bounds.size.width.max(px(1.))).clamp(0., 1.);
        let y = ((point.y - bounds.top()) / bounds.size.height.max(px(1.))).clamp(0., 1.);
        if x.min(1. - x) < y.min(1. - y) {
            if x < 0.5 { Self::Left } else { Self::Right }
        } else if y < 0.5 {
            Self::Top
        } else {
            Self::Bottom
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Layout {
    Region(Region),
    Split {
        vertical: bool,
        ratio: f32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        editor_collapsed: bool,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    fn split(vertical: bool, ratio: f32, first: Self, second: Self) -> Self {
        Self::Split {
            vertical,
            ratio,
            editor_collapsed: false,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub fn initial(bottom: settings::BottomDockLayout, filling: Option<DockPosition>) -> Self {
        use Region::*;
        let leaf = Self::Region;
        let center_right = Self::split(false, 0.5, leaf(Editor), leaf(Right));
        let middle = Self::split(false, 0.23, leaf(Left), center_right.clone());
        if filling.is_some() || bottom == settings::BottomDockLayout::Full {
            return Self::split(true, 0.72, middle, leaf(Bottom));
        }
        match bottom {
            settings::BottomDockLayout::RightAligned => Self::split(
                false,
                0.23,
                leaf(Left),
                Self::split(true, 0.72, center_right, leaf(Bottom)),
            ),
            settings::BottomDockLayout::LeftAligned => Self::split(
                false,
                0.7,
                Self::split(
                    true,
                    0.72,
                    Self::split(false, 0.3, leaf(Left), leaf(Editor)),
                    leaf(Bottom),
                ),
                leaf(Right),
            ),
            _ => Self::split(
                false,
                0.23,
                leaf(Left),
                Self::split(
                    false,
                    0.65,
                    Self::split(true, 0.72, leaf(Editor), leaf(Bottom)),
                    leaf(Right),
                ),
            ),
        }
    }

    fn fit_bounds(&mut self, regions: &HashMap<Region, Bounds<Pixels>>) -> Option<Bounds<Pixels>> {
        match self {
            Self::Region(region) => regions.get(region).copied(),
            Self::Split {
                vertical,
                ratio,
                first,
                second,
                ..
            } => {
                let first = first.fit_bounds(regions);
                let second = second.fit_bounds(regions);
                match (first, second) {
                    (Some(first), Some(second)) => {
                        let left = first.left().min(second.left());
                        let top = first.top().min(second.top());
                        let right = first.right().max(second.right());
                        let bottom = first.bottom().max(second.bottom());
                        let extent = if *vertical {
                            bottom - top
                        } else {
                            right - left
                        };
                        let first_extent = if *vertical {
                            first.size.height
                        } else {
                            first.size.width
                        };
                        if extent > px(0.) {
                            *ratio = (first_extent / extent).clamp(0.01, 0.99);
                        }
                        Some(Bounds::new(
                            gpui::point(left, top),
                            gpui::size(right - left, bottom - top),
                        ))
                    }
                    (first, second) => first.or(second),
                }
            }
        }
    }

    fn has_panes(&self) -> bool {
        match self {
            Self::Region(Region::Pane(_)) => true,
            Self::Region(_) => false,
            Self::Split { first, second, .. } => first.has_panes() || second.has_panes(),
        }
    }

    fn regions(&self, regions: &mut Vec<Region>) {
        match self {
            Self::Region(region) => regions.push(*region),
            Self::Split { first, second, .. } => {
                first.regions(regions);
                second.regions(regions);
            }
        }
    }

    fn replace_region(&mut self, region: Region, replacement: &Layout) {
        match self {
            Self::Region(value) if *value == region => *self = replacement.clone(),
            Self::Split { first, second, .. } => {
                first.replace_region(region, replacement);
                second.replace_region(region, replacement);
            }
            _ => {}
        }
    }

    fn from_panes(member: &crate::pane_group::Member, cx: &App) -> Option<Self> {
        match member {
            crate::pane_group::Member::Pane(pane) => pane
                .read(cx)
                .layout_id
                .map(|id| Self::Region(Region::Pane(id))),
            crate::pane_group::Member::Axis(axis) => {
                let flexes = axis.flexes.lock();
                let mut children = axis
                    .members
                    .iter()
                    .enumerate()
                    .filter_map(|(index, member)| {
                        Self::from_panes(member, cx).map(|layout| {
                            (layout, flexes.get(index).copied().unwrap_or(1.).max(0.01))
                        })
                    });
                let (mut layout, mut weight) = children.next()?;
                for (next, next_weight) in children {
                    layout = Self::split(
                        axis.axis == gpui::Axis::Vertical,
                        weight / (weight + next_weight),
                        layout,
                        next,
                    );
                    weight += next_weight;
                }
                Some(layout)
            }
        }
    }

    fn contains(&self, region: Region) -> bool {
        match self {
            Self::Region(value) => *value == region,
            Self::Split { first, second, .. } => first.contains(region) || second.contains(region),
        }
    }

    fn without(self, region: Region) -> Option<Self> {
        match self {
            Self::Region(value) => (value != region).then_some(Self::Region(value)),
            Self::Split {
                vertical,
                ratio,
                first,
                second,
                ..
            } => match (first.without(region), second.without(region)) {
                (Some(first), Some(second)) => Some(Self::split(vertical, ratio, first, second)),
                (first, second) => first.or(second),
            },
        }
    }

    pub fn move_region(&self, source: Region, target: Region, edge: Edge) -> Option<Self> {
        if source == target || !self.contains(source) || !self.contains(target) {
            return None;
        }
        let mut layout = self.clone().without(source)?;
        layout.insert(source, target, edge);
        Some(layout)
    }

    fn insert(&mut self, source: Region, target: Region, edge: Edge) {
        match self {
            Self::Region(value) if *value == target => {
                let source = Self::Region(source);
                let target = Self::Region(target);
                let vertical = matches!(edge, Edge::Top | Edge::Bottom);
                *self = if matches!(edge, Edge::Top | Edge::Left) {
                    Self::split(vertical, 0.5, source, target)
                } else {
                    Self::split(vertical, 0.5, target, source)
                };
            }
            Self::Split { first, second, .. } => {
                first.insert(source, target, edge);
                second.insert(source, target, edge);
            }
            _ => {}
        }
    }

    fn visible(&self, regions: &HashMap<Region, AnyElement>) -> bool {
        match self {
            Self::Region(region) => regions.contains_key(region),
            Self::Split { first, second, .. } => first.visible(regions) || second.visible(regions),
        }
    }

    fn sole_visible_region(&self, regions: &HashMap<Region, AnyElement>) -> Option<Region> {
        let mut visible = regions
            .keys()
            .copied()
            .filter(|region| self.contains(*region));
        let region = visible.next()?;
        if visible.next().is_some() {
            return None;
        }
        Some(region)
    }

    fn resize_split(&mut self, path: &[bool], ratio: f32, collapse_editor: bool) {
        if let Self::Split {
            ratio: value,
            editor_collapsed,
            first,
            second,
            ..
        } = self
        {
            if let Some((next, rest)) = path.split_first() {
                if *next { second } else { first }.resize_split(rest, ratio, collapse_editor);
            } else {
                *value = ratio;
                *editor_collapsed = collapse_editor;
            }
        }
    }

    fn collapse_editor(&mut self, visible: &[Region]) -> bool {
        let Self::Split {
            editor_collapsed,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        let only_editor = |branch: &Layout| {
            branch.contains(Region::Editor)
                && !visible
                    .iter()
                    .any(|region| *region != Region::Editor && branch.contains(*region))
        };
        let visible_branch =
            |branch: &Layout| visible.iter().any(|region| branch.contains(*region));
        if (only_editor(first) && visible_branch(second))
            || (only_editor(second) && visible_branch(first))
        {
            *editor_collapsed = true;
            true
        } else {
            first.collapse_editor(visible) || second.collapse_editor(visible)
        }
    }

    pub(super) fn reveal_editor(&mut self) -> bool {
        match self {
            Self::Region(_) => false,
            Self::Split {
                editor_collapsed,
                first,
                second,
                ..
            } => std::mem::take(editor_collapsed) | first.reveal_editor() | second.reveal_editor(),
        }
    }

    fn valid(&self) -> bool {
        fn collect(layout: &Layout, regions: &mut Vec<Region>, depth: usize) -> bool {
            if depth > 128 {
                return false;
            }
            match layout {
                Layout::Region(region) => {
                    regions.push(*region);
                    true
                }
                Layout::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    ratio.is_finite()
                        && (0.0..1.0).contains(ratio)
                        && *ratio > 0.
                        && collect(first, regions, depth + 1)
                        && collect(second, regions, depth + 1)
                }
            }
        }
        let mut regions = Vec::new();
        collect(self, &mut regions, 0)
            && regions
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == regions.len()
            && [Region::Left, Region::Right, Region::Bottom]
                .iter()
                .all(|region| regions.contains(region))
            && regions
                .iter()
                .any(|region| matches!(region, Region::Editor | Region::Pane(_)))
    }
}

#[derive(Clone)]
pub struct DraggedRegion {
    pub workspace: EntityId,
    pub region: Region,
    pub(crate) pane: Option<gpui::WeakEntity<crate::Pane>>,
}

impl DraggedRegion {
    fn resolved_region(&self, cx: &App) -> Region {
        self.pane
            .as_ref()
            .and_then(|pane| pane.upgrade())
            .and_then(|pane| pane.read(cx).layout_id)
            .map(Region::Pane)
            .unwrap_or(self.region)
    }
}

impl Render for DraggedRegion {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(cx.theme().colors().element_background)
            .child(Label::new("Move panel"))
    }
}

pub fn drag_handle(workspace: EntityId, region: Region) -> impl IntoElement {
    div()
        .id("move-panel")
        .debug_selector(|| "panel-drag-handle".into())
        .cursor(gpui::CursorStyle::OpenHand)
        .tooltip(Tooltip::text("Drag to move panel"))
        .on_drag(
            DraggedRegion {
                workspace,
                region,
                pane: None,
            },
            |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            },
        )
        .child(ui::IconButton::new("move-panel-icon", IconName::Menu).icon_size(IconSize::Small))
}

impl Workspace {
    pub(crate) fn record_panel_split(
        &mut self,
        source: &gpui::Entity<crate::Pane>,
        destination: &gpui::Entity<crate::Pane>,
        direction: crate::SplitDirection,
        cx: &mut App,
    ) {
        self.selected_pane = Some(destination.downgrade());
        self.agent_mode_files_visible = true;
        let Some(layout) = self
            .panel_layout
            .as_mut()
            .filter(|layout| layout.has_panes())
        else {
            return;
        };
        let Some(source_id) = source.read(cx).layout_id else {
            return;
        };
        let destination_id = destination.update(cx, |pane, _| {
            *pane.layout_id.get_or_insert_with(uuid::Uuid::new_v4)
        });
        let edge = match direction {
            crate::SplitDirection::Up => Edge::Top,
            crate::SplitDirection::Down => Edge::Bottom,
            crate::SplitDirection::Left => Edge::Left,
            crate::SplitDirection::Right => Edge::Right,
        };
        layout.insert(Region::Pane(destination_id), Region::Pane(source_id), edge);
        layout.reveal_editor();
        self.save_panel_layout(cx);
    }

    pub(crate) fn materialize_panel_panes(&mut self, cx: &mut App) {
        for pane in &self.panes {
            pane.update(cx, |pane, _| {
                pane.layout_id.get_or_insert_with(uuid::Uuid::new_v4);
            });
        }
        let center = Layout::from_panes(&self.center.root, cx);
        let layout = self.panel_layout.get_or_insert_with(|| {
            let mut layout = Layout::initial(
                crate::WorkspaceSettings::get_global(cx).bottom_dock_layout,
                None,
            );
            layout.fit_bounds(&self.panel_region_bounds.borrow());
            layout
        });
        if let Some(center) = center {
            layout.replace_region(Region::Editor, &center);
        }
    }

    pub(crate) fn sync_panel_panes(&mut self, cx: &mut App) {
        if self.restoring_workspace
            || self.panel_layout_pending_restore
            || !self.panel_layout.as_ref().is_some_and(Layout::has_panes)
        {
            return;
        }
        for pane in &self.panes {
            pane.update(cx, |pane, _| {
                pane.layout_id.get_or_insert_with(uuid::Uuid::new_v4);
            });
        }
        let live = self
            .panes
            .iter()
            .filter_map(|pane| pane.read(cx).layout_id)
            .map(Region::Pane)
            .collect::<Vec<_>>();
        let Some(mut layout) = self.panel_layout.take() else {
            return;
        };
        let mut leaves = Vec::new();
        layout.regions(&mut leaves);
        for region in leaves
            .iter()
            .filter(|region| matches!(region, Region::Pane(_)) && !live.contains(region))
        {
            if let Some(pruned) = layout.clone().without(*region) {
                layout = pruned;
            }
        }
        for region in live {
            if !layout.contains(region) {
                let mut existing = Vec::new();
                layout.regions(&mut existing);
                let target = existing
                    .iter()
                    .find(|region| matches!(region, Region::Pane(_) | Region::Editor))
                    .copied()
                    .unwrap_or(Region::Right);
                layout.insert(region, target, Edge::Right);
            }
        }
        self.panel_layout = Some(layout);
    }

    pub fn detach_item_to_group(
        &mut self,
        source: &gpui::Entity<crate::Pane>,
        item_id: EntityId,
        target: &gpui::Entity<crate::Pane>,
        direction: crate::SplitDirection,
        preserve_source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.materialize_panel_panes(cx);
        let Some(target_region) = self.panel_drag_source(target, cx).map(|drag| drag.region) else {
            return;
        };
        self.detach_item_to_region(
            source,
            item_id,
            target_region,
            direction,
            preserve_source,
            window,
            cx,
        );
    }

    fn detach_item_to_region(
        &mut self,
        source: &gpui::Entity<crate::Pane>,
        item_id: EntityId,
        mut target_region: Region,
        direction: crate::SplitDirection,
        preserve_source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = source
            .read(cx)
            .items()
            .find(|item| item.item_id() == item_id)
            .cloned()
        else {
            return;
        };
        let pinned = source
            .read(cx)
            .items()
            .position(|item| item.item_id() == item_id)
            .is_some_and(|index| index < source.read(cx).pinned_count());
        self.materialize_panel_panes(cx);
        if target_region == Region::Editor {
            let Some(id) = self.panes.first().and_then(|pane| pane.read(cx).layout_id) else {
                return;
            };
            target_region = Region::Pane(id);
        }
        let preserve_source = preserve_source
            || self
                .panel_drag_source(source, cx)
                .is_some_and(|drag| drag.region == target_region);
        self.dismiss_zoomed_items_to_reveal(None, window, cx);
        let Some(center_target) = self.panes.first().cloned() else {
            return;
        };
        let reuse_empty_center = !matches!(target_region, Region::Pane(_))
            && self.panes.len() == 1
            && center_target.read(cx).items_len() == 0;
        let destination = if reuse_empty_center {
            center_target.clone()
        } else {
            self.add_pane(window, cx)
        };
        let id = destination.update(cx, |pane, _| {
            *pane.layout_id.get_or_insert_with(uuid::Uuid::new_v4)
        });
        if reuse_empty_center {
            self.panel_layout = self
                .panel_layout
                .take()
                .and_then(|layout| layout.without(Region::Pane(id)));
        } else {
            self.center
                .split_group(&center_target, &destination, direction, cx);
        }
        let edge = match direction {
            crate::SplitDirection::Up => Edge::Top,
            crate::SplitDirection::Down => Edge::Bottom,
            crate::SplitDirection::Left => Edge::Left,
            crate::SplitDirection::Right => Edge::Right,
        };
        if let Some(layout) = &mut self.panel_layout {
            layout.insert(Region::Pane(id), target_region, edge);
            layout.reveal_editor();
        }
        source.update(cx, |pane, cx| {
            pane.remove_item(item_id, false, !preserve_source, window, cx)
        });
        destination.update(cx, |pane, cx| {
            pane.add_item(item, true, true, None, window, cx);
            if pinned {
                pane.set_pinned_count(1);
            }
        });
        self.agent_mode_files_visible = true;
        self.save_panel_layout(cx);
        window.focus(&destination.focus_handle(cx), cx);
        self.serialize_workspace(window, cx);
        cx.notify();
    }

    pub(crate) fn reset_panel_layout(
        &mut self,
        _: &crate::ResetPanelLayout,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel_layout = None;
        self.panel_layout_pending_restore = false;
        self.panel_drop = None;
        if let Some(key) = self.panel_layout_key.clone() {
            let kvp = db::kvp::KeyValueStore::global(cx);
            cx.background_spawn(async move { kvp.scoped("panel_layout").delete(key).await })
                .detach_and_log_err(cx);
        }
        cx.notify();
    }
    pub fn has_custom_panel_layout(&self) -> bool {
        self.panel_layout.is_some()
    }

    pub fn place_dock_above(
        &mut self,
        upper: DockPosition,
        lower: DockPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = self
            .panel_layout
            .clone()
            .unwrap_or_else(|| Layout::initial(settings::BottomDockLayout::RightAligned, None));
        if let Some(layout) = layout.move_region(upper.into(), lower.into(), Edge::Top) {
            self.dismiss_zoomed_items_to_reveal(None, window, cx);
            self.panel_layout = Some(layout);
            self.save_panel_layout(cx);
            cx.notify();
        }
    }

    pub fn panel_drag_source(
        &self,
        pane: &gpui::Entity<crate::Pane>,
        cx: &App,
    ) -> Option<DraggedRegion> {
        self.panel_drag_source_with_id(pane, pane.read(cx).layout_id, cx)
    }

    pub(crate) fn panel_drag_source_with_id(
        &self,
        pane: &gpui::Entity<crate::Pane>,
        layout_id: Option<uuid::Uuid>,
        cx: &App,
    ) -> Option<DraggedRegion> {
        let region = if self
            .center
            .panes()
            .iter()
            .any(|candidate| *candidate == pane)
        {
            layout_id
                .filter(|_| self.panel_layout.as_ref().is_some_and(Layout::has_panes))
                .map(Region::Pane)
                .unwrap_or(Region::Editor)
        } else {
            self.all_docks().into_iter().find_map(|dock| {
                let dock = dock.read(cx);
                dock.active_panel()
                    .filter(|panel| panel.panes(cx).contains(pane))
                    .map(|_| Region::from(dock.position()))
            })?
        };
        Some(DraggedRegion {
            workspace: self.weak_self.entity_id(),
            region,
            pane: None,
        })
    }
    pub(crate) fn load_panel_layout(&mut self, cx: &App) {
        let key = self
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(self.session_id());
        if key == self.panel_layout_key {
            return;
        }
        self.panel_layout_key = key.clone();
        if let Some(key) = key {
            self.panel_layout = db::kvp::KeyValueStore::global(cx)
                .scoped("panel_layout")
                .read(&key)
                .log_err()
                .flatten()
                .and_then(|value| serde_json::from_str::<Layout>(&value).log_err())
                .filter(Layout::valid);
            // The first frames still contain the placeholder center pane. Do not
            // prune or persist saved pane IDs until session restoration finishes.
            let mut regions = Vec::new();
            if let Some(layout) = &self.panel_layout {
                layout.regions(&mut regions);
            }
            self.panel_layout_pending_restore = regions.iter().any(|region| match region {
                Region::Pane(id) => !self
                    .panes
                    .iter()
                    .any(|pane| pane.read(cx).layout_id == Some(*id)),
                _ => false,
            });
        }
    }

    pub(super) fn save_panel_layout(&self, cx: &mut App) {
        if self.restoring_workspace || self.panel_layout_pending_restore {
            return;
        }
        let Some(key) = self.panel_layout_key.clone() else {
            return;
        };
        let Some(layout) = &self.panel_layout else {
            return;
        };
        let Some(value) = serde_json::to_string(layout).log_err() else {
            return;
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        cx.background_spawn(async move { kvp.scoped("panel_layout").write(key, value).await })
            .detach_and_log_err(cx);
    }

    pub(crate) fn panel_drop_target(
        &self,
        region: Region,
        element: impl IntoElement,
        cx: &App,
    ) -> AnyElement {
        let workspace = self.weak_self.clone();
        let owner = workspace.entity_id();
        let dropping = workspace.clone();
        let tab_hover = workspace.clone();
        let tab_drop = workspace.clone();
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let measured = bounds.clone();
        let region_bounds = self.panel_region_bounds.clone();
        let bounds_for_tab = bounds.clone();
        div()
            .id(SharedString::from(format!("panel-region-{region:?}")))
            .debug_selector(move || format!("panel-region-{region:?}"))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                canvas(
                    move |rect, _, _| {
                        measured.set(rect);
                        region_bounds.borrow_mut().insert(region, rect);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(element)
            .can_drop(move |value, _, cx| {
                value
                    .downcast_ref::<crate::pane::DetachedTab>()
                    .is_some_and(|drag| drag.1 == owner)
                    || value.downcast_ref::<DraggedRegion>().is_some_and(|drag| {
                        drag.workspace == owner && drag.resolved_region(cx) != region
                    })
            })
            .on_drag_move::<crate::pane::DetachedTab>(move |event, _, cx| {
                if !event.bounds.contains(&event.event.position) {
                    return;
                }
                let edge = Edge::at(event.bounds, event.event.position);
                tab_hover
                    .update(cx, |workspace, cx| {
                        if workspace.panel_drop != Some((region, edge)) {
                            workspace.panel_drop = Some((region, edge));
                            cx.notify();
                        }
                    })
                    .log_err();
            })
            .on_drop(move |drag: &crate::pane::DetachedTab, window, cx| {
                if drag.1 != owner {
                    return;
                }
                let source = drag.0.pane.clone();
                let item = drag.0.item.item_id();
                let edge = Edge::at(bounds_for_tab.get(), window.mouse_position());
                let direction = match edge {
                    Edge::Left => crate::SplitDirection::Left,
                    Edge::Right => crate::SplitDirection::Right,
                    Edge::Top => crate::SplitDirection::Up,
                    Edge::Bottom => crate::SplitDirection::Down,
                };
                let workspace = tab_drop.clone();
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.panel_drop = None;
                            workspace.detach_item_to_region(
                                &source, item, region, direction, false, window, cx,
                            );
                        })
                        .log_err();
                });
                cx.stop_propagation();
            })
            .on_drag_move::<DraggedRegion>(move |event, _, cx| {
                if event.drag(cx).workspace != owner || event.drag(cx).resolved_region(cx) == region
                {
                    return;
                }
                if !event.bounds.contains(&event.event.position) {
                    return;
                }
                let edge = Edge::at(event.bounds, event.event.position);
                workspace
                    .update(cx, |workspace, cx| {
                        if workspace.panel_drop != Some((region, edge)) {
                            workspace.panel_drop = Some((region, edge));
                            cx.notify();
                        }
                    })
                    .log_err();
            })
            .on_drop(move |drag: &DraggedRegion, window, cx| {
                let source = drag.resolved_region(cx);
                if drag.workspace != owner || source == region {
                    return;
                }
                let edge = Edge::at(bounds.get(), window.mouse_position());
                dropping
                    .update(cx, |workspace, cx| {
                        workspace.panel_drop = None;
                        let layout = workspace.panel_layout.clone().unwrap_or_else(|| {
                            Layout::initial(
                                crate::WorkspaceSettings::get_global(cx).bottom_dock_layout,
                                workspace.filling_dock(cx),
                            )
                        });
                        if let Some(layout) = layout.move_region(source, region, edge) {
                            workspace.panel_layout = Some(layout);
                            workspace.save_panel_layout(cx);
                            cx.notify();
                        }
                    })
                    .log_err();
                cx.stop_propagation();
            })
            .when_some(
                self.panel_drop
                    .filter(|(target, _)| *target == region && cx.has_active_drag()),
                |element, (_, edge)| {
                    element.child(
                        div()
                            .absolute()
                            .inset_0()
                            .invisible()
                            .drag_over::<DraggedRegion>(|style, _, _, _| style.visible())
                            .drag_over::<crate::pane::DetachedTab>(|style, _, _, _| style.visible())
                            .child(
                                div()
                                    .absolute()
                                    .bg(cx.theme().colors().drop_target_background)
                                    .border_2()
                                    .border_color(cx.theme().colors().drop_target_border)
                                    .when(matches!(edge, Edge::Left | Edge::Right), |preview| {
                                        preview.top_0().h_full().w(relative(0.5))
                                    })
                                    .when(matches!(edge, Edge::Top | Edge::Bottom), |preview| {
                                        preview.left_0().w_full().h(relative(0.5))
                                    })
                                    .when(edge == Edge::Left, |preview| preview.left_0())
                                    .when(edge == Edge::Right, |preview| preview.right_0())
                                    .when(edge == Edge::Top, |preview| preview.top_0())
                                    .when(edge == Edge::Bottom, |preview| preview.bottom_0()),
                            ),
                    )
                },
            )
            .into_any_element()
    }

    pub(crate) fn collapse_closed_center_pane(
        &mut self,
        pane: &gpui::Entity<crate::Pane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The root editor pane must remain available for reopening files, even when hidden.
        if self.panes.len() != 1
            || self.panes.first() != Some(pane)
            || pane.read(cx).items_len() != 0
        {
            return;
        }
        let visible_docks = self
            .all_docks()
            .into_iter()
            .cloned()
            .filter(|dock| dock.read(cx).is_open() && dock.read(cx).visible_panel().is_some())
            .collect::<Vec<_>>();
        if visible_docks.is_empty() {
            return;
        }
        self.load_panel_layout(cx);
        let default_layout = Layout::initial(
            crate::WorkspaceSettings::get_global(cx).bottom_dock_layout,
            self.filling_dock(cx),
        );
        let layout = self.panel_layout.get_or_insert(default_layout);
        let mut visible = vec![Region::Editor];
        visible.extend(
            visible_docks
                .iter()
                .map(|dock| Region::from(dock.read(cx).position())),
        );
        if layout.collapse_editor(&visible) {
            if self.zoomed_position.is_none() {
                self.zoomed = None;
                pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                cx.emit(crate::Event::ZoomChanged);
            }
            if let Some(dock) = visible_docks
                .iter()
                .find(|dock| dock.read(cx).position() == DockPosition::Right)
                .or(visible_docks.first())
            {
                dock.focus_handle(cx).focus(window, cx);
            }
            self.save_panel_layout(cx);
            self.serialize_workspace(window, cx);
            cx.notify();
        }
    }

    pub(crate) fn render_panel_layout(
        &self,
        layout: &Layout,
        render_cx: &PaneRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let mut regions = HashMap::new();
        let mut hidden = Vec::new();
        for (position, dock) in [
            (DockPosition::Left, &self.left_dock),
            (DockPosition::Right, &self.right_dock),
            (DockPosition::Bottom, &self.bottom_dock),
        ] {
            if let Some(element) = self.render_dock(position, dock, window, cx) {
                if dock.read(cx).is_open() {
                    regions.insert(Region::from(position), element.into_any_element());
                } else {
                    hidden.push(element.into_any_element());
                }
            }
        }
        if layout.has_panes() {
            self.center.outer_bounds.lock().clear();
            for pane in &self.panes {
                let Some(id) = pane.read(cx).layout_id else {
                    continue;
                };
                let region = Region::Pane(id);
                let content = crate::pane_group::Member::Pane(pane.clone())
                    .render(0, self.zoomed.as_ref(), None, render_cx, window, cx)
                    .element;
                let content = self.frame_pane(content, cx);
                let bounds = self.center.outer_bounds.clone();
                let entity_id = pane.entity_id();
                let content = div()
                    .relative()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        canvas(
                            move |rect, _, _| {
                                bounds.lock().insert(entity_id, rect);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(content);
                regions.insert(region, self.panel_drop_target(region, content, cx));
            }
        } else if self.filling_dock(cx).is_none() {
            regions.insert(
                Region::Editor,
                self.render_center(render_cx, window, cx).into_any_element(),
            );
        }
        let content = self.render_panel_branch(layout, Vec::new(), &mut regions, cx);
        let workspace = self.weak_self.clone();
        let save_workspace = workspace.clone();
        let owner = workspace.entity_id();
        div()
            .id("custom-panel-layout")
            .size_full()
            .min_w_0()
            .min_h_0()
            .relative()
            .child(content)
            .child(div().absolute().size_0().overflow_hidden().children(hidden))
            .on_drag_move::<DraggedDivider>(move |event, window, cx| {
                let drag = event.drag(cx).clone();
                if drag.workspace != owner {
                    return;
                }
                workspace
                    .update(cx, |workspace, cx| {
                        drag.resize(workspace, event.event.position, window, cx);
                    })
                    .log_err();
            })
            .on_drop(move |drag: &DraggedDivider, window, cx| {
                if drag.workspace != owner {
                    return;
                }
                save_workspace
                    .update(cx, |workspace, cx| {
                        drag.resize(workspace, window.mouse_position(), window, cx);
                        workspace.save_panel_layout(cx);
                    })
                    .log_err();
                cx.stop_propagation();
            })
            .into_any_element()
    }

    fn render_panel_branch(
        &self,
        layout: &Layout,
        path: Vec<bool>,
        regions: &mut HashMap<Region, AnyElement>,
        cx: &App,
    ) -> AnyElement {
        match layout {
            Layout::Region(region) => regions
                .remove(region)
                .unwrap_or_else(|| div().size_0().into_any_element()),
            Layout::Split {
                vertical,
                ratio,
                editor_collapsed,
                first,
                second,
            } => {
                let mut first_path = path.clone();
                first_path.push(false);
                let mut second_path = path.clone();
                second_path.push(true);
                let first_editor = first.sole_visible_region(regions) == Some(Region::Editor);
                let second_editor = second.sole_visible_region(regions) == Some(Region::Editor);
                let first_visible = first.visible(regions);
                let second_visible = second.visible(regions);
                // Keep a draggable edge at this split only for docks collapsed by dragging.
                // Looking up the split in the tree also handles repositioned docks.
                let resizable_dock = |branch: &Layout, visible| {
                    if visible {
                        match branch.sole_visible_region(regions)? {
                            Region::Left => Some(DockPosition::Left),
                            Region::Right => Some(DockPosition::Right),
                            Region::Bottom => Some(DockPosition::Bottom),
                            Region::Editor | Region::Pane(_) => None,
                        }
                    } else {
                        self.all_docks().into_iter().find_map(|dock| {
                            let dock = dock.read(cx);
                            (dock.collapsed_by_drag() && branch.contains(dock.position().into()))
                                .then_some(dock.position())
                        })
                    }
                };
                let first_dock = resizable_dock(first, first_visible);
                let second_dock = resizable_dock(second, second_visible);
                if !first_visible && first_dock.is_none() {
                    return self.render_panel_branch(second, second_path, regions, cx);
                }
                if !second_visible && second_dock.is_none() {
                    return self.render_panel_branch(first, first_path, regions, cx);
                }
                // Keep the divider as a hit target, but unmount the collapsed editor.
                let (first_visible, second_visible) = (
                    first_visible && !(*editor_collapsed && first_editor && second_visible),
                    second_visible && !(*editor_collapsed && second_editor && first_visible),
                );
                let first =
                    first_visible.then(|| self.render_panel_branch(first, first_path, regions, cx));
                let second = second_visible
                    .then(|| self.render_panel_branch(second, second_path, regions, cx));
                let vertical = *vertical;
                let original_ratio = *ratio;
                let ratio = if !first_visible {
                    0.
                } else if !second_visible {
                    1.
                } else {
                    original_ratio
                };
                let at_edge = !first_visible || !second_visible;
                let grip = crate::dock::RESIZE_HANDLE_SIZE;
                let grip_offset = if !first_visible {
                    px(0.)
                } else if !second_visible {
                    -grip
                } else {
                    -grip / 2.
                };
                let framed = crate::WorkspaceSettings::get_global(cx).pane_layout
                    != settings::PaneLayout::Flat;
                let bounds = Rc::new(Cell::new(Bounds::default()));
                let measured = bounds.clone();
                div()
                    .flex()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .relative()
                    .when(vertical, |row| row.flex_col())
                    .child(
                        canvas(move |rect, _, _| measured.set(rect), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .child(
                        div()
                            .flex_grow(ratio)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .children(first),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .relative()
                            .when(vertical, |divider| {
                                divider
                                    .h(px(if framed || at_edge { 0. } else { 1. }))
                                    .w_full()
                            })
                            .when(!vertical, |divider| {
                                divider
                                    .w(px(if framed || at_edge { 0. } else { 1. }))
                                    .h_full()
                            }),
                    )
                    .child(
                        div()
                            .flex_grow(1. - ratio)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .children(second),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("panel-split-{path:?}")))
                            .absolute()
                            .when(vertical, |divider| {
                                divider
                                    .top(relative(ratio))
                                    .mt(grip_offset)
                                    .h(grip)
                                    .w_full()
                                    .cursor_row_resize()
                            })
                            .when(!vertical, |divider| {
                                divider
                                    .left(relative(ratio))
                                    .ml(grip_offset)
                                    .w(grip)
                                    .h_full()
                                    .cursor_col_resize()
                            })
                            .on_drag(
                                DraggedDivider {
                                    workspace: self.weak_self.entity_id(),
                                    path: path.clone(),
                                    first_dock,
                                    second_dock,
                                    first_editor,
                                    second_editor,
                                    original_ratio,
                                    vertical,
                                    bounds,
                                },
                                |drag, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.new(|_| drag.clone())
                                },
                            )
                            .map(|handle| {
                                // The parent split can share this edge with the hidden editor.
                                if *editor_collapsed && at_edge {
                                    gpui::deferred(handle).into_any_element()
                                } else {
                                    handle.into_any_element()
                                }
                            }),
                    )
                    .into_any_element()
            }
        }
    }
}

#[derive(Clone)]
struct DraggedDivider {
    first_editor: bool,
    second_editor: bool,
    first_dock: Option<DockPosition>,
    second_dock: Option<DockPosition>,
    original_ratio: f32,
    workspace: EntityId,
    path: Vec<bool>,
    vertical: bool,
    bounds: Rc<Cell<Bounds<Pixels>>>,
}
impl DraggedDivider {
    fn resize(
        &self,
        workspace: &mut Workspace,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let bounds = self.bounds.get();
        let (offset, extent) = if self.vertical {
            (position.y - bounds.top(), bounds.size.height)
        } else {
            (position.x - bounds.left(), bounds.size.width)
        };
        if extent <= px(0.) {
            return;
        }
        let close = if offset <= crate::dock::RESIZE_HANDLE_SIZE {
            self.first_dock
        } else if extent - offset <= crate::dock::RESIZE_HANDLE_SIZE {
            self.second_dock
        } else {
            None
        };
        let Some(layout) = &mut workspace.panel_layout else {
            return;
        };
        let collapse_editor = (self.first_editor && offset <= crate::dock::RESIZE_HANDLE_SIZE)
            || (self.second_editor && extent - offset <= crate::dock::RESIZE_HANDLE_SIZE);
        let ratio = (offset / extent).clamp(
            if self.first_dock.is_some() || self.first_editor {
                0.
            } else {
                0.1
            },
            if self.second_dock.is_some() || self.second_editor {
                1.
            } else {
                0.9
            },
        );
        layout.resize_split(
            &self.path,
            if close.is_some() || collapse_editor {
                self.original_ratio
            } else {
                ratio
            },
            collapse_editor,
        );
        let mut visibility_changed = false;
        for position in [self.first_dock, self.second_dock].into_iter().flatten() {
            let dock = workspace.dock_at_position(position);
            if Some(position) == close && dock.read(cx).is_open() {
                dock.update(cx, |dock, cx| dock.collapse_by_drag(window, cx));
                visibility_changed = true;
            } else if Some(position) != close && dock.read(cx).collapsed_by_drag() {
                dock.update(cx, |dock, cx| dock.set_open(true, window, cx));
                visibility_changed = true;
            }
        }
        if visibility_changed {
            workspace.serialize_workspace(window, cx);
        }
        cx.notify();
    }
}
impl Render for DraggedDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::{PanelEvent, test::TestPanel};
    use gpui::{Entity, Modifiers, MouseButton, TestAppContext, point};

    struct LayoutTestView {
        workspace: Entity<Workspace>,
    }
    impl Render for LayoutTestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .relative()
                .child(self.workspace.clone())
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .child(drag_handle(self.workspace.entity_id(), Region::Bottom)),
                )
        }
    }

    #[gpui::test]
    async fn custom_pane_splits_follow_direction_and_click_selects_destination(
        cx: &mut TestAppContext,
    ) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let first = workspace.update(cx, |workspace, cx| {
            workspace.materialize_panel_panes(cx);
            workspace.active_pane.clone()
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        for direction in [
            crate::SplitDirection::Down,
            crate::SplitDirection::Up,
            crate::SplitDirection::Left,
            crate::SplitDirection::Right,
        ] {
            first.update_in(cx, |pane, window, cx| {
                pane.split(direction, crate::pane::SplitMode::EmptyPane, window, cx)
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let destination =
                workspace.read_with(cx, |workspace, _| workspace.panes.last().unwrap().clone());
            let bounds = |pane: &Entity<crate::Pane>, cx: &mut gpui::VisualTestContext| {
                let id = pane.read_with(cx, |pane, _| pane.layout_id.unwrap());
                let selector: &'static str =
                    Box::leak(format!("panel-region-{:?}", Region::Pane(id)).into_boxed_str());
                cx.debug_bounds(selector).unwrap()
            };
            let source_bounds = bounds(&first, cx);
            let destination_bounds = bounds(&destination, cx);
            match direction {
                crate::SplitDirection::Down => {
                    assert!(destination_bounds.top() >= source_bounds.bottom())
                }
                crate::SplitDirection::Up => {
                    assert!(destination_bounds.bottom() <= source_bounds.top())
                }
                crate::SplitDirection::Left => {
                    assert!(destination_bounds.right() <= source_bounds.left())
                }
                crate::SplitDirection::Right => {
                    assert!(destination_bounds.left() >= source_bounds.right())
                }
            }
            cx.simulate_click(destination_bounds.center(), Modifiers::default());
            cx.run_until_parked();
            workspace.update_in(cx, |workspace, window, cx| {
                assert_eq!(workspace.selected_center_pane(), Some(destination.clone()));
                assert_eq!(workspace.active_pane, destination);
                let item = cx.new(crate::item::test::TestItem::new);
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
                assert_eq!(destination.read(cx).items_len(), 1);
                assert_eq!(first.read(cx).items_len(), 0);
            });
        }
    }

    #[gpui::test]
    async fn dragging_detached_tab_below_editor_keeps_vertical_placement(cx: &mut TestAppContext) {
        check_drag_below_editor(cx, true).await;
    }

    #[gpui::test]
    async fn empty_pane_handle_moves_existing_custom_pane_down(cx: &mut TestAppContext) {
        check_empty_pane_drag(cx, true).await;
    }

    #[gpui::test]
    async fn empty_pane_handle_materializes_default_layout_before_dragging(
        cx: &mut TestAppContext,
    ) {
        check_empty_pane_drag(cx, false).await;
    }

    async fn check_empty_pane_drag(cx: &mut TestAppContext, custom: bool) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let (left, right) = workspace.update_in(cx, |workspace, window, cx| {
            if custom {
                workspace.materialize_panel_panes(cx);
            }
            let left = workspace.active_pane.clone();
            let right =
                workspace.split_pane(left.clone(), crate::SplitDirection::Right, window, cx);
            (left, right)
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let handle: &'static str =
            Box::leak(format!("detach-tab-handle-{}", right.entity_id()).into_boxed_str());
        let left_selector: &'static str =
            Box::leak(format!("pane-{}", left.entity_id()).into_boxed_str());
        let right_selector: &'static str =
            Box::leak(format!("pane-{}", right.entity_id()).into_boxed_str());
        let origin = cx.debug_bounds(handle).unwrap().center();
        let before = cx.debug_bounds(left_selector).unwrap();
        let target = point(before.center().x, before.bottom() - px(15.));
        cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            origin + point(px(12.), px(12.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|_, cx| {
            assert!(
                cx.has_active_drag(),
                "empty pane handle must start dragging"
            )
        });
        cx.run_until_parked();
        cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let top = cx.debug_bounds(left_selector).unwrap();
        let bottom = cx.debug_bounds(right_selector).unwrap();
        assert!(bottom.top() >= top.bottom());
        assert!((bottom.left() - top.left()).abs() < px(2.));
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.panes.len(),
                2,
                "moving an empty pane must not create another pane"
            );
            assert_eq!(right.read(cx).items_len(), 0);
        });
    }

    #[gpui::test]
    async fn dragging_regular_tab_below_editor_keeps_vertical_placement(cx: &mut TestAppContext) {
        check_drag_below_editor(cx, false).await;
    }

    async fn check_drag_below_editor(cx: &mut TestAppContext, detached: bool) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        let (left, right, item_id) = workspace.update_in(cx, |workspace, window, cx| {
            let left = workspace.active_pane.clone();
            let item = cx.new(crate::item::test::TestItem::new);
            left.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), true, true, None, window, cx)
            });
            workspace.materialize_panel_panes(cx);
            let right =
                workspace.split_pane(left.clone(), crate::SplitDirection::Right, window, cx);
            if !detached {
                let keep = cx.new(|cx| crate::item::test::TestItem::new(cx).with_label("keep"));
                right.update(cx, |pane, cx| {
                    pane.add_item(Box::new(keep), false, false, None, window, cx)
                });
            }
            let item = cx.new(|cx| crate::item::test::TestItem::new(cx).with_label("terminal"));
            let item_id = item.entity_id();
            right.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), true, true, None, window, cx)
            });
            (left, right, item_id)
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let handle: &'static str =
            Box::leak(format!("detach-tab-handle-{}", right.entity_id()).into_boxed_str());
        let source = if detached {
            cx.debug_bounds(handle).unwrap().center()
        } else {
            cx.debug_bounds("TAB-1").unwrap().center()
        };
        let left_id = left.read_with(cx, |pane, _| pane.layout_id.unwrap());
        let selector: &'static str =
            Box::leak(format!("panel-region-{:?}", Region::Pane(left_id)).into_boxed_str());
        let left_bounds = cx.debug_bounds(selector).unwrap();
        let target = point(left_bounds.center().x, left_bounds.bottom() - px(12.));
        cx.simulate_mouse_down(source, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            source + point(px(12.), px(12.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|_, cx| assert!(cx.has_active_drag(), "tab drag starts"));
        cx.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let destination_id = workspace.read_with(cx, |workspace, cx| {
            let destination = workspace
                .panes
                .iter()
                .find(|pane| pane.read(cx).items().any(|item| item.item_id() == item_id))
                .unwrap();
            assert_ne!(destination, &right);
            destination.read(cx).layout_id.unwrap()
        });
        let destination: &'static str =
            Box::leak(format!("panel-region-{:?}", Region::Pane(destination_id)).into_boxed_str());
        let top = cx.debug_bounds(selector).unwrap();
        let bottom = cx.debug_bounds(destination).unwrap();
        assert!(
            bottom.top() >= top.bottom(),
            "top {top:?}; bottom {bottom:?}; layout {:?}",
            workspace.read_with(cx, |workspace, _| workspace.panel_layout.clone())
        );
        assert!((bottom.left() - top.left()).abs() < px(2.));
    }

    #[gpui::test]
    async fn startup_placeholder_cannot_overwrite_saved_pane_layout(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (original, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let database = cx.update(|_, cx| crate::persistence::WorkspaceDb::global(cx));
        let workspace_id = database.next_id().await.unwrap();
        let expected = original.update_in(cx, |workspace, window, cx| {
            workspace.set_database_id(workspace_id);
            workspace.load_panel_layout(cx);
            let first = workspace.active_pane.clone();
            let second = workspace.add_pane(window, cx);
            workspace
                .center
                .split_group(&first, &second, crate::SplitDirection::Right, cx);
            workspace.materialize_panel_panes(cx);
            let first_id = first.read(cx).layout_id.unwrap();
            let second_id = second.read(cx).layout_id.unwrap();
            let mut layout = Layout::initial(settings::BottomDockLayout::Contained, None);
            layout.replace_region(
                Region::Editor,
                &Layout::split(
                    true,
                    0.68,
                    Layout::Region(Region::Pane(first_id)),
                    Layout::Region(Region::Pane(second_id)),
                ),
            );
            workspace.panel_layout = Some(layout.clone());
            workspace.save_panel_layout(cx);
            layout
        });
        original
            .update_in(cx, |workspace, window, cx| {
                workspace.serialize_workspace_internal(window, cx)
            })
            .await;
        cx.run_until_parked();
        let serialized = database.workspace_for_id(workspace_id).unwrap();
        let app_state = original.read_with(cx, |workspace, _| workspace.app_state.clone());
        let restored = cx
            .update(|window, cx| cx.new(|cx| Workspace::new(None, project, app_state, window, cx)));
        restored.update(cx, |workspace, cx| {
            workspace.set_database_id(workspace_id);
            workspace.load_panel_layout(cx);
            assert!(workspace.panel_layout_pending_restore);
            workspace.sync_panel_panes(cx);
            workspace.save_panel_layout(cx);
            assert_eq!(workspace.panel_layout.as_ref(), Some(&expected));
        });
        cx.run_until_parked();
        let stored = cx.update(|_, cx| {
            db::kvp::KeyValueStore::global(cx)
                .scoped("panel_layout")
                .read(&i64::from(workspace_id).to_string())
                .unwrap()
                .unwrap()
        });
        assert_eq!(serde_json::from_str::<Layout>(&stored).unwrap(), expected);
        restored
            .update_in(cx, |_, window, cx| {
                Workspace::load_workspace(serialized, Vec::new(), window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        restored.update(cx, |workspace, cx| {
            assert!(!workspace.panel_layout_pending_restore);
            workspace.load_panel_layout(cx);
            workspace.sync_panel_panes(cx);
            assert_eq!(workspace.panel_layout.as_ref(), Some(&expected));
            assert_eq!(workspace.panes.len(), 2);
        });
    }

    #[gpui::test]
    async fn drag_drop_preserves_panels_and_zoom_restores_layout(cx: &mut TestAppContext) {
        check_panel_layout(cx, settings::PaneLayout::Separated).await;
    }

    #[gpui::test]
    async fn flat_panel_dividers_remain_one_pixel(cx: &mut TestAppContext) {
        check_panel_layout(cx, settings::PaneLayout::Flat).await;
    }

    #[gpui::test]
    async fn close_root_editor_from_header_and_reopen(cx: &mut TestAppContext) {
        check_close_root_editor(cx, false).await;
    }

    #[gpui::test]
    async fn close_populated_root_editor_from_header_and_reopen(cx: &mut TestAppContext) {
        check_close_root_editor(cx, true).await;
    }

    async fn check_close_root_editor(cx: &mut TestAppContext, populated: bool) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TestPanel::new(DockPosition::Right, 1, cx));
            workspace.add_panel(panel, window, cx);
            workspace.right_dock.update(cx, |dock, cx| {
                dock.activate_panel(0, window, cx);
                dock.set_open(true, window, cx);
            });
            workspace.active_pane.update(cx, |pane, cx| {
                pane.set_should_display_tab_bar(|_, _| true);
                pane.set_display_tab_bar_when_empty(true);
                cx.notify();
            });
        });
        if populated {
            workspace.update_in(cx, |workspace, window, cx| {
                let item = cx.new(crate::item::test::TestItem::new);
                workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            });
        }
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let editor = cx.debug_bounds("panel-region-Editor").unwrap();
        let header = point(editor.center().x, editor.top() + px(12.));
        cx.simulate_mouse_move(header, None, Modifiers::default());
        cx.simulate_mouse_down(header, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(header, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = cx.debug_bounds("MENU_ITEM-Close Pane").unwrap().center();
        cx.simulate_mouse_move(close, None, Modifiers::default());
        cx.simulate_mouse_down(close, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(close, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("panel-region-Editor").is_none(),
            "Close Pane hides the root editor"
        );
        assert!(cx.debug_bounds("right-dock").unwrap().size.width > editor.size.width);
        workspace.update_in(cx, |workspace, window, cx| {
            let item = cx.new(crate::item::test::TestItem::new);
            let pane = workspace.panes[0].clone();
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(item), true, true, None, window, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("panel-region-Editor").is_some(),
            "opening a file restores the editor"
        );
    }

    #[gpui::test]
    async fn drag_editor_against_sidebar_and_reopen_at_same_edge(cx: &mut TestAppContext) {
        crate::tests::init_test(cx);
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            for position in [DockPosition::Left, DockPosition::Bottom] {
                let panel = cx.new(|cx| TestPanel::new(position, 1, cx));
                workspace.add_panel(panel, window, cx);
                workspace.dock_at_position(position).update(cx, |dock, cx| {
                    dock.activate_panel(0, window, cx);
                    dock.set_open(true, window, cx);
                });
            }
            let item = cx.new(crate::item::test::TestItem::new);
            workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
            let leaf = Layout::Region;
            workspace.panel_layout = Some(Layout::split(
                false,
                0.22,
                leaf(Region::Left),
                Layout::split(
                    false,
                    0.65,
                    Layout::split(false, 0.1, leaf(Region::Editor), leaf(Region::Bottom)),
                    leaf(Region::Right),
                ),
            ));
        });
        cx.simulate_resize(gpui::size(px(1200.), px(800.)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let sidebar = cx.debug_bounds("left-dock").unwrap();
        let chat = cx.debug_bounds("bottom-dock").unwrap();
        let start = point(chat.left(), chat.center().y);
        let edge = point(sidebar.right() + px(1.), start.y);
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start - point(px(15.), px(0.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("panel-region-Editor").is_none(),
            "editor fully hidden"
        );
        assert!((cx.debug_bounds("bottom-dock").unwrap().left() - sidebar.right()).abs() <= px(1.));
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), sidebar);
        workspace.read_with(cx, |workspace, _| {
            let saved = serde_json::to_string(workspace.panel_layout.as_ref().unwrap()).unwrap();
            let restored: Layout = serde_json::from_str(&saved).unwrap();
            assert!(restored.valid());
            assert_eq!(Some(&restored), workspace.panel_layout.as_ref());
        });
        cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            edge + point(px(20.), px(0.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_move(start, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(start, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("panel-region-Editor").is_some(),
            "edge reopens editor"
        );
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), sidebar);
        assert!((cx.debug_bounds("bottom-dock").unwrap().left() - start.x).abs() <= px(2.));
    }

    async fn check_panel_layout(cx: &mut TestAppContext, mode: settings::PaneLayout) {
        crate::tests::init_test(cx);
        cx.update_global(|store: &mut settings::SettingsStore, cx| {
            store.update_user_settings(cx, |settings| settings.workspace.pane_layout = Some(mode));
        });
        let project = project::Project::test(fs::FakeFs::new(cx.executor()), None, cx).await;
        let (host, cx) = cx.add_window_view(|window, cx| LayoutTestView {
            workspace: cx.new(|cx| Workspace::test_new(project, window, cx)),
        });
        let workspace = host.read_with(cx, |host, _| host.workspace.clone());
        let panels = workspace.update_in(cx, |workspace, window, cx| {
            [
                DockPosition::Left,
                DockPosition::Right,
                DockPosition::Bottom,
            ]
            .map(|position| {
                let panel = cx.new(|cx| {
                    let mut panel = TestPanel::new(position, 1, cx);
                    panel.fills_empty_workspace = position == DockPosition::Right;
                    panel
                });
                workspace.add_panel(panel.clone(), window, cx);
                workspace.dock_at_position(position).update(cx, |dock, cx| {
                    dock.activate_panel(0, window, cx);
                    dock.set_open(true, window, cx);
                });
                panel
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let start = cx
            .debug_bounds("panel-drag-handle")
            .expect("drag handle")
            .center();
        let target = cx
            .debug_bounds("panel-region-Right")
            .expect("right drop target");
        let destination = point(target.center().x, target.bottom() - px(15.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(
            start + point(px(15.), px(15.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|_, cx| assert!(cx.has_active_drag(), "panel handle starts a drag"));
        cx.simulate_mouse_move(destination, Some(MouseButton::Left), Modifiers::default());
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.panel_drop,
                Some((Region::Right, Edge::Bottom)),
                "panel gets drop preview"
            )
        });
        cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let layout = workspace.read_with(cx, |workspace, _| {
            workspace.panel_layout.clone().expect("drop applies layout")
        });
        let left = cx.debug_bounds("left-dock").expect("sidebar");
        let right = cx.debug_bounds("right-dock").expect("agents");
        let bottom = cx.debug_bounds("bottom-dock").expect("terminal");
        assert_eq!(
            left.right()
                + px(if mode == settings::PaneLayout::Flat {
                    1.
                } else {
                    0.
                }),
            right.left(),
            "frames supply the horizontal gap without an extra spacer"
        );
        assert_eq!(right.left(), bottom.left());
        assert_eq!(
            right.bottom()
                + px(if mode == settings::PaneLayout::Flat {
                    1.
                } else {
                    0.
                }),
            bottom.top(),
            "frames supply the vertical gap without an extra spacer"
        );
        assert_eq!(left.bottom(), bottom.bottom());
        panels[1].update_in(cx, |panel, window, cx| {
            window.focus(&panel.focus_handle, cx);
            cx.emit(PanelEvent::ZoomIn);
        });
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, _| workspace.zoomed.is_some()));
        panels[1].update(cx, |_, cx| cx.emit(PanelEvent::ZoomOut));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.panel_layout.as_ref(), Some(&layout));
            assert!(workspace.zoomed.is_none());
            for (position, panel) in [
                DockPosition::Left,
                DockPosition::Right,
                DockPosition::Bottom,
            ]
            .into_iter()
            .zip(&panels)
            {
                assert_eq!(
                    workspace
                        .dock_at_position(position)
                        .read(cx)
                        .active_panel()
                        .unwrap()
                        .panel_id(),
                    panel.entity_id()
                );
            }
        });
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), left);
        assert_eq!(cx.debug_bounds("right-dock").unwrap(), right);
        assert_eq!(cx.debug_bounds("bottom-dock").unwrap(), bottom);
        let divider = point(right.center().x, (right.bottom() + bottom.top()) / 2.);
        let resized = divider - point(px(0.), px(40.));
        cx.simulate_mouse_move(divider, None, Modifiers::default());
        cx.simulate_mouse_down(divider, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(resized, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_move(
            resized - point(px(0.), px(10.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(resized, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("right-dock").unwrap().size.height < right.size.height);
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), left);
        let resized_right = cx.debug_bounds("right-dock").unwrap();
        let resized_bottom = cx.debug_bounds("bottom-dock").unwrap();
        let divider = point(
            resized_right.center().x,
            (resized_right.bottom() + resized_bottom.top()) / 2.,
        );
        let edge = point(divider.x, resized_bottom.bottom() - px(2.));
        cx.simulate_mouse_move(divider, None, Modifiers::default());
        cx.simulate_mouse_down(divider, MouseButton::Left, Modifiers::default());
        let narrow = point(edge.x, resized_bottom.bottom() - px(14.));
        cx.simulate_mouse_move(narrow, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_move(narrow, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let narrow_height = cx.debug_bounds("bottom-dock").unwrap().size.height;
        assert!(
            (narrow_height - px(14.)).abs() <= px(2.),
            "tracks pointer below ten percent: {narrow_height:?}"
        );
        cx.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        assert!(!workspace.read_with(cx, |workspace, cx| workspace.bottom_dock.read(cx).is_open()));
        cx.simulate_mouse_move(narrow, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        assert!(
            workspace.read_with(cx, |workspace, cx| workspace.bottom_dock.read(cx).is_open()),
            "reverse before release"
        );
        cx.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(edge, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!workspace.read_with(cx, |workspace, cx| workspace.bottom_dock.read(cx).is_open()));
        assert_eq!(
            cx.debug_bounds("right-dock").unwrap().bottom(),
            left.bottom()
        );
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), left);
        cx.simulate_mouse_move(edge, None, Modifiers::default());
        cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(narrow, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_move(divider, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(divider, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let reopened = cx.debug_bounds("bottom-dock").unwrap();
        assert!(
            (reopened.size.height - resized_bottom.size.height).abs() <= px(1.),
            "new drag from collapsed edge"
        );
        assert_eq!(cx.debug_bounds("left-dock").unwrap(), left);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.reset_panel_layout(&crate::ResetPanelLayout, window, cx)
        });
        assert!(workspace.read_with(cx, |workspace, _| workspace.panel_layout.is_none()));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let start = cx.debug_bounds("panel-drag-handle").unwrap().center();
        let target = cx.debug_bounds("panel-region-Right").unwrap();
        let destination = point(target.center().x, target.top() + px(15.));
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(destination, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(
            cx.debug_bounds("bottom-dock").unwrap().bottom()
                <= cx.debug_bounds("right-dock").unwrap().top()
        );
    }
    #[test]
    fn moving_bottom_under_agents_keeps_sidebar_outside_the_stack() {
        let layout = Layout::initial(settings::BottomDockLayout::Full, Some(DockPosition::Right));
        let moved = layout
            .move_region(Region::Bottom, Region::Right, Edge::Bottom)
            .unwrap();
        assert!(moved.valid());
        let Layout::Split {
            vertical: false,
            first,
            second,
            ..
        } = &moved
        else {
            panic!("sidebar remains in its column")
        };
        assert_eq!(**first, Layout::Region(Region::Left));
        assert!(second.contains(Region::Right) && second.contains(Region::Bottom));
    }
    #[test]
    fn every_region_can_move_to_every_edge_without_losing_or_duplicating_regions() {
        let mut layout = Layout::initial(settings::BottomDockLayout::Contained, None);
        for source in [Region::Left, Region::Right, Region::Bottom, Region::Editor] {
            for target in [Region::Left, Region::Right, Region::Bottom, Region::Editor] {
                for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                    if source == target {
                        assert!(layout.move_region(source, target, edge).is_none());
                    } else {
                        layout = layout.move_region(source, target, edge).unwrap();
                        assert!(layout.valid());
                    }
                }
            }
        }
        let restored: Layout =
            serde_json::from_str(&serde_json::to_string(&layout).unwrap()).unwrap();
        assert_eq!(restored, layout);
    }
}

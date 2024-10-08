//! A picking backend for UI nodes.
//!
//! # Usage
//!
//! This backend does not require markers on cameras or entities to function. It will look for any
//! pointers using the same render target as the UI camera, and run hit tests on the UI node tree.
//!
//! ## Important Note
//!
//! This backend completely ignores [`FocusPolicy`](crate::FocusPolicy). The design of `bevy_ui`'s
//! focus systems and the picking plugin are not compatible. Instead, use the [`Pickable`] component
//! to customize how an entity responds to picking focus. Nodes without the [`Pickable`] component
//! will not trigger events.
//!
//! ## Implementation Notes
//!
//! - `bevy_ui` can only render to the primary window
//! - `bevy_ui` can render on any camera with a flag, it is special, and is not tied to a particular
//!   camera.
//! - To correctly sort picks, the order of `bevy_ui` is set to be the camera order plus 0.5.

#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![deny(missing_docs)]

use crate::{prelude::*, UiStack};
use bevy_app::prelude::*;
use bevy_ecs::{prelude::*, query::QueryData};
use bevy_math::Vec2;
use bevy_render::prelude::*;
use bevy_transform::prelude::*;
use bevy_utils::hashbrown::HashMap;
use bevy_window::PrimaryWindow;

use bevy_picking::backend::prelude::*;

/// A plugin that adds picking support for UI nodes.
#[derive(Clone)]
pub struct UiPickingBackend;
impl Plugin for UiPickingBackend {
    fn build(&self, app: &mut App) {
        app.add_systems(PreUpdate, ui_picking.in_set(PickSet::Backend));
    }
}

/// Main query from bevy's `ui_focus_system`
#[derive(QueryData)]
#[query_data(mutable)]
pub struct NodeQuery {
    entity: Entity,
    node: &'static Node,
    global_transform: &'static GlobalTransform,
    pickable: Option<&'static Pickable>,
    calculated_clip: Option<&'static CalculatedClip>,
    view_visibility: Option<&'static ViewVisibility>,
    target_camera: Option<&'static TargetCamera>,
}

/// Computes the UI node entities under each pointer.
///
/// Bevy's [`UiStack`] orders all nodes in the order they will be rendered, which is the same order
/// we need for determining picking.
pub fn ui_picking(
    pointers: Query<(&PointerId, &PointerLocation)>,
    camera_query: Query<(Entity, &Camera, Has<IsDefaultUiCamera>)>,
    default_ui_camera: DefaultUiCamera,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    ui_scale: Res<UiScale>,
    ui_stack: Res<UiStack>,
    mut node_query: Query<NodeQuery>,
    mut output: EventWriter<PointerHits>,
) {
    // For each camera, the pointer and its position
    let mut pointer_pos_by_camera = HashMap::<Entity, HashMap<PointerId, Vec2>>::new();

    for (pointer_id, pointer_location) in
        pointers.iter().filter_map(|(pointer, pointer_location)| {
            Some(*pointer).zip(pointer_location.location().cloned())
        })
    {
        // This pointer is associated with a render target, which could be used by multiple
        // cameras. We want to ensure we return all cameras with a matching target.
        for camera in camera_query
            .iter()
            .map(|(entity, camera, _)| {
                (
                    entity,
                    camera.target.normalize(primary_window.get_single().ok()),
                )
            })
            .filter_map(|(entity, target)| Some(entity).zip(target))
            .filter(|(_entity, target)| target == &pointer_location.target)
            .map(|(cam_entity, _target)| cam_entity)
        {
            let Ok((_, camera_data, _)) = camera_query.get(camera) else {
                continue;
            };
            let mut pointer_pos = pointer_location.position;
            if let Some(viewport) = camera_data.logical_viewport_rect() {
                pointer_pos -= viewport.min;
            }
            let scaled_pointer_pos = pointer_pos / **ui_scale;
            pointer_pos_by_camera
                .entry(camera)
                .or_default()
                .insert(pointer_id, scaled_pointer_pos);
        }
    }

    // The list of node entities hovered for each (camera, pointer) combo
    let mut hit_nodes = HashMap::<(Entity, PointerId), Vec<Entity>>::new();

    // prepare an iterator that contains all the nodes that have the cursor in their rect,
    // from the top node to the bottom one. this will also reset the interaction to `None`
    // for all nodes encountered that are no longer hovered.
    for node_entity in ui_stack
        .uinodes
        .iter()
        // reverse the iterator to traverse the tree from closest nodes to furthest
        .rev()
    {
        let Ok(node) = node_query.get_mut(*node_entity) else {
            continue;
        };

        // Nodes that are not rendered should not be interactable
        if node
            .view_visibility
            .map(|view_visibility| view_visibility.get())
            != Some(true)
        {
            continue;
        }
        let Some(camera_entity) = node
            .target_camera
            .map(TargetCamera::entity)
            .or(default_ui_camera.get())
        else {
            continue;
        };

        let node_rect = node.node.logical_rect(node.global_transform);

        // Nodes with Display::None have a (0., 0.) logical rect and can be ignored
        if node_rect.size() == Vec2::ZERO {
            continue;
        }

        // Intersect with the calculated clip rect to find the bounds of the visible region of the node
        let visible_rect = node
            .calculated_clip
            .map(|clip| node_rect.intersect(clip.clip))
            .unwrap_or(node_rect);

        let pointers_on_this_cam = pointer_pos_by_camera.get(&camera_entity);

        // The mouse position relative to the node
        // (0., 0.) is the top-left corner, (1., 1.) is the bottom-right corner
        // Coordinates are relative to the entire node, not just the visible region.
        for (pointer_id, cursor_position) in pointers_on_this_cam.iter().flat_map(|h| h.iter()) {
            let relative_cursor_position = (*cursor_position - node_rect.min) / node_rect.size();

            if visible_rect
                .normalize(node_rect)
                .contains(relative_cursor_position)
                && pick_rounded_rect(
                    *cursor_position - node_rect.center(),
                    node_rect.size(),
                    node.node.border_radius,
                )
            {
                hit_nodes
                    .entry((camera_entity, *pointer_id))
                    .or_default()
                    .push(*node_entity);
            }
        }
    }

    for ((camera, pointer), hovered_nodes) in hit_nodes.iter() {
        // As soon as a node with a `Block` focus policy is detected, the iteration will stop on it
        // because it "captures" the interaction.
        let mut iter = node_query.iter_many_mut(hovered_nodes.iter());
        let mut picks = Vec::new();
        let mut depth = 0.0;

        while let Some(node) = iter.fetch_next() {
            let Some(camera_entity) = node
                .target_camera
                .map(TargetCamera::entity)
                .or(default_ui_camera.get())
            else {
                continue;
            };

            picks.push((node.entity, HitData::new(camera_entity, depth, None, None)));

            if let Some(pickable) = node.pickable {
                // If an entity has a `Pickable` component, we will use that as the source of truth.
                if pickable.should_block_lower {
                    break;
                }
            } else {
                // If the Pickable component doesn't exist, default behavior is to block.
                break;
            }

            depth += 0.00001; // keep depth near 0 for precision
        }

        let order = camera_query
            .get(*camera)
            .map(|(_, cam, _)| cam.order)
            .unwrap_or_default() as f32
            + 0.5; // bevy ui can run on any camera, it's a special case

        output.send(PointerHits::new(*pointer, picks, order));
    }
}

// Returns true if `point` (relative to the rectangle's center) is within the bounds of a rounded rectangle with
// the given size and border radius.
//
// Matches the sdf function in `ui.wgsl` that is used by the UI renderer to draw rounded rectangles.
pub(crate) fn pick_rounded_rect(
    point: Vec2,
    size: Vec2,
    border_radius: ResolvedBorderRadius,
) -> bool {
    let s = point.signum();
    let r = (border_radius.top_left * (1. - s.x) * (1. - s.y)
        + border_radius.top_right * (1. + s.x) * (1. - s.y)
        + border_radius.bottom_right * (1. + s.x) * (1. + s.y)
        + border_radius.bottom_left * (1. - s.x) * (1. + s.y))
        / 4.;

    let corner_to_point = point.abs() - 0.5 * size;
    let q = corner_to_point + r;
    let l = q.max(Vec2::ZERO).length();
    let m = q.max_element().min(0.);
    l + m - r < 0.
}

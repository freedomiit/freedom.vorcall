//! Dragging channels, categories and roles into place.
//!
//! The reorder arithmetic is pure and lives here: the page hands in the list as
//! it stands plus the slot the pointer is over, and gets the full list the
//! protocol wants back. Nothing is applied locally — `PROTOCOL.md` § Channels
//! answers every reorder with a broadcast, and that broadcast is what moves the
//! model.

use iced::Task;
use vorcall_core::connection::{AdminCommand, Command};
use vorcall_core::{ChannelPosition, Role, permissions};

use crate::app::App;
use crate::app::message::{DragItem, DragMsg, DragSlot, Message};
use crate::app::state::server::ServerModel;
use crate::app::state::ui::DragState;

pub fn update(app: &mut App, message: DragMsg) -> Task<Message> {
    match message {
        DragMsg::DragStart(item) => {
            app.ui.drag = Some(DragState {
                item,
                slot: None,
                at: app.ui.cursor,
            });
            Task::none()
        }
        DragMsg::DragOver(slot) => {
            if let Some(drag) = &mut app.ui.drag {
                drag.slot = Some(slot);
            }
            Task::none()
        }
        DragMsg::DragEnd => drop_item(app),
        DragMsg::DragCancel => {
            app.ui.drag = None;
            Task::none()
        }
    }
}

/// The end of a drag: the full list the server stores, or nothing at all when the
/// pointer never reached a slot.
fn drop_item(app: &mut App) -> Task<Message> {
    let Some(drag) = app.ui.drag.take() else {
        return Task::none();
    };
    let Some(slot) = drag.slot else {
        return Task::none();
    };
    let Some(main) = app.main_mut() else {
        return Task::none();
    };

    match drag.item {
        DragItem::Channel(id) => {
            if !main.server.can(permissions::MANAGE_CHANNELS, None) {
                return Task::none();
            }
            let current = channel_positions(&main.server);
            let positions = reorder_channels(&current, id, slot);
            if positions != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderChannels { positions }));
            }
        }
        DragItem::Category(id) => {
            if !main.server.can(permissions::MANAGE_CHANNELS, None) {
                return Task::none();
            }
            let current = category_ids(&main.server);
            let ids = reorder_ids(&current, id, slot);
            if ids != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderCategories { ids }));
            }
        }
        DragItem::Role(id) => {
            if !main.server.can_manage_role(id) {
                return Task::none();
            }
            let current = role_ids(&main.server);
            let moved = reorder_ids(&current, id, slot);
            if moved != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderRoles {
                    ids: bottom_first(&moved),
                }));
            }
        }
    }
    Task::none()
}

/// One step up or down the list, which is what the arrow buttons do.
pub fn nudge(app: &mut App, item: DragItem, up: bool) -> Task<Message> {
    let Some(main) = app.main_mut() else {
        return Task::none();
    };
    // The same guards the drop has: an arrow is the keyboard's drag, not a way
    // around the hierarchy.
    match item {
        DragItem::Channel(id) => {
            if !main.server.can(permissions::MANAGE_CHANNELS, None) {
                return Task::none();
            }
            let current = channel_positions(&main.server);
            let positions = nudge_channel(&current, id, up);
            if positions != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderChannels { positions }));
            }
        }
        DragItem::Category(id) => {
            if !main.server.can(permissions::MANAGE_CHANNELS, None) {
                return Task::none();
            }
            let current = category_ids(&main.server);
            let ids = nudge_ids(&current, id, up);
            if ids != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderCategories { ids }));
            }
        }
        DragItem::Role(id) => {
            if !main.server.can_manage_role(id) {
                return Task::none();
            }
            let current = role_ids(&main.server);
            let moved = nudge_ids(&current, id, up);
            if moved != current {
                main.send_or_notice(Command::Admin(AdminCommand::ReorderRoles {
                    ids: bottom_first(&moved),
                }));
            }
        }
    }
    Task::none()
}

/// Every non-DM channel as the tree draws it: the group with no category first,
/// then each category in its own order, positions renumbered densely so a
/// comparison against a reordered list is exact.
pub fn channel_positions(server: &ServerModel) -> Vec<ChannelPosition> {
    let mut groups: Vec<(i64, Vec<i64>)> = vec![(
        0,
        server
            .channels_in(None)
            .iter()
            .map(|channel| channel.id)
            .collect(),
    )];
    for category in server.ordered_categories() {
        groups.push((
            category.id,
            server
                .channels_in(Some(category.id))
                .iter()
                .map(|channel| channel.id)
                .collect(),
        ));
    }
    groups.retain(|(_, ids)| !ids.is_empty());
    flatten(&groups)
}

/// Every category in sidebar order.
pub fn category_ids(server: &ServerModel) -> Vec<i64> {
    server
        .ordered_categories()
        .into_iter()
        .map(|category| category.id)
        .collect()
}

/// Every role but `@everyone`, highest first, which is the order the page draws.
pub fn role_ids(server: &ServerModel) -> Vec<i64> {
    let mut roles: Vec<&Role> = server
        .roles
        .values()
        .filter(|role| !role.everyone)
        .collect();
    roles.sort_by_key(|role| std::cmp::Reverse((role.position, role.id)));
    roles.into_iter().map(|role| role.id).collect()
}

/// `ReorderRoles` takes the list bottom first, so `position = index + 1`
/// (`PROTOCOL.md` § Roles and permissions); the page works top first.
pub fn bottom_first(top_first: &[i64]) -> Vec<i64> {
    top_first.iter().rev().copied().collect()
}

/// One channel moved to `slot`. The answer is the full list, whatever moved.
pub fn reorder_channels(
    current: &[ChannelPosition],
    item: i64,
    slot: DragSlot,
) -> Vec<ChannelPosition> {
    if !current.iter().any(|entry| entry.id == item) {
        return current.to_vec();
    }
    let mut groups = groups(current);
    let Some((category, before)) = channel_target(&groups, slot) else {
        return current.to_vec();
    };
    // Dropped just before itself: the list it came from is the list it goes to.
    if before == Some(item) {
        return current.to_vec();
    }

    for (_, ids) in &mut groups {
        ids.retain(|id| *id != item);
    }
    match groups.iter_mut().find(|(id, _)| *id == category) {
        Some((_, ids)) => {
            let at = before
                .and_then(|before| ids.iter().position(|id| *id == before))
                .unwrap_or(ids.len());
            ids.insert(at, item);
        }
        // The first channel of a category that had none.
        None => groups.push((category, vec![item])),
    }
    groups.retain(|(_, ids)| !ids.is_empty());
    flatten(&groups)
}

/// One id of a flat list moved to `slot`.
pub fn reorder_ids(ids: &[i64], item: i64, slot: DragSlot) -> Vec<i64> {
    let before = match slot {
        DragSlot::BeforeCategory(id) | DragSlot::BeforeRole(id) => Some(id),
        DragSlot::EndOfCategories | DragSlot::EndOfRoles => None,
        // A channel slot says nothing about a flat list.
        DragSlot::BeforeChannel(_) | DragSlot::EndOfCategory(_) => return ids.to_vec(),
    };
    if before == Some(item) || !ids.contains(&item) {
        return ids.to_vec();
    }

    let mut moved: Vec<i64> = ids.iter().copied().filter(|id| *id != item).collect();
    let at = before
        .and_then(|before| moved.iter().position(|id| *id == before))
        .unwrap_or(moved.len());
    moved.insert(at, item);
    moved
}

/// One channel one step up or down: past its neighbour inside its own category,
/// or into the group beside it when it is already at the edge.
pub fn nudge_channel(current: &[ChannelPosition], item: i64, up: bool) -> Vec<ChannelPosition> {
    let groups = groups(current);
    match nudge_slot(&groups, item, up) {
        Some(slot) => reorder_channels(current, item, slot),
        None => current.to_vec(),
    }
}

/// One id swapped with its neighbour.
pub fn nudge_ids(ids: &[i64], item: i64, up: bool) -> Vec<i64> {
    let mut moved = ids.to_vec();
    let Some(index) = moved.iter().position(|id| *id == item) else {
        return moved;
    };
    let other = if up {
        index.checked_sub(1)
    } else {
        (index + 1 < moved.len()).then_some(index + 1)
    };
    if let Some(other) = other {
        moved.swap(index, other);
    }
    moved
}

/// The tree as `(category_id, channel ids)`, groups in first-seen order.
fn groups(current: &[ChannelPosition]) -> Vec<(i64, Vec<i64>)> {
    let mut groups: Vec<(i64, Vec<i64>)> = Vec::new();
    for entry in current {
        match groups.iter_mut().find(|(id, _)| *id == entry.category_id) {
            Some((_, ids)) => ids.push(entry.id),
            None => groups.push((entry.category_id, vec![entry.id])),
        }
    }
    groups
}

/// Where a channel slot points: the category it lands in, and the channel it
/// lands before — `None` being the end of that category.
fn channel_target(groups: &[(i64, Vec<i64>)], slot: DragSlot) -> Option<(i64, Option<i64>)> {
    match slot {
        DragSlot::BeforeChannel(before) => groups
            .iter()
            .find(|(_, ids)| ids.contains(&before))
            .map(|(category, _)| (*category, Some(before))),
        DragSlot::EndOfCategory(category) => Some((category.unwrap_or(0), None)),
        DragSlot::BeforeCategory(_)
        | DragSlot::EndOfCategories
        | DragSlot::BeforeRole(_)
        | DragSlot::EndOfRoles => None,
    }
}

/// The slot an arrow press means.
fn nudge_slot(groups: &[(i64, Vec<i64>)], item: i64, up: bool) -> Option<DragSlot> {
    let (group, index) = groups.iter().enumerate().find_map(|(group, (_, ids))| {
        ids.iter()
            .position(|id| *id == item)
            .map(|index| (group, index))
    })?;
    let ids = &groups[group].1;

    if up {
        if index > 0 {
            return Some(DragSlot::BeforeChannel(ids[index - 1]));
        }
        let above = groups.get(group.checked_sub(1)?)?;
        return Some(DragSlot::EndOfCategory(category_slot(above.0)));
    }
    if index + 2 < ids.len() {
        return Some(DragSlot::BeforeChannel(ids[index + 2]));
    }
    if index + 1 < ids.len() {
        return Some(DragSlot::EndOfCategory(category_slot(groups[group].0)));
    }
    let below = groups.get(group + 1)?;
    Some(match below.1.first() {
        Some(first) => DragSlot::BeforeChannel(*first),
        None => DragSlot::EndOfCategory(category_slot(below.0)),
    })
}

/// `category_id` 0 on the wire is "no category", which a slot spells `None`.
fn category_slot(category_id: i64) -> Option<i64> {
    (category_id != 0).then_some(category_id)
}

/// The list grouped back into `ChannelPosition` rows, positions dense per group.
fn flatten(groups: &[(i64, Vec<i64>)]) -> Vec<ChannelPosition> {
    groups
        .iter()
        .flat_map(|(category_id, ids)| {
            ids.iter()
                .enumerate()
                .map(move |(index, id)| ChannelPosition {
                    id: *id,
                    category_id: *category_id,
                    position: i32::try_from(index).unwrap_or(i32::MAX),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two channels with no category, three in category 1, one in category 2.
    fn tree() -> Vec<ChannelPosition> {
        let rows = [(10, 0), (11, 0), (20, 1), (21, 1), (22, 1), (30, 2)];
        let mut positions = Vec::new();
        let mut next: Vec<(i64, i32)> = Vec::new();
        for (id, category_id) in rows {
            let position = match next
                .iter_mut()
                .find(|(category, _)| *category == category_id)
            {
                Some((_, position)) => {
                    *position += 1;
                    *position
                }
                None => {
                    next.push((category_id, 0));
                    0
                }
            };
            positions.push(ChannelPosition {
                id,
                category_id,
                position,
            });
        }
        positions
    }

    /// `(id, category, position)` for every row, which is what the wire carries.
    fn shape(positions: &[ChannelPosition]) -> Vec<(i64, i64, i32)> {
        positions
            .iter()
            .map(|entry| (entry.id, entry.category_id, entry.position))
            .collect()
    }

    #[test]
    fn a_channel_moves_up_inside_its_category() {
        let moved = reorder_channels(&tree(), 22, DragSlot::BeforeChannel(21));

        assert_eq!(
            shape(&moved),
            [
                (10, 0, 0),
                (11, 0, 1),
                (20, 1, 0),
                (22, 1, 1),
                (21, 1, 2),
                (30, 2, 0),
            ]
        );
    }

    /// Down one means "before the one after the next", which is what an arrow
    /// press works out for itself.
    #[test]
    fn a_channel_moves_down_inside_its_category() {
        let moved = nudge_channel(&tree(), 20, false);

        assert_eq!(shape(&moved)[2..5], [(21, 1, 0), (20, 1, 1), (22, 1, 2)]);
    }

    #[test]
    fn a_channel_crosses_into_another_category() {
        let moved = reorder_channels(&tree(), 11, DragSlot::BeforeChannel(21));

        assert_eq!(
            shape(&moved),
            [
                (10, 0, 0),
                (20, 1, 0),
                (11, 1, 1),
                (21, 1, 2),
                (22, 1, 3),
                (30, 2, 0),
            ]
        );
    }

    #[test]
    fn a_channel_dropped_at_the_end_of_a_category_lands_last() {
        let moved = reorder_channels(&tree(), 10, DragSlot::EndOfCategory(Some(2)));

        assert_eq!(shape(&moved)[4..], [(30, 2, 0), (10, 2, 1)]);
        // "No category" is the same slot with nothing in it.
        let back = reorder_channels(&moved, 10, DragSlot::EndOfCategory(None));
        assert_eq!(shape(&back)[..2], [(11, 0, 0), (10, 0, 1)]);
    }

    #[test]
    fn a_channel_is_the_first_of_an_empty_category() {
        let moved = reorder_channels(&tree(), 30, DragSlot::EndOfCategory(Some(9)));

        // Category 2 is left out of the list entirely once it is empty.
        assert_eq!(shape(&moved).last(), Some(&(30, 9, 0)));
        assert!(!shape(&moved).iter().any(|(_, category, _)| *category == 2));
    }

    #[test]
    fn a_channel_dropped_where_it_already_is_changes_nothing() {
        let tree = tree();

        assert_eq!(
            reorder_channels(&tree, 21, DragSlot::BeforeChannel(21)),
            tree
        );
        assert_eq!(
            reorder_channels(&tree, 30, DragSlot::EndOfCategory(Some(2))),
            tree
        );
        // An unknown channel, and a slot that belongs to another list.
        assert_eq!(
            reorder_channels(&tree, 99, DragSlot::BeforeChannel(21)),
            tree
        );
        assert_eq!(reorder_channels(&tree, 21, DragSlot::EndOfRoles), tree);
    }

    #[test]
    fn an_arrow_at_the_edge_of_a_category_crosses_into_the_next_one() {
        // 20 is the first of category 1, so up puts it last in the group above.
        let up = nudge_channel(&tree(), 20, true);
        assert_eq!(shape(&up)[..3], [(10, 0, 0), (11, 0, 1), (20, 0, 2)]);

        // 22 is the last of category 1, so down puts it before category 2's first.
        let down = nudge_channel(&tree(), 22, false);
        assert_eq!(shape(&down)[4..], [(22, 2, 0), (30, 2, 1)]);
    }

    #[test]
    fn an_arrow_at_the_end_of_the_tree_does_nothing() {
        let tree = tree();

        assert_eq!(nudge_channel(&tree, 10, true), tree);
        assert_eq!(nudge_channel(&tree, 30, false), tree);
        assert_eq!(nudge_channel(&tree, 99, true), tree);
    }

    #[test]
    fn a_flat_list_reorders_by_the_slot_it_is_dropped_on() {
        let ids = [1, 2, 3, 4];

        assert_eq!(reorder_ids(&ids, 4, DragSlot::BeforeRole(2)), [1, 4, 2, 3]);
        assert_eq!(reorder_ids(&ids, 1, DragSlot::EndOfRoles), [2, 3, 4, 1]);
        assert_eq!(
            reorder_ids(&ids, 3, DragSlot::BeforeCategory(1)),
            [3, 1, 2, 4]
        );
        assert_eq!(
            reorder_ids(&ids, 2, DragSlot::EndOfCategories),
            [1, 3, 4, 2]
        );
    }

    #[test]
    fn a_flat_list_dropped_where_it_already_is_changes_nothing() {
        let ids = [1, 2, 3];

        assert_eq!(reorder_ids(&ids, 2, DragSlot::BeforeRole(2)), ids);
        assert_eq!(reorder_ids(&ids, 3, DragSlot::EndOfRoles), ids);
        assert_eq!(reorder_ids(&ids, 9, DragSlot::BeforeRole(1)), ids);
        // A channel slot never touches a flat list.
        assert_eq!(reorder_ids(&ids, 1, DragSlot::EndOfCategory(None)), ids);
    }

    #[test]
    fn an_arrow_swaps_a_flat_list_entry_with_its_neighbour() {
        let ids = [1, 2, 3];

        assert_eq!(nudge_ids(&ids, 2, true), [2, 1, 3]);
        assert_eq!(nudge_ids(&ids, 2, false), [1, 3, 2]);
        assert_eq!(nudge_ids(&ids, 1, true), ids);
        assert_eq!(nudge_ids(&ids, 3, false), ids);
        assert_eq!(nudge_ids(&ids, 9, true), ids);
    }

    /// The page draws the hierarchy highest first; the wire wants it bottom
    /// first, so `position = index + 1` lands where the page put it.
    #[test]
    fn the_wire_order_is_the_page_order_reversed() {
        assert_eq!(bottom_first(&[7, 4, 2]), [2, 4, 7]);
        assert!(bottom_first(&[]).is_empty());
    }
}

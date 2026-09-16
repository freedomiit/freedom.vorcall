//! The server as this account may see it: the snapshot, every delta applied to
//! it, and the questions the views ask of it.
//!
//! The permission mirror is `vorcall_core::permissions`, the same engine the
//! server enforces with. It is used here to *hide* — the server is still the
//! boundary — so a resolve that disagrees costs a greyed-out control, never an
//! accepted frame.

use std::collections::{BTreeMap, BTreeSet};

use vorcall_core::permissions::{self, ChannelDef, Hierarchy, MemberDef, OverrideDef, RoleDef};
use vorcall_core::{
    Category, Channel, ChannelKind, ChannelPosition, Profile, ReadState, Role, Server,
    ServerSnapshot,
};

/// What a member with no roles of its own resolves against.
const NO_ROLES: &[i64] = &[];
/// What a channel with no overrides resolves against.
const NO_OVERRIDES: &[OverrideDef] = &[];

/// Who is listed where in the member pane.
#[derive(Debug, Clone)]
pub struct MemberGroup {
    pub label: String,
    pub members: Vec<i64>,
}

#[derive(Debug, Default)]
pub struct ServerModel {
    /// The signed-in account, which is who every `can_*` answer is about.
    pub me: i64,
    pub server: Server,
    pub roles: BTreeMap<i64, Role>,
    pub categories: BTreeMap<i64, Category>,
    pub channels: BTreeMap<i64, Channel>,
    pub members: BTreeMap<i64, Profile>,
    pub reads: BTreeMap<i64, ReadState>,
    /// The roles as the engine takes them, rebuilt whenever a role changes: a
    /// resolve per drawn row must not pay for building it.
    role_defs: Vec<RoleDef>,
    /// The same, per channel, for its overrides.
    overrides: BTreeMap<i64, Vec<OverrideDef>>,
}

impl ServerModel {
    /// Replaces everything with what the snapshot says. A reconnect starts here.
    pub fn apply_snapshot(&mut self, snapshot: ServerSnapshot) {
        self.server = snapshot.server.unwrap_or_default();
        self.roles = snapshot
            .roles
            .into_iter()
            .map(|role| (role.id, role))
            .collect();
        self.categories = snapshot
            .categories
            .into_iter()
            .map(|category| (category.id, category))
            .collect();
        self.channels = snapshot
            .channels
            .into_iter()
            .map(|channel| (channel.id, channel))
            .collect();
        self.members = snapshot
            .members
            .into_iter()
            .map(|member| (member.user_id, member))
            .collect();
        self.reads = snapshot
            .read_states
            .into_iter()
            .map(|state| (state.channel_id, state))
            .collect();
        self.rebuild_roles();
        self.rebuild_overrides();
    }

    pub fn upsert_role(&mut self, role: Role) {
        self.roles.insert(role.id, role);
        self.rebuild_roles();
    }

    pub fn delete_role(&mut self, id: i64) {
        self.roles.remove(&id);
        for member in self.members.values_mut() {
            member.role_ids.retain(|role_id| *role_id != id);
        }
        self.rebuild_roles();
    }

    /// `ids` is the full list, the everyone role excluded, bottom first.
    pub fn set_role_order(&mut self, ids: &[i64]) {
        for (index, id) in ids.iter().enumerate() {
            if let Some(role) = self.roles.get_mut(id) {
                role.position = i32::try_from(index + 1).unwrap_or(i32::MAX);
            }
        }
        self.rebuild_roles();
    }

    pub fn upsert_category(&mut self, category: Category) {
        self.categories.insert(category.id, category);
    }

    pub fn delete_category(&mut self, id: i64) {
        self.categories.remove(&id);
        // The server moves its channels to "no category"; until the deltas for
        // them land, showing them there is what the list does anyway.
        for channel in self.channels.values_mut() {
            if channel.category_id == id {
                channel.category_id = 0;
            }
        }
    }

    pub fn upsert_channel(&mut self, channel: Channel) {
        let id = channel.id;
        self.channels.insert(id, channel);
        self.rebuild_channel_overrides(id);
    }

    pub fn delete_channel(&mut self, id: i64) {
        self.channels.remove(&id);
        self.overrides.remove(&id);
        self.reads.remove(&id);
    }

    pub fn set_channel_order(&mut self, positions: &[ChannelPosition]) {
        for position in positions {
            if let Some(channel) = self.channels.get_mut(&position.id) {
                channel.category_id = position.category_id;
                channel.position = position.position;
            }
        }
    }

    pub fn upsert_member(&mut self, member: Profile) {
        self.members.insert(member.user_id, member);
    }

    pub fn remove_member(&mut self, user_id: i64) {
        self.members.remove(&user_id);
    }

    fn rebuild_roles(&mut self) {
        let mut defs: Vec<RoleDef> = self
            .roles
            .values()
            .map(|role| RoleDef {
                id: role.id,
                position: role.position,
                permissions: role.permissions,
                everyone: role.everyone,
            })
            .collect();
        defs.sort_by_key(|role| (role.position, role.id));
        self.role_defs = defs;
    }

    fn rebuild_overrides(&mut self) {
        let ids: Vec<i64> = self.channels.keys().copied().collect();
        self.overrides.clear();
        for id in ids {
            self.rebuild_channel_overrides(id);
        }
    }

    fn rebuild_channel_overrides(&mut self, id: i64) {
        let Some(channel) = self.channels.get(&id) else {
            self.overrides.remove(&id);
            return;
        };
        let defs: Vec<OverrideDef> = channel
            .overrides
            .iter()
            .map(|entry| OverrideDef {
                role_id: (entry.role_id != 0).then_some(entry.role_id),
                user_id: (entry.user_id != 0).then_some(entry.user_id),
                allow: entry.allow,
                deny: entry.deny,
            })
            .collect();
        self.overrides.insert(id, defs);
    }

    /// The everyone role, which every member holds implicitly.
    pub fn everyone(&self) -> Option<&Role> {
        self.roles.values().find(|role| role.everyone)
    }

    fn hierarchy(&self) -> Hierarchy<'_> {
        Hierarchy {
            owner_id: self.server.owner_id,
            roles: &self.role_defs,
        }
    }

    fn member_def(&self, user_id: i64) -> MemberDef<'_> {
        MemberDef {
            user_id,
            role_ids: self
                .members
                .get(&user_id)
                .map_or(NO_ROLES, |member| member.role_ids.as_slice()),
        }
    }

    fn channel_def(&self, channel_id: i64) -> ChannelDef<'_> {
        ChannelDef {
            id: channel_id,
            is_general: channel_id != 0 && channel_id == self.server.general_channel_id,
            overrides: self
                .overrides
                .get(&channel_id)
                .map_or(NO_OVERRIDES, Vec::as_slice),
        }
    }

    /// `user_id`'s permissions: server-wide with `channel` unset, inside that
    /// channel otherwise.
    pub fn resolve(&self, user_id: i64, channel: Option<i64>) -> u64 {
        permissions::resolve(
            &self.hierarchy(),
            &self.member_def(user_id),
            channel.map(|id| self.channel_def(id)),
        )
    }

    /// Whether this account holds `bit`, in `channel` when one is given.
    pub fn can(&self, bit: u64, channel: Option<i64>) -> bool {
        permissions::has(self.resolve(self.me, channel), bit)
    }

    /// Whether this account owns the server, which bypasses every check.
    pub fn is_owner(&self) -> bool {
        self.me != 0 && self.me == self.server.owner_id
    }

    /// Whether this account may act on `user_id` at all: hierarchy only, the
    /// permission itself is a separate question.
    pub fn can_target(&self, user_id: i64) -> bool {
        permissions::can_target(
            &self.hierarchy(),
            &self.member_def(self.me),
            &self.member_def(user_id),
        ) == permissions::TargetVerdict::Allowed
    }

    /// Whether this account may edit `role_id`.
    pub fn can_manage_role(&self, role_id: i64) -> bool {
        let Some(role) = self.roles.get(&role_id) else {
            return false;
        };
        permissions::can_manage_role(
            &self.hierarchy(),
            &self.member_def(self.me),
            &RoleDef {
                id: role.id,
                position: role.position,
                permissions: role.permissions,
                everyone: role.everyone,
            },
        )
    }

    /// Whether this account may hand `bits` out.
    pub fn can_grant(&self, bits: u64) -> bool {
        permissions::can_grant(&self.hierarchy(), &self.member_def(self.me), bits)
    }

    pub fn channel(&self, id: i64) -> Option<&Channel> {
        self.channels.get(&id)
    }

    pub fn is_general(&self, id: i64) -> bool {
        id != 0 && id == self.server.general_channel_id
    }

    /// The channel the window opens when it has no better idea: the one that was
    /// in view last, else general — the one nobody can delete and nobody can be
    /// denied — else the first text channel there is.
    pub fn fallback_channel(&self, last: i64) -> Option<i64> {
        if last != 0 && self.channels.contains_key(&last) {
            return Some(last);
        }
        let general = self.server.general_channel_id;
        if general != 0 && self.channels.contains_key(&general) {
            return Some(general);
        }
        self.channels_of_kind(ChannelKind::Text)
            .first()
            .map(|channel| channel.id)
    }

    /// The categories in sidebar order.
    pub fn ordered_categories(&self) -> Vec<&Category> {
        let mut categories: Vec<&Category> = self.categories.values().collect();
        categories.sort_by_key(|category| (category.position, category.id));
        categories
    }

    /// The channels of one category — `None` being the group with no category —
    /// in sidebar order. DMs are never in it.
    pub fn channels_in(&self, category: Option<i64>) -> Vec<&Channel> {
        let wanted = category.unwrap_or(0);
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| {
                channel_kind(channel) != ChannelKind::Dm && channel.category_id == wanted
            })
            .collect();
        channels.sort_by_key(|channel| (channel.position, channel.id));
        channels
    }

    /// Every channel of one kind, in sidebar order across categories.
    pub fn channels_of_kind(&self, kind: ChannelKind) -> Vec<&Channel> {
        let order: BTreeMap<i64, i32> = self
            .categories
            .values()
            .map(|category| (category.id, category.position))
            .collect();
        let mut channels: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel_kind(channel) == kind)
            .collect();
        channels.sort_by_key(|channel| {
            (
                order.get(&channel.category_id).copied().unwrap_or(i32::MAX),
                channel.position,
                channel.id,
            )
        });
        channels
    }

    /// This account's DM channels, newest id first.
    pub fn dms(&self) -> Vec<&Channel> {
        let mut dms: Vec<&Channel> = self
            .channels
            .values()
            .filter(|channel| channel_kind(channel) == ChannelKind::Dm)
            .collect();
        dms.sort_by_key(|channel| std::cmp::Reverse(channel.id));
        dms
    }

    /// The DM with one member, whether or not it is hidden from the list.
    pub fn dm_with(&self, user_id: i64) -> Option<i64> {
        self.channels
            .values()
            .find(|channel| {
                channel_kind(channel) == ChannelKind::Dm
                    && channel.dm_member_ids.contains(&user_id)
                    && channel.dm_member_ids.contains(&self.me)
            })
            .map(|channel| channel.id)
    }

    /// The other person in a DM.
    pub fn dm_partner(&self, channel: &Channel) -> Option<i64> {
        channel
            .dm_member_ids
            .iter()
            .copied()
            .find(|user_id| *user_id != self.me)
            .or_else(|| channel.dm_member_ids.first().copied())
    }

    /// What the header and the sidebar call a channel.
    pub fn channel_title(&self, id: i64) -> String {
        let Some(channel) = self.channels.get(&id) else {
            return "unknown".to_owned();
        };
        if channel_kind(channel) == ChannelKind::Dm {
            return match self.dm_partner(channel) {
                Some(user_id) => self.display_name(user_id).to_owned(),
                None => "Direct message".to_owned(),
            };
        }
        channel.name.clone()
    }

    /// The name a member is shown under: their nickname, else their username.
    pub fn display_name(&self, user_id: i64) -> &str {
        match self.members.get(&user_id) {
            Some(member) if !member.nickname.is_empty() => member.nickname.as_str(),
            Some(member) => member.username.as_str(),
            None => "unknown",
        }
    }

    pub fn is_online(&self, user_id: i64) -> bool {
        self.members
            .get(&user_id)
            .is_some_and(|member| member.online)
    }

    /// The colour a member's name is painted in: the highest-positioned role of
    /// theirs that has one. `0` is no colour of its own.
    pub fn member_color(&self, user_id: i64) -> u32 {
        let Some(member) = self.members.get(&user_id) else {
            return 0;
        };
        let mut best: Option<(i32, u32)> = None;
        for role in member
            .role_ids
            .iter()
            .filter_map(|id| self.roles.get(id))
            .chain(self.everyone())
        {
            if role.color == 0 {
                continue;
            }
            if best.is_none_or(|(position, _)| role.position > position) {
                best = Some((role.position, role.color));
            }
        }
        best.map_or(0, |(_, color)| color)
    }

    /// The member's highest-positioned role that carries an icon, for the row.
    pub fn member_badge_role(&self, user_id: i64) -> Option<&Role> {
        let member = self.members.get(&user_id)?;
        member
            .role_ids
            .iter()
            .filter_map(|id| self.roles.get(id))
            .filter(|role| !role.icon_emoji.is_empty() || role.icon_image_id != 0)
            .max_by_key(|role| (role.position, role.id))
    }

    /// Every member that may see `channel_id`, which is what the member pane
    /// lists. Sorted by display name, case-insensitively.
    pub fn members_of(&self, channel_id: i64) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .members
            .keys()
            .copied()
            .filter(|user_id| {
                permissions::has(
                    self.resolve(*user_id, Some(channel_id)),
                    permissions::VIEW_CHANNEL,
                )
            })
            .collect();
        ids.sort_by_key(|user_id| self.display_name(*user_id).to_lowercase());
        ids
    }

    /// The member pane's groups: every hoisted role by position, highest first,
    /// then everyone else who is online, then the offline.
    pub fn member_groups(&self, channel_id: i64) -> Vec<MemberGroup> {
        let viewers = self.members_of(channel_id);

        let mut hoisted: Vec<&Role> = self
            .roles
            .values()
            .filter(|role| role.hoist && !role.everyone)
            .collect();
        hoisted.sort_by_key(|role| std::cmp::Reverse((role.position, role.id)));

        let mut groups: Vec<MemberGroup> = Vec::new();
        let mut placed: BTreeSet<i64> = BTreeSet::new();
        for role in hoisted {
            let members: Vec<i64> = viewers
                .iter()
                .copied()
                .filter(|user_id| !placed.contains(user_id) && self.is_online(*user_id))
                .filter(|user_id| {
                    self.members
                        .get(user_id)
                        .is_some_and(|member| member.role_ids.contains(&role.id))
                })
                .collect();
            if members.is_empty() {
                continue;
            }
            placed.extend(members.iter().copied());
            groups.push(MemberGroup {
                label: role.name.clone(),
                members,
            });
        }

        let online: Vec<i64> = viewers
            .iter()
            .copied()
            .filter(|user_id| !placed.contains(user_id) && self.is_online(*user_id))
            .collect();
        if !online.is_empty() {
            groups.push(MemberGroup {
                label: "Online".to_owned(),
                members: online,
            });
        }

        let offline: Vec<i64> = viewers
            .iter()
            .copied()
            .filter(|user_id| !self.is_online(*user_id))
            .collect();
        if !offline.is_empty() {
            groups.push(MemberGroup {
                label: "Offline".to_owned(),
                members: offline,
            });
        }
        groups
    }

    /// Every image the panes draw: the server's icon, each role's, and every
    /// member's avatar and banner. `0` is no image and is left out.
    pub fn image_ids(&self) -> Vec<i64> {
        std::iter::once(self.server.icon_image_id)
            .chain(self.roles.values().map(|role| role.icon_image_id))
            .chain(
                self.members
                    .values()
                    .flat_map(|member| [member.avatar_image_id, member.banner_image_id]),
            )
            .filter(|id| *id != 0)
            .collect()
    }

    /// Every member as `vorcall_core::mentions` takes them.
    pub fn mention_pairs(&self) -> Vec<(i64, String)> {
        self.members
            .values()
            .map(|member| (member.user_id, member.username.clone()))
            .collect()
    }
}

/// The kind one channel carries. prost leaves an enumeration field an `i32` and
/// generates no accessor, so every read goes through this rather than comparing
/// numbers at the call site.
pub fn channel_kind(channel: &Channel) -> ChannelKind {
    ChannelKind::try_from(channel.kind).unwrap_or(ChannelKind::Unspecified)
}

#[cfg(test)]
mod tests {
    use vorcall_core::Override;
    use vorcall_core::permissions as perms;

    use super::*;

    fn role(id: i64, position: i32, permissions: u64) -> Role {
        Role {
            id,
            name: format!("role{id}"),
            color: 0,
            icon_emoji: String::new(),
            icon_image_id: 0,
            position,
            permissions,
            hoist: false,
            everyone: false,
        }
    }

    fn member(user_id: i64, role_ids: Vec<i64>) -> Profile {
        Profile {
            user_id,
            username: format!("user{user_id}"),
            nickname: String::new(),
            avatar_image_id: 0,
            banner_image_id: 0,
            description: String::new(),
            accent_color: 0,
            role_ids,
            online: true,
            server_muted: false,
            server_deafened: false,
        }
    }

    fn channel(id: i64, kind: ChannelKind, name: &str, category: i64, position: i32) -> Channel {
        Channel {
            id,
            kind: kind as i32,
            name: name.to_owned(),
            topic: String::new(),
            category_id: category,
            position,
            overrides: Vec::new(),
            dm_member_ids: Vec::new(),
        }
    }

    /// One category with a text and a voice channel, an everyone role with the
    /// defaults, and two members.
    fn model() -> ServerModel {
        let mut everyone = role(1, 0, perms::EVERYONE_DEFAULT);
        everyone.everyone = true;
        everyone.name = "everyone".to_owned();

        let mut model = ServerModel {
            me: 7,
            ..ServerModel::default()
        };
        model.apply_snapshot(ServerSnapshot {
            server: Some(Server {
                name: "Vorcall".to_owned(),
                description: String::new(),
                icon_image_id: 0,
                owner_id: 9,
                general_channel_id: 10,
            }),
            roles: vec![everyone, role(2, 1, perms::MANAGE_CHANNELS)],
            categories: vec![
                Category {
                    id: 1,
                    name: "General".to_owned(),
                    position: 0,
                },
                Category {
                    id: 2,
                    name: "Other".to_owned(),
                    position: 1,
                },
            ],
            channels: vec![
                channel(11, ChannelKind::Voice, "General", 1, 1),
                channel(10, ChannelKind::Text, "general", 1, 0),
                channel(12, ChannelKind::Text, "music", 2, 0),
            ],
            members: vec![member(7, vec![2]), member(9, vec![])],
            read_states: vec![ReadState {
                channel_id: 10,
                unread: 3,
                mentions: 1,
                last_message_id: 40,
            }],
            sounds: vec![],
            stickers: vec![],
        });
        model
    }

    #[test]
    fn the_sidebar_order_is_position_then_id() {
        let model = model();

        let categories: Vec<i64> = model
            .ordered_categories()
            .into_iter()
            .map(|category| category.id)
            .collect();
        assert_eq!(categories, [1, 2]);

        let channels: Vec<i64> = model
            .channels_in(Some(1))
            .into_iter()
            .map(|channel| channel.id)
            .collect();
        assert_eq!(channels, [10, 11]);
        assert!(model.channels_in(None).is_empty());

        let text: Vec<i64> = model
            .channels_of_kind(ChannelKind::Text)
            .into_iter()
            .map(|channel| channel.id)
            .collect();
        assert_eq!(text, [10, 12]);
    }

    /// A channel this account may no longer see is gone from the next list, and
    /// the channel nobody can delete is what the window falls back to.
    #[test]
    fn a_channel_list_without_the_channel_in_view_falls_back_to_general() {
        let mut model = model();

        assert_eq!(model.fallback_channel(12), Some(12));

        model.delete_channel(12);
        assert_eq!(model.fallback_channel(12), Some(10));

        // Without a general channel the first text channel in sidebar order is
        // all there is to fall back to, and then nothing at all.
        model.server.general_channel_id = 0;
        assert_eq!(model.fallback_channel(0), Some(10));
        model.delete_channel(10);
        assert_eq!(model.fallback_channel(0), None);
    }

    #[test]
    fn the_everyone_defaults_reach_a_member_with_no_roles() {
        let model = model();

        assert!(permissions::has(
            model.resolve(9, Some(10)),
            perms::SEND_MESSAGES
        ));
        // The owner bypasses everything, whatever the roles say.
        assert_eq!(model.resolve(9, None), perms::ALL);
        // A role of one's own is added on top of the everyone defaults.
        assert!(model.can(perms::MANAGE_CHANNELS, None));
        assert!(model.can(perms::SEND_MESSAGES, Some(10)));
    }

    #[test]
    fn a_deny_on_a_channel_hides_it_from_the_member_pane() {
        let mut model = model();
        let mut music = model.channel(12).expect("the channel is there").clone();
        music.overrides = vec![Override {
            role_id: 2,
            user_id: 0,
            allow: 0,
            deny: perms::VIEW_CHANNEL,
        }];
        model.upsert_channel(music);

        // The deny is on this account's own role.
        assert!(!model.can(perms::VIEW_CHANNEL, Some(12)));
        assert_eq!(model.members_of(12), vec![9]);
        // general can never be hidden, and everyone can see it.
        assert_eq!(model.members_of(10), vec![7, 9]);
    }

    #[test]
    fn the_member_pane_groups_hoisted_roles_then_the_rest() {
        let mut model = model();
        let mut mods = model.roles.get(&2).expect("the role is there").clone();
        mods.hoist = true;
        mods.name = "Mods".to_owned();
        mods.color = 0x00_C8_10_2E;
        model.upsert_role(mods);
        let mut offline = model.members.get(&9).expect("the member is there").clone();
        offline.online = false;
        model.upsert_member(offline);

        let groups = model.member_groups(10);
        let shape: Vec<(&str, Vec<i64>)> = groups
            .iter()
            .map(|group| (group.label.as_str(), group.members.clone()))
            .collect();
        assert_eq!(shape, [("Mods", vec![7]), ("Offline", vec![9])]);
        assert_eq!(model.member_color(7), 0x00_C8_10_2E);
        assert_eq!(model.member_color(9), 0);
    }

    #[test]
    fn a_dm_is_titled_by_the_other_member() {
        let mut model = model();
        let mut dm = channel(20, ChannelKind::Dm, "", 0, 0);
        dm.dm_member_ids = vec![7, 9];
        model.upsert_channel(dm);

        assert_eq!(model.channel_title(20), "user9");
        assert_eq!(model.channel_title(10), "general");
        assert_eq!(model.channel_title(99), "unknown");
        let dms: Vec<i64> = model.dms().into_iter().map(|channel| channel.id).collect();
        assert_eq!(dms, [20]);
    }

    #[test]
    fn the_dm_with_a_member_is_found_by_both_ids() {
        let mut model = model();
        let mut dm = channel(20, ChannelKind::Dm, "", 0, 0);
        dm.dm_member_ids = vec![7, 9];
        model.upsert_channel(dm);
        let mut other = channel(21, ChannelKind::Dm, "", 0, 0);
        // A DM between two other people: never this account's.
        other.dm_member_ids = vec![9, 11];
        model.upsert_channel(other);

        assert_eq!(model.dm_with(9), Some(20));
        assert_eq!(model.dm_with(11), None);
    }

    #[test]
    fn the_images_to_fetch_are_the_ones_the_panes_draw() {
        let mut model = model();
        let mut server = model.server.clone();
        server.icon_image_id = 1;
        model.server = server;
        let mut badge = model.roles.get(&2).expect("the role is there").clone();
        badge.icon_image_id = 2;
        model.upsert_role(badge);
        let mut member = model.members.get(&7).expect("the member is there").clone();
        member.avatar_image_id = 3;
        member.banner_image_id = 4;
        model.upsert_member(member);

        let mut ids = model.image_ids();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 4]);
    }

    #[test]
    fn a_nickname_is_what_a_member_is_called() {
        let mut model = model();
        let mut renamed = model.members.get(&9).expect("the member is there").clone();
        renamed.nickname = "Ana".to_owned();
        model.upsert_member(renamed);

        assert_eq!(model.display_name(9), "Ana");
        assert_eq!(model.display_name(7), "user7");
        assert_eq!(model.display_name(404), "unknown");
    }
}

//! The client's copy of the permission engine.
//!
//! `PROTOCOL.md` § Roles and permissions is the specification both sides
//! implement; this is a bit-for-bit mirror of `server/Permissions/`, tested
//! against the same matrix, so the UI can grey out what the server would refuse
//! instead of guessing. A permission set is a plain `u64` here — the generated
//! `Permission` enum only names the bits on the wire.

pub const MANAGE_SERVER: u64 = 1 << 0;
pub const MANAGE_CHANNELS: u64 = 1 << 1;
pub const MANAGE_ROLES: u64 = 1 << 2;
pub const MANAGE_MEMBERS: u64 = 1 << 3;
pub const MANAGE_MESSAGES: u64 = 1 << 4;
pub const MANAGE_INVITES: u64 = 1 << 5;
pub const KICK_MEMBERS: u64 = 1 << 6;
pub const BAN_MEMBERS: u64 = 1 << 7;
pub const VIEW_CHANNEL: u64 = 1 << 8;
pub const SEND_MESSAGES: u64 = 1 << 9;
pub const ATTACH_FILES: u64 = 1 << 10;
pub const ADD_REACTIONS: u64 = 1 << 11;
pub const MENTION_EVERYONE: u64 = 1 << 12;
pub const CONNECT: u64 = 1 << 13;
pub const SPEAK: u64 = 1 << 14;
pub const SHARE_SCREEN: u64 = 1 << 15;
pub const MUTE_MEMBERS: u64 = 1 << 16;
pub const DEAFEN_MEMBERS: u64 = 1 << 17;
pub const MOVE_MEMBERS: u64 = 1 << 18;
pub const PRIORITY_SPEAKER: u64 = 1 << 19;
pub const CHANGE_NICKNAME: u64 = 1 << 20;

/// Every bit the schema defines.
pub const ALL: u64 = 0x1F_FFFF;

/// Held server-wide: these never appear in a channel override, and a channel
/// never takes one away.
pub const SERVER_SCOPED: u64 = MANAGE_SERVER
    | MANAGE_ROLES
    | MANAGE_MEMBERS
    | MANAGE_INVITES
    | KICK_MEMBERS
    | BAN_MEMBERS
    | CHANGE_NICKNAME;

/// Everything an override may touch.
pub const CHANNEL_SCOPED: u64 = ALL & !SERVER_SCOPED;

/// What `@everyone` carries on a freshly seeded server.
pub const EVERYONE_DEFAULT: u64 = VIEW_CHANNEL
    | SEND_MESSAGES
    | ATTACH_FILES
    | ADD_REACTIONS
    | CONNECT
    | SPEAK
    | SHARE_SCREEN
    | CHANGE_NICKNAME;

/// Every bit with the spelling the wire uses, ascending: the proto constant
/// without its `PERMISSION_` prefix, which is also what the server sends as the
/// `detail` of a `PERMISSION_DENIED`.
pub const BITS: [(u64, &str); 21] = [
    (MANAGE_SERVER, "MANAGE_SERVER"),
    (MANAGE_CHANNELS, "MANAGE_CHANNELS"),
    (MANAGE_ROLES, "MANAGE_ROLES"),
    (MANAGE_MEMBERS, "MANAGE_MEMBERS"),
    (MANAGE_MESSAGES, "MANAGE_MESSAGES"),
    (MANAGE_INVITES, "MANAGE_INVITES"),
    (KICK_MEMBERS, "KICK_MEMBERS"),
    (BAN_MEMBERS, "BAN_MEMBERS"),
    (VIEW_CHANNEL, "VIEW_CHANNEL"),
    (SEND_MESSAGES, "SEND_MESSAGES"),
    (ATTACH_FILES, "ATTACH_FILES"),
    (ADD_REACTIONS, "ADD_REACTIONS"),
    (MENTION_EVERYONE, "MENTION_EVERYONE"),
    (CONNECT, "CONNECT"),
    (SPEAK, "SPEAK"),
    (SHARE_SCREEN, "SHARE_SCREEN"),
    (MUTE_MEMBERS, "MUTE_MEMBERS"),
    (DEAFEN_MEMBERS, "DEAFEN_MEMBERS"),
    (MOVE_MEMBERS, "MOVE_MEMBERS"),
    (PRIORITY_SPEAKER, "PRIORITY_SPEAKER"),
    (CHANGE_NICKNAME, "CHANGE_NICKNAME"),
];

/// Whether `set` carries every bit of `bit`.
pub fn has(set: u64, bit: u64) -> bool {
    set & bit == bit
}

/// Drops the bits this build does not define, exactly as the server's
/// `Perms.Clean` does: a newer peer's extra bit is ignored, never refused.
pub fn clean(raw: u64) -> u64 {
    raw & ALL
}

pub fn name(bit: u64) -> Option<&'static str> {
    BITS.iter()
        .find(|(candidate, _)| *candidate == bit)
        .map(|(_, name)| *name)
}

pub fn parse(name: &str) -> Option<u64> {
    BITS.iter()
        .find(|(_, candidate)| *candidate == name)
        .map(|(bit, _)| *bit)
}

/// The switch's title in the roles editor; empty for a bit this build does not
/// know.
pub fn label(bit: u64) -> &'static str {
    match bit {
        MANAGE_SERVER => "Manage server",
        MANAGE_CHANNELS => "Manage channels",
        MANAGE_ROLES => "Manage roles",
        MANAGE_MEMBERS => "Manage members",
        MANAGE_MESSAGES => "Manage messages",
        MANAGE_INVITES => "Manage invites",
        KICK_MEMBERS => "Kick members",
        BAN_MEMBERS => "Ban members",
        VIEW_CHANNEL => "View channel",
        SEND_MESSAGES => "Send messages",
        ATTACH_FILES => "Attach files",
        ADD_REACTIONS => "Add reactions",
        MENTION_EVERYONE => "Mention everyone",
        CONNECT => "Connect",
        SPEAK => "Speak",
        SHARE_SCREEN => "Share screen",
        MUTE_MEMBERS => "Mute members",
        DEAFEN_MEMBERS => "Deafen members",
        MOVE_MEMBERS => "Move members",
        PRIORITY_SPEAKER => "Priority speaker",
        CHANGE_NICKNAME => "Change nickname",
        _ => "",
    }
}

/// The line under the switch's title.
pub fn describe(bit: u64) -> &'static str {
    match bit {
        MANAGE_SERVER => "Change the server's name, description and icon",
        MANAGE_CHANNELS => "Create, edit, reorder and delete channels and categories",
        MANAGE_ROLES => "Create and edit roles below their own, and assign them",
        MANAGE_MEMBERS => "Change other members' nicknames and roles",
        MANAGE_MESSAGES => "Delete anyone's messages in the channel",
        MANAGE_INVITES => "Create, list and revoke invites",
        KICK_MEMBERS => "Remove a member from the server",
        BAN_MEMBERS => "Ban a member and erase their messages",
        VIEW_CHANNEL => "See the channel and read its messages",
        SEND_MESSAGES => "Post messages in the channel",
        ATTACH_FILES => "Attach images to a message",
        ADD_REACTIONS => "React to messages",
        MENTION_EVERYONE => "Use @everyone and @here",
        CONNECT => "Join the voice channel",
        SPEAK => "Talk in the voice channel",
        SHARE_SCREEN => "Share a screen or a window in the voice channel",
        MUTE_MEMBERS => "Server mute another member in voice",
        DEAFEN_MEMBERS => "Server deafen another member in voice",
        MOVE_MEMBERS => "Move another member between voice channels, or disconnect them",
        PRIORITY_SPEAKER => "Others are quieter while this member speaks",
        CHANGE_NICKNAME => "Change their own nickname",
        _ => "",
    }
}

/// One role, reduced to what resolution needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleDef {
    pub id: i64,
    pub position: i32,
    pub permissions: u64,
    pub everyone: bool,
}

/// One channel override. Exactly one of `role_id` / `user_id` is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverrideDef {
    pub role_id: Option<i64>,
    pub user_id: Option<i64>,
    pub allow: u64,
    pub deny: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ChannelDef<'a> {
    pub id: i64,
    /// The one channel nothing can hide.
    pub is_general: bool,
    pub overrides: &'a [OverrideDef],
}

/// A member, reduced to what resolution needs. `role_ids` need not list
/// `@everyone` — every member holds it.
#[derive(Debug, Clone, Copy)]
pub struct MemberDef<'a> {
    pub user_id: i64,
    pub role_ids: &'a [i64],
}

/// The server's roles in any order, plus the one account that bypasses every
/// check.
#[derive(Debug, Clone, Copy)]
pub struct Hierarchy<'a> {
    pub owner_id: i64,
    pub roles: &'a [RoleDef],
}

/// Why a member may or may not be acted upon. The server answers `FORBIDDEN`
/// for [`TargetVerdict::IsOwner`] and [`TargetVerdict::IsSelf`], `HIERARCHY`
/// for [`TargetVerdict::Outranked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetVerdict {
    Allowed,
    IsOwner,
    IsSelf,
    Outranked,
}

/// `member`'s permissions: server-wide with `channel` unset, inside that channel
/// otherwise — the ten steps of `PROTOCOL.md` § Resolution.
pub fn resolve(h: &Hierarchy<'_>, member: &MemberDef<'_>, channel: Option<ChannelDef<'_>>) -> u64 {
    if member.user_id == h.owner_id {
        return ALL;
    }

    let everyone = h.roles.iter().find(|role| role.everyone);
    let mut base = everyone.map_or(0, |role| role.permissions);
    for id in member.role_ids {
        if let Some(role) = h.roles.iter().find(|role| role.id == *id) {
            base |= role.permissions;
        }
    }
    let base = clean(base);

    let Some(channel) = channel else {
        return base;
    };

    let server_half = base & SERVER_SCOPED;
    let mut p = base & CHANNEL_SCOPED;

    if let Some(everyone) = everyone
        && let Some(entry) = role_override(&channel, everyone.id)
    {
        p = apply(p, entry);
    }

    // Ascending position, ties by id, applied one after another: a higher
    // role's override wins a conflict with a lower one.
    let mut held: Vec<&RoleDef> = h
        .roles
        .iter()
        .filter(|role| !role.everyone && member.role_ids.contains(&role.id))
        .collect();
    held.sort_by_key(|role| (role.position, role.id));
    for role in held {
        if let Some(entry) = role_override(&channel, role.id) {
            p = apply(p, entry);
        }
    }

    if let Some(entry) = channel
        .overrides
        .iter()
        .find(|entry| entry.user_id == Some(member.user_id))
    {
        p = apply(p, entry);
    }

    if channel.is_general {
        p |= VIEW_CHANNEL;
    }

    if p & VIEW_CHANNEL == 0 {
        return server_half;
    }

    p | server_half
}

fn role_override<'a>(channel: &ChannelDef<'a>, role_id: i64) -> Option<&'a OverrideDef> {
    channel
        .overrides
        .iter()
        .find(|entry| entry.role_id == Some(role_id))
}

/// An override only ever touches channel-scoped bits, whatever it carries.
fn apply(p: u64, entry: &OverrideDef) -> u64 {
    (p & !(entry.deny & CHANNEL_SCOPED)) | (entry.allow & CHANNEL_SCOPED)
}

/// The greatest position among the member's roles; `0` when it holds only
/// `@everyone`.
pub fn highest_position(h: &Hierarchy<'_>, member: &MemberDef<'_>) -> i32 {
    h.roles
        .iter()
        .filter(|role| !role.everyone && member.role_ids.contains(&role.id))
        .map(|role| role.position)
        // `@everyone` sits at 0 and every member holds it, so nothing below 0
        // can be a member's highest position.
        .fold(0, i32::max)
}

pub fn is_owner(h: &Hierarchy<'_>, user_id: i64) -> bool {
    user_id == h.owner_id
}

/// Whether `actor` may create, edit, delete, reorder or assign `target`.
///
/// `@everyone`'s permissions are the exception to the hierarchy: any
/// `MANAGE_ROLES` holder may edit them.
pub fn can_manage_role(h: &Hierarchy<'_>, actor: &MemberDef<'_>, target: &RoleDef) -> bool {
    if is_owner(h, actor.user_id) {
        return true;
    }
    if !has(resolve(h, actor, None), MANAGE_ROLES) {
        return false;
    }
    target.everyone || target.position < highest_position(h, actor)
}

/// Whether `actor` may put `bits` into a role or an override.
pub fn can_grant(h: &Hierarchy<'_>, actor: &MemberDef<'_>, bits: u64) -> bool {
    missing_grant(h, actor, bits).is_none()
}

/// The lowest bit of `bits` that `actor` does not hold itself, which is the one
/// the server names in its refusal.
pub fn missing_grant(h: &Hierarchy<'_>, actor: &MemberDef<'_>, bits: u64) -> Option<u64> {
    if is_owner(h, actor.user_id) {
        return None;
    }
    let held = resolve(h, actor, None);
    BITS.iter()
        .map(|(bit, _)| *bit)
        .find(|bit| bits & bit != 0 && held & bit == 0)
}

/// Whether `actor` may kick, ban, rename, moderate or re-role `target`.
pub fn can_target(
    h: &Hierarchy<'_>,
    actor: &MemberDef<'_>,
    target: &MemberDef<'_>,
) -> TargetVerdict {
    if is_owner(h, target.user_id) {
        return TargetVerdict::IsOwner;
    }
    if target.user_id == actor.user_id {
        return TargetVerdict::IsSelf;
    }
    if is_owner(h, actor.user_id) {
        return TargetVerdict::Allowed;
    }
    if highest_position(h, actor) > highest_position(h, target) {
        return TargetVerdict::Allowed;
    }
    TargetVerdict::Outranked
}

/// Whether an override the UI is about to send is well formed: exactly one
/// subject, and no bit both allowed and denied.
///
/// Both halves are masked first, like the server's: an overlap that is only
/// server-scoped is not an overlap, because neither side of it ever reaches a
/// channel.
pub fn is_valid_override(entry: &OverrideDef) -> bool {
    entry.role_id.is_some() != entry.user_id.is_some()
        && (entry.allow & CHANNEL_SCOPED) & (entry.deny & CHANNEL_SCOPED) == 0
}

/// The one override the server always refuses: `@everyone` losing sight of
/// `general`.
pub fn denies_general_view(
    channel: &ChannelDef<'_>,
    candidate: &OverrideDef,
    everyone_role_id: i64,
) -> bool {
    channel.is_general
        && candidate.role_id == Some(everyone_role_id)
        && has(candidate.deny, VIEW_CHANNEL)
}

/// Where a role `actor` creates lands: at its own highest position, or just
/// above `@everyone` when it holds nothing else.
pub fn insert_position(h: &Hierarchy<'_>, actor: &MemberDef<'_>) -> i32 {
    let highest = highest_position(h, actor);
    if highest > 0 { highest } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER_ID: i64 = 99;
    const EVERYONE_ID: i64 = 1;
    const LOW_ROLE_ID: i64 = 2;
    const HIGH_ROLE_ID: i64 = 3;
    const ACTOR_ID: i64 = 10;
    const OTHER_ID: i64 = 11;
    const CHANNEL_ID: i64 = 7;

    /// The 21 defined bits and the 7 server-scoped ones, listed here rather than
    /// taken from the engine so the matrix never agrees with the code by
    /// construction.
    const ALL_BITS: [u64; 21] = [
        MANAGE_SERVER,
        MANAGE_CHANNELS,
        MANAGE_ROLES,
        MANAGE_MEMBERS,
        MANAGE_MESSAGES,
        MANAGE_INVITES,
        KICK_MEMBERS,
        BAN_MEMBERS,
        VIEW_CHANNEL,
        SEND_MESSAGES,
        ATTACH_FILES,
        ADD_REACTIONS,
        MENTION_EVERYONE,
        CONNECT,
        SPEAK,
        SHARE_SCREEN,
        MUTE_MEMBERS,
        DEAFEN_MEMBERS,
        MOVE_MEMBERS,
        PRIORITY_SPEAKER,
        CHANGE_NICKNAME,
    ];

    const SERVER_SCOPED_BITS: [u64; 7] = [
        MANAGE_SERVER,
        MANAGE_ROLES,
        MANAGE_MEMBERS,
        MANAGE_INVITES,
        KICK_MEMBERS,
        BAN_MEMBERS,
        CHANGE_NICKNAME,
    ];

    /// Which layer of `PROTOCOL.md` § Resolution a matrix case puts its single
    /// override on, as the factory that builds it.
    type MakeOverride = fn(u64, u64) -> OverrideDef;

    // ---- constants -----------------------------------------------------------------------

    #[test]
    fn every_bit_keeps_its_wire_value_and_its_wire_name() {
        let rows: [(u64, u64, &str); 21] = [
            (MANAGE_SERVER, 1, "MANAGE_SERVER"),
            (MANAGE_CHANNELS, 2, "MANAGE_CHANNELS"),
            (MANAGE_ROLES, 4, "MANAGE_ROLES"),
            (MANAGE_MEMBERS, 8, "MANAGE_MEMBERS"),
            (MANAGE_MESSAGES, 16, "MANAGE_MESSAGES"),
            (MANAGE_INVITES, 32, "MANAGE_INVITES"),
            (KICK_MEMBERS, 64, "KICK_MEMBERS"),
            (BAN_MEMBERS, 128, "BAN_MEMBERS"),
            (VIEW_CHANNEL, 256, "VIEW_CHANNEL"),
            (SEND_MESSAGES, 512, "SEND_MESSAGES"),
            (ATTACH_FILES, 1024, "ATTACH_FILES"),
            (ADD_REACTIONS, 2048, "ADD_REACTIONS"),
            (MENTION_EVERYONE, 4096, "MENTION_EVERYONE"),
            (CONNECT, 8192, "CONNECT"),
            (SPEAK, 16384, "SPEAK"),
            (SHARE_SCREEN, 32768, "SHARE_SCREEN"),
            (MUTE_MEMBERS, 65536, "MUTE_MEMBERS"),
            (DEAFEN_MEMBERS, 131072, "DEAFEN_MEMBERS"),
            (MOVE_MEMBERS, 262144, "MOVE_MEMBERS"),
            (PRIORITY_SPEAKER, 524288, "PRIORITY_SPEAKER"),
            (CHANGE_NICKNAME, 1048576, "CHANGE_NICKNAME"),
        ];

        for (bit, value, spelling) in rows {
            assert_eq!(bit, value, "{spelling}");
            assert_eq!(name(bit), Some(spelling));
            assert_eq!(parse(spelling), Some(bit));
        }
    }

    #[test]
    fn the_scopes_partition_the_twenty_one_defined_bits() {
        let all = ALL_BITS.iter().fold(0u64, |acc, bit| acc | bit);
        let server_scoped = SERVER_SCOPED_BITS.iter().fold(0u64, |acc, bit| acc | bit);

        // 1 | 4 | 8 | 32 | 64 | 128 | 1048576 = 1048813, which leaves 1048338 channel-scoped.
        assert_eq!(all, 0x1F_FFFF);
        assert_eq!(server_scoped, 1_048_813);
        assert_eq!(all & !server_scoped, 1_048_338);

        assert_eq!(ALL, 0x1F_FFFF);
        assert_eq!(SERVER_SCOPED, 1_048_813);
        assert_eq!(CHANNEL_SCOPED, 1_048_338);
        assert_eq!(SERVER_SCOPED & CHANNEL_SCOPED, 0);
        assert_eq!(SERVER_SCOPED | CHANNEL_SCOPED, ALL);
    }

    #[test]
    fn everyone_default_is_view_send_attach_react_connect_speak_share_and_nickname() {
        let expected = VIEW_CHANNEL
            | SEND_MESSAGES
            | ATTACH_FILES
            | ADD_REACTIONS
            | CONNECT
            | SPEAK
            | SHARE_SCREEN
            | CHANGE_NICKNAME;

        assert_eq!(expected, 1_109_760);
        assert_eq!(EVERYONE_DEFAULT, 1_109_760);
    }

    #[test]
    fn has_asks_for_every_bit_of_its_argument_and_clean_drops_undefined_bits() {
        assert!(has(VIEW_CHANNEL | SEND_MESSAGES, SEND_MESSAGES));
        assert!(!has(VIEW_CHANNEL, SEND_MESSAGES));
        assert!(has(
            VIEW_CHANNEL | SEND_MESSAGES,
            VIEW_CHANNEL | SEND_MESSAGES
        ));
        assert!(!has(SEND_MESSAGES, VIEW_CHANNEL | SEND_MESSAGES));

        assert_eq!(clean(SEND_MESSAGES | (1 << 21) | (1 << 63)), SEND_MESSAGES);
        assert_eq!(clean(u64::MAX), 0x1F_FFFF);
    }

    #[test]
    fn name_has_nothing_to_say_about_a_mask_that_is_not_one_defined_bit() {
        assert_eq!(name(0), None);
        assert_eq!(name(VIEW_CHANNEL | SEND_MESSAGES), None);
        assert_eq!(name(1 << 21), None);
    }

    #[test]
    fn try_parse_refuses_a_name_it_does_not_know() {
        assert_eq!(parse("send_messages"), None);
        assert_eq!(parse("PERMISSION_SEND_MESSAGES"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn bits_yields_the_defined_bits_of_a_mask_in_ascending_order() {
        assert_eq!(
            bits(MANAGE_SERVER | VIEW_CHANNEL | CHANGE_NICKNAME),
            vec![MANAGE_SERVER, VIEW_CHANNEL, CHANGE_NICKNAME]
        );
        assert_eq!(bits(0x1F_FFFF), ALL_BITS.to_vec());
        assert!(bits(0).is_empty());
        assert_eq!(
            bits(SEND_MESSAGES | (1 << 21) | (1 << 40)),
            vec![SEND_MESSAGES]
        );
    }

    // ---- resolution ----------------------------------------------------------------------

    #[test]
    fn the_owner_resolves_every_bit_however_the_overrides_deny() {
        let roles = [everyone_role(0), role(LOW_ROLE_ID, 1, 0)];
        let h = hierarchy(OWNER_ID, &roles);
        let owner = member(OWNER_ID, &[]);
        let overrides = [
            role_override(EVERYONE_ID, 0, CHANNEL_SCOPED),
            user_override(OWNER_ID, 0, CHANNEL_SCOPED),
        ];

        let resolved = resolve(&h, &owner, Some(chan(CHANNEL_ID, false, &overrides)));

        assert_eq!(resolved, 0x1F_FFFF);
        assert_eq!(resolve(&h, &owner, None), 0x1F_FFFF);
        for bit in ALL_BITS {
            assert!(has(resolved, bit), "{:?}", name(bit));
        }
    }

    #[test]
    fn everyone_is_the_base_of_a_member_that_holds_no_other_role() {
        let roles = [everyone_role(1_109_760)];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(resolve(&h, &member(ACTOR_ID, &[]), None), 1_109_760);
    }

    #[test]
    fn the_base_is_everyone_unioned_with_every_role_the_member_holds() {
        // 256 | 512 | 8192 = 8960.
        let roles = [
            everyone_role(VIEW_CHANNEL),
            role(LOW_ROLE_ID, 1, SEND_MESSAGES),
            role(HIGH_ROLE_ID, 2, CONNECT),
        ];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(
            resolve(&h, &member(ACTOR_ID, &[LOW_ROLE_ID, HIGH_ROLE_ID]), None),
            8960
        );
    }

    #[test]
    fn a_role_id_that_names_no_role_is_ignored() {
        // 256 | 512 = 768; role 777 does not exist.
        let roles = [
            everyone_role(VIEW_CHANNEL),
            role(LOW_ROLE_ID, 1, SEND_MESSAGES),
        ];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(
            resolve(&h, &member(ACTOR_ID, &[LOW_ROLE_ID, 777]), None),
            768
        );
    }

    #[test]
    fn a_server_level_resolve_never_looks_at_a_channel() {
        let roles = [everyone_role(VIEW_CHANNEL | SEND_MESSAGES)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);
        let overrides = [role_override(EVERYONE_ID, 0, VIEW_CHANNEL | SEND_MESSAGES)];

        assert_eq!(resolve(&h, &actor, None), 768);
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &overrides))),
            0
        );
    }

    // 21 bits x 3 layers x {allow, deny, inherit} x {base has the bit, base lacks it} = 378 cases.
    #[test]
    fn one_override_on_one_layer_resolves_exactly_as_the_protocol_says() {
        for bit in ALL_BITS {
            let server_scoped = SERVER_SCOPED_BITS.contains(&bit);
            for (layer, make) in layers() {
                for action in ["allow", "deny", "inherit"] {
                    for base_has_bit in [true, false] {
                        // A server-scoped bit cannot appear in an override, so only the base
                        // decides it; a channel-scoped one is whatever the single override says.
                        let present = if server_scoped || action == "inherit" {
                            base_has_bit
                        } else {
                            action == "allow"
                        };

                        // VIEW_CHANNEL is what keeps the channel-scoped half alive, and the base
                        // carries it except when it is itself the bit under test.
                        let surviving_view = if bit == VIEW_CHANNEL { 0 } else { VIEW_CHANNEL };
                        let expected = (if present { bit } else { 0 }) | surviving_view;

                        let roles = [
                            everyone_role(matrix_base(bit, base_has_bit)),
                            role(LOW_ROLE_ID, 1, 0),
                        ];
                        let h = hierarchy(OWNER_ID, &roles);
                        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);

                        let overrides: Vec<OverrideDef> = if action == "inherit" {
                            Vec::new()
                        } else {
                            vec![make(
                                if action == "allow" { bit } else { 0 },
                                if action == "deny" { bit } else { 0 },
                            )]
                        };

                        assert_eq!(
                            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &overrides))),
                            expected,
                            "{:?} {action} in the {layer} layer, base_has_bit = {base_has_bit}",
                            name(bit)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_higher_role_wins_a_conflict_and_the_listed_order_does_not_matter() {
        let everyone = everyone_role(VIEW_CHANNEL | SEND_MESSAGES);
        let low = role(LOW_ROLE_ID, 1, 0);
        let high = role(HIGH_ROLE_ID, 2, 0);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID, HIGH_ROLE_ID]);

        let low_allows = role_override(LOW_ROLE_ID, SEND_MESSAGES, 0);
        let high_denies = role_override(HIGH_ROLE_ID, 0, SEND_MESSAGES);

        // Position 2 is applied last, so SEND_MESSAGES ends up off and only VIEW_CHANNEL survives.
        let roles = [everyone, low, high];
        let overrides = [low_allows, high_denies];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &roles),
                &actor,
                Some(chan(CHANNEL_ID, false, &overrides))
            ),
            256
        );

        let shuffled_roles = [high, everyone, low];
        let shuffled_overrides = [high_denies, low_allows];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &shuffled_roles),
                &actor,
                Some(chan(CHANNEL_ID, false, &shuffled_overrides))
            ),
            256
        );

        // The reverse assignment flips the result: 256 | 512 = 768.
        let reversed = [
            role_override(LOW_ROLE_ID, 0, SEND_MESSAGES),
            role_override(HIGH_ROLE_ID, SEND_MESSAGES, 0),
        ];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &roles),
                &actor,
                Some(chan(CHANNEL_ID, false, &reversed))
            ),
            768
        );
    }

    #[test]
    fn two_roles_at_the_same_position_are_applied_by_ascending_id() {
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, 0),
            role(HIGH_ROLE_ID, 1, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID, HIGH_ROLE_ID]);

        // Role 3 is applied after role 2, so role 3's deny is the one that stands.
        let high_denies = [
            role_override(HIGH_ROLE_ID, 0, SEND_MESSAGES),
            role_override(LOW_ROLE_ID, SEND_MESSAGES, 0),
        ];
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &high_denies))),
            256
        );

        let high_allows = [
            role_override(HIGH_ROLE_ID, SEND_MESSAGES, 0),
            role_override(LOW_ROLE_ID, 0, SEND_MESSAGES),
        ];
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &high_allows))),
            768
        );
    }

    #[test]
    fn the_everyone_override_is_applied_before_the_role_overrides() {
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);

        // @everyone denies first, the role allows after: 256 | 512 = 768.
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, 0),
        ];
        let overrides = [
            role_override(EVERYONE_ID, 0, SEND_MESSAGES),
            role_override(LOW_ROLE_ID, SEND_MESSAGES, 0),
        ];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &roles),
                &actor,
                Some(chan(CHANNEL_ID, false, &overrides))
            ),
            768
        );

        // The other way round the role's deny is the later one, so SEND_MESSAGES ends up off.
        let view_only = [everyone_role(VIEW_CHANNEL), role(LOW_ROLE_ID, 1, 0)];
        let reversed = [
            role_override(EVERYONE_ID, SEND_MESSAGES, 0),
            role_override(LOW_ROLE_ID, 0, SEND_MESSAGES),
        ];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &view_only),
                &actor,
                Some(chan(CHANNEL_ID, false, &reversed))
            ),
            256
        );
    }

    #[test]
    fn the_member_override_beats_a_role_deny_and_a_role_allow() {
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);

        let allows = [
            role_override(LOW_ROLE_ID, 0, SEND_MESSAGES),
            user_override(ACTOR_ID, SEND_MESSAGES, 0),
        ];
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &allows))),
            768
        );

        let view_only = [everyone_role(VIEW_CHANNEL), role(LOW_ROLE_ID, 1, 0)];
        let denies = [
            role_override(LOW_ROLE_ID, SEND_MESSAGES, 0),
            user_override(ACTOR_ID, 0, SEND_MESSAGES),
        ];
        assert_eq!(
            resolve(
                &hierarchy(OWNER_ID, &view_only),
                &actor,
                Some(chan(CHANNEL_ID, false, &denies))
            ),
            256
        );
    }

    #[test]
    fn without_view_channel_only_the_server_scoped_half_of_the_base_survives() {
        // base = 256 | 512 | 64 | 16 = 848; KICK_MEMBERS is the only server-scoped bit in it.
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, KICK_MEMBERS | MANAGE_MESSAGES),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);
        let overrides = [role_override(EVERYONE_ID, 0, VIEW_CHANNEL)];

        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &overrides))),
            64
        );
    }

    #[test]
    fn general_keeps_view_channel_even_when_everyone_is_denied_it() {
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, KICK_MEMBERS | MANAGE_MESSAGES),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);
        let overrides = [role_override(EVERYONE_ID, 0, VIEW_CHANNEL)];

        // The deny is undone by the floor: the whole base, 848, comes back.
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, true, &overrides))),
            848
        );
    }

    #[test]
    fn the_general_view_floor_applies_after_the_member_override() {
        let roles = [
            everyone_role(VIEW_CHANNEL | SEND_MESSAGES),
            role(LOW_ROLE_ID, 1, KICK_MEMBERS | MANAGE_MESSAGES),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);
        let overrides = [user_override(ACTOR_ID, 0, VIEW_CHANNEL | SEND_MESSAGES)];

        // The member deny takes SEND_MESSAGES away for good but not VIEW_CHANNEL:
        // 16 | 256 | 64 = 336.
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, true, &overrides))),
            336
        );
    }

    #[test]
    fn a_malformed_override_is_masked_rather_than_thrown_on() {
        let roles = [everyone_role(VIEW_CHANNEL), role(LOW_ROLE_ID, 1, 0)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);

        // Server-scoped bits in an override are masked away, and an allow that overlaps its own
        // deny still resolves: 256 | 512 = 768.
        let malformed = OverrideDef {
            role_id: Some(EVERYONE_ID),
            user_id: Some(ACTOR_ID),
            allow: SEND_MESSAGES | KICK_MEMBERS,
            deny: SEND_MESSAGES | BAN_MEMBERS,
        };
        let overrides = [malformed];

        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &overrides))),
            768
        );
        assert!(!is_valid_override(&malformed));
    }

    // ---- hierarchy -----------------------------------------------------------------------

    #[test]
    fn highest_position_is_zero_without_roles_and_the_greatest_position_held_otherwise() {
        let roles = [
            everyone_role(0),
            role(2, 1, 0),
            role(3, 5, 0),
            role(4, 2, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(highest_position(&h, &member(ACTOR_ID, &[])), 0);
        assert_eq!(highest_position(&h, &member(ACTOR_ID, &[2])), 1);
        assert_eq!(highest_position(&h, &member(ACTOR_ID, &[2, 3, 4])), 5);
        assert_eq!(highest_position(&h, &member(ACTOR_ID, &[4])), 2);
        assert_eq!(highest_position(&h, &member(ACTOR_ID, &[777])), 0);

        // A member holding @everyone only is 0 whatever position that row claims to be at.
        let misplaced_roles = [RoleDef {
            id: EVERYONE_ID,
            position: 9,
            permissions: 0,
            everyone: true,
        }];
        let misplaced = hierarchy(OWNER_ID, &misplaced_roles);
        assert_eq!(highest_position(&misplaced, &member(ACTOR_ID, &[])), 0);
        assert_eq!(
            highest_position(&misplaced, &member(ACTOR_ID, &[EVERYONE_ID])),
            0
        );
    }

    #[test]
    fn is_owner_compares_the_user_id_against_the_server_owner() {
        let roles = [everyone_role(0)];
        let h = hierarchy(OWNER_ID, &roles);

        assert!(is_owner(&h, member(OWNER_ID, &[]).user_id));
        assert!(!is_owner(&h, member(ACTOR_ID, &[]).user_id));
    }

    #[test]
    fn the_owner_may_manage_a_role_above_every_role_it_holds() {
        let target = role(LOW_ROLE_ID, 5, 0);
        let roles = [everyone_role(0), target];
        let h = hierarchy(OWNER_ID, &roles);

        assert!(can_manage_role(&h, &member(OWNER_ID, &[]), &target));
    }

    #[test]
    fn a_manage_roles_holder_may_manage_only_a_role_strictly_below_it() {
        let actor_role = role(5, 5, MANAGE_ROLES);
        let below = role(4, 4, 0);
        let equal = role(6, 5, 0);
        let above = role(7, 6, 0);
        let roles = [everyone_role(0), actor_role, below, equal, above];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[5]);

        assert!(can_manage_role(&h, &actor, &below));
        assert!(!can_manage_role(&h, &actor, &equal));
        assert!(!can_manage_role(&h, &actor, &above));
        assert!(!can_manage_role(&h, &actor, &actor_role));
    }

    #[test]
    fn everyone_is_manageable_by_any_manage_roles_holder_however_low_it_sits() {
        let everyone = everyone_role(MANAGE_ROLES);
        let other = role(LOW_ROLE_ID, 1, 0);
        let roles = [everyone, other];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);

        assert_eq!(highest_position(&h, &actor), 0);
        assert!(can_manage_role(&h, &actor, &everyone));
        assert!(!can_manage_role(&h, &actor, &other));
    }

    #[test]
    fn a_member_without_manage_roles_may_manage_nothing() {
        let everyone = everyone_role(1_109_760);
        let roles = [everyone, role(LOW_ROLE_ID, 1, 0)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[LOW_ROLE_ID]);

        assert!(!can_manage_role(&h, &actor, &everyone));
        assert!(!can_manage_role(&h, &actor, &role(99, 0, 0)));
    }

    #[test]
    fn a_caller_may_grant_only_bits_it_holds_at_server_level() {
        let roles = [everyone_role(1_109_760)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);

        assert!(can_grant(&h, &actor, VIEW_CHANNEL | SEND_MESSAGES));
        assert!(can_grant(&h, &actor, 1_109_760));
        assert!(can_grant(&h, &actor, 0));
        assert_eq!(missing_grant(&h, &actor, 1_109_760), None);

        assert!(!can_grant(&h, &actor, SEND_MESSAGES | MANAGE_SERVER));
        assert!(!can_grant(&h, &actor, MANAGE_ROLES | BAN_MEMBERS));
    }

    #[test]
    fn missing_grant_names_the_lowest_bit_the_caller_lacks() {
        let roles = [everyone_role(1_109_760)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);

        let two_missing = missing_grant(&h, &actor, MANAGE_ROLES | BAN_MEMBERS);
        assert_eq!(two_missing, Some(MANAGE_ROLES));
        assert_eq!(name(two_missing.unwrap()), Some("MANAGE_ROLES"));

        let one_missing = missing_grant(&h, &actor, SEND_MESSAGES | MANAGE_SERVER);
        assert_eq!(one_missing, Some(MANAGE_SERVER));
    }

    #[test]
    fn the_owner_may_grant_every_bit() {
        let roles = [everyone_role(0)];
        let h = hierarchy(OWNER_ID, &roles);
        let owner = member(OWNER_ID, &[]);

        assert!(can_grant(&h, &owner, 0x1F_FFFF));
        assert_eq!(missing_grant(&h, &owner, 0x1F_FFFF), None);
    }

    #[test]
    fn an_undefined_bit_is_ignored_by_both_can_grant_and_missing_grant() {
        let roles = [everyone_role(1_109_760)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);

        // Cleaned away rather than refused, so the two answers can never disagree.
        assert!(can_grant(&h, &actor, 1 << 21));
        assert_eq!(missing_grant(&h, &actor, 1 << 21), None);

        for bits in [
            0,
            VIEW_CHANNEL,
            MANAGE_SERVER,
            1_109_760,
            0x1F_FFFF,
            u64::MAX,
        ] {
            assert_eq!(
                can_grant(&h, &actor, bits),
                missing_grant(&h, &actor, bits).is_none(),
                "{bits}"
            );
        }
    }

    #[test]
    fn the_owner_is_never_a_target_and_neither_is_yourself() {
        let roles = [
            everyone_role(0),
            role(LOW_ROLE_ID, 1, 0),
            role(HIGH_ROLE_ID, 2, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[HIGH_ROLE_ID]);

        assert_eq!(
            can_target(&h, &actor, &member(OWNER_ID, &[])),
            TargetVerdict::IsOwner
        );
        assert_eq!(
            can_target(&h, &actor, &member(ACTOR_ID, &[HIGH_ROLE_ID])),
            TargetVerdict::IsSelf
        );
        assert_eq!(
            can_target(&h, &member(OWNER_ID, &[]), &member(OWNER_ID, &[])),
            TargetVerdict::IsOwner
        );
    }

    #[test]
    fn a_target_must_sit_strictly_below_the_caller() {
        let roles = [
            everyone_role(0),
            role(LOW_ROLE_ID, 1, 0),
            role(HIGH_ROLE_ID, 2, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(
            can_target(
                &h,
                &member(ACTOR_ID, &[HIGH_ROLE_ID]),
                &member(OTHER_ID, &[LOW_ROLE_ID])
            ),
            TargetVerdict::Allowed
        );
        assert_eq!(
            can_target(
                &h,
                &member(ACTOR_ID, &[LOW_ROLE_ID]),
                &member(OTHER_ID, &[LOW_ROLE_ID])
            ),
            TargetVerdict::Outranked
        );
        assert_eq!(
            can_target(&h, &member(ACTOR_ID, &[]), &member(OTHER_ID, &[])),
            TargetVerdict::Outranked
        );
        assert_eq!(
            can_target(
                &h,
                &member(ACTOR_ID, &[LOW_ROLE_ID]),
                &member(OTHER_ID, &[HIGH_ROLE_ID])
            ),
            TargetVerdict::Outranked
        );
        assert_eq!(
            can_target(
                &h,
                &member(OWNER_ID, &[]),
                &member(OTHER_ID, &[HIGH_ROLE_ID])
            ),
            TargetVerdict::Allowed
        );
    }

    #[test]
    fn an_override_targets_exactly_one_role_or_one_member() {
        assert!(is_valid_override(&role_override(
            EVERYONE_ID,
            SEND_MESSAGES,
            VIEW_CHANNEL
        )));
        assert!(is_valid_override(&user_override(
            ACTOR_ID,
            SEND_MESSAGES,
            VIEW_CHANNEL
        )));
        assert!(!is_valid_override(&OverrideDef {
            role_id: Some(EVERYONE_ID),
            user_id: Some(ACTOR_ID),
            allow: SEND_MESSAGES,
            deny: 0,
        }));
        assert!(!is_valid_override(&OverrideDef {
            role_id: None,
            user_id: None,
            allow: SEND_MESSAGES,
            deny: 0,
        }));
        assert!(!is_valid_override(&role_override(
            EVERYONE_ID,
            SEND_MESSAGES,
            SEND_MESSAGES
        )));

        // Both sides are masked first, so an overlap that is only server-scoped is not an overlap.
        assert!(is_valid_override(&role_override(
            EVERYONE_ID,
            KICK_MEMBERS,
            KICK_MEMBERS
        )));
    }

    #[test]
    fn only_everyone_losing_view_on_general_is_refused_at_write_time() {
        let none: [OverrideDef; 0] = [];
        let general = chan(CHANNEL_ID, true, &none);
        let plain = chan(CHANNEL_ID, false, &none);

        assert!(denies_general_view(
            &general,
            &role_override(EVERYONE_ID, 0, VIEW_CHANNEL),
            EVERYONE_ID
        ));
        assert!(denies_general_view(
            &general,
            &role_override(EVERYONE_ID, 0, VIEW_CHANNEL | SEND_MESSAGES),
            EVERYONE_ID
        ));
        assert!(!denies_general_view(
            &plain,
            &role_override(EVERYONE_ID, 0, VIEW_CHANNEL),
            EVERYONE_ID
        ));
        assert!(!denies_general_view(
            &general,
            &role_override(LOW_ROLE_ID, 0, VIEW_CHANNEL),
            EVERYONE_ID
        ));
        assert!(!denies_general_view(
            &general,
            &role_override(EVERYONE_ID, 0, SEND_MESSAGES),
            EVERYONE_ID
        ));
        assert!(!denies_general_view(
            &general,
            &user_override(ACTOR_ID, 0, VIEW_CHANNEL),
            EVERYONE_ID
        ));
    }

    #[test]
    fn a_new_role_goes_in_at_the_creators_own_position_or_just_above_everyone() {
        let roles = [
            everyone_role(0),
            role(LOW_ROLE_ID, 1, 0),
            role(HIGH_ROLE_ID, 2, 0),
        ];
        let h = hierarchy(OWNER_ID, &roles);

        assert_eq!(insert_position(&h, &member(ACTOR_ID, &[HIGH_ROLE_ID])), 2);
        assert_eq!(insert_position(&h, &member(ACTOR_ID, &[LOW_ROLE_ID])), 1);
        assert_eq!(insert_position(&h, &member(ACTOR_ID, &[])), 1);
        assert_eq!(insert_position(&h, &member(OWNER_ID, &[])), 1);
    }

    // ---- mirror-only ---------------------------------------------------------------------

    #[test]
    fn mirror_only_every_bit_has_ui_copy() {
        let mut labels = Vec::new();
        for (bit, spelling) in BITS {
            assert!(!label(bit).is_empty(), "{spelling} has no label");
            assert!(!describe(bit).is_empty(), "{spelling} has no description");
            labels.push(label(bit));
        }
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "two bits share a label");

        assert_eq!(label(1 << 21), "");
        assert_eq!(describe(1 << 21), "");
    }

    #[test]
    fn mirror_only_an_undefined_bit_in_a_role_is_dropped_at_resolve_time() {
        let undefined = 1 << 21;
        let roles = [everyone_role(VIEW_CHANNEL | undefined)];
        let h = hierarchy(OWNER_ID, &roles);
        let actor = member(ACTOR_ID, &[]);
        let none: [OverrideDef; 0] = [];

        assert_eq!(resolve(&h, &actor, None), VIEW_CHANNEL);
        assert_eq!(
            resolve(&h, &actor, Some(chan(CHANNEL_ID, false, &none))),
            VIEW_CHANNEL
        );
    }

    // ---- helpers -------------------------------------------------------------------------

    /// The defined bits of `mask`, ascending — the mirror's stand-in for the
    /// server's `PermNames.Bits`.
    fn bits(mask: u64) -> Vec<u64> {
        BITS.iter()
            .map(|(bit, _)| *bit)
            .filter(|bit| mask & bit != 0)
            .collect()
    }

    fn matrix_base(bit: u64, base_has_bit: bool) -> u64 {
        (if base_has_bit { bit } else { 0 }) | (if bit == VIEW_CHANNEL { 0 } else { VIEW_CHANNEL })
    }

    fn layers() -> [(&'static str, MakeOverride); 3] {
        [
            ("everyone", |allow, deny| {
                role_override(EVERYONE_ID, allow, deny)
            }),
            ("role", |allow, deny| {
                role_override(LOW_ROLE_ID, allow, deny)
            }),
            ("member", |allow, deny| user_override(ACTOR_ID, allow, deny)),
        ]
    }

    fn everyone_role(permissions: u64) -> RoleDef {
        RoleDef {
            id: EVERYONE_ID,
            position: 0,
            permissions,
            everyone: true,
        }
    }

    fn role(id: i64, position: i32, permissions: u64) -> RoleDef {
        RoleDef {
            id,
            position,
            permissions,
            everyone: false,
        }
    }

    fn member(user_id: i64, role_ids: &[i64]) -> MemberDef<'_> {
        MemberDef { user_id, role_ids }
    }

    fn chan(id: i64, is_general: bool, overrides: &[OverrideDef]) -> ChannelDef<'_> {
        ChannelDef {
            id,
            is_general,
            overrides,
        }
    }

    fn hierarchy(owner_id: i64, roles: &[RoleDef]) -> Hierarchy<'_> {
        Hierarchy { owner_id, roles }
    }

    fn role_override(role_id: i64, allow: u64, deny: u64) -> OverrideDef {
        OverrideDef {
            role_id: Some(role_id),
            user_id: None,
            allow,
            deny,
        }
    }

    fn user_override(user_id: i64, allow: u64, deny: u64) -> OverrideDef {
        OverrideDef {
            role_id: None,
            user_id: Some(user_id),
            allow,
            deny,
        }
    }
}

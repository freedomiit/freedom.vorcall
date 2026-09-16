using Vorcall.Server.Permissions;

namespace Vorcall.Server.Tests;

// The rules of PROTOCOL.md § Roles and permissions, rule by rule. Every expected value is read off
// that section or off proto/vorcall.proto's enum, never recomputed the way PermissionEngine
// computes it: the bit lists and the resolution outcomes below are spelled out by hand.
public class PermissionEngineTests
{
    // Which layer of PROTOCOL.md § Resolution a matrix case puts its single override on.
    public enum OverrideLayer
    {
        Everyone,
        Role,
        Member,
    }

    // What that override says about the bit under test; Inherit means no override at all.
    public enum OverrideAction
    {
        Allow,
        Deny,
        Inherit,
    }

    private const long OwnerId = 99;
    private const long EveryoneId = 1;
    private const long LowRoleId = 2;
    private const long HighRoleId = 3;
    private const long ActorId = 10;
    private const long OtherId = 11;
    private const long ChannelId = 7;

    private const ulong ManageServer = (ulong)Perm.ManageServer;        // 1
    private const ulong ManageRoles = (ulong)Perm.ManageRoles;          // 4
    private const ulong ManageMessages = (ulong)Perm.ManageMessages;    // 16
    private const ulong Kick = (ulong)Perm.KickMembers;                 // 64
    private const ulong Ban = (ulong)Perm.BanMembers;                   // 128
    private const ulong View = (ulong)Perm.ViewChannel;                 // 256
    private const ulong Send = (ulong)Perm.SendMessages;                // 512
    private const ulong Connect = (ulong)Perm.Connect;                  // 8192

    // The 25 defined bits and the 9 server-scoped ones, listed here rather than taken from Perms so
    // the matrix never agrees with the code by construction.
    private static readonly Perm[] AllBits =
    [
        Perm.ManageServer,
        Perm.ManageChannels,
        Perm.ManageRoles,
        Perm.ManageMembers,
        Perm.ManageMessages,
        Perm.ManageInvites,
        Perm.KickMembers,
        Perm.BanMembers,
        Perm.ViewChannel,
        Perm.SendMessages,
        Perm.AttachFiles,
        Perm.AddReactions,
        Perm.MentionEveryone,
        Perm.Connect,
        Perm.Speak,
        Perm.ShareScreen,
        Perm.MuteMembers,
        Perm.DeafenMembers,
        Perm.MoveMembers,
        Perm.PrioritySpeaker,
        Perm.ChangeNickname,
        Perm.Soundpad,
        Perm.ManageSounds,
        Perm.Video,
        Perm.ManageStickers,
    ];

    private static readonly Perm[] ServerScopedBits =
    [
        Perm.ManageServer,
        Perm.ManageRoles,
        Perm.ManageMembers,
        Perm.ManageInvites,
        Perm.KickMembers,
        Perm.BanMembers,
        Perm.ChangeNickname,
        Perm.ManageSounds,
        Perm.ManageStickers,
    ];

    // ---- constants -------------------------------------------------------------------------

    [Theory]
    [InlineData(Perm.ManageServer, 1UL, "MANAGE_SERVER")]
    [InlineData(Perm.ManageChannels, 2UL, "MANAGE_CHANNELS")]
    [InlineData(Perm.ManageRoles, 4UL, "MANAGE_ROLES")]
    [InlineData(Perm.ManageMembers, 8UL, "MANAGE_MEMBERS")]
    [InlineData(Perm.ManageMessages, 16UL, "MANAGE_MESSAGES")]
    [InlineData(Perm.ManageInvites, 32UL, "MANAGE_INVITES")]
    [InlineData(Perm.KickMembers, 64UL, "KICK_MEMBERS")]
    [InlineData(Perm.BanMembers, 128UL, "BAN_MEMBERS")]
    [InlineData(Perm.ViewChannel, 256UL, "VIEW_CHANNEL")]
    [InlineData(Perm.SendMessages, 512UL, "SEND_MESSAGES")]
    [InlineData(Perm.AttachFiles, 1024UL, "ATTACH_FILES")]
    [InlineData(Perm.AddReactions, 2048UL, "ADD_REACTIONS")]
    [InlineData(Perm.MentionEveryone, 4096UL, "MENTION_EVERYONE")]
    [InlineData(Perm.Connect, 8192UL, "CONNECT")]
    [InlineData(Perm.Speak, 16384UL, "SPEAK")]
    [InlineData(Perm.ShareScreen, 32768UL, "SHARE_SCREEN")]
    [InlineData(Perm.MuteMembers, 65536UL, "MUTE_MEMBERS")]
    [InlineData(Perm.DeafenMembers, 131072UL, "DEAFEN_MEMBERS")]
    [InlineData(Perm.MoveMembers, 262144UL, "MOVE_MEMBERS")]
    [InlineData(Perm.PrioritySpeaker, 524288UL, "PRIORITY_SPEAKER")]
    [InlineData(Perm.ChangeNickname, 1048576UL, "CHANGE_NICKNAME")]
    [InlineData(Perm.Soundpad, 2097152UL, "SOUNDPAD")]
    [InlineData(Perm.ManageSounds, 4194304UL, "MANAGE_SOUNDS")]
    [InlineData(Perm.Video, 8388608UL, "VIDEO")]
    [InlineData(Perm.ManageStickers, 16777216UL, "MANAGE_STICKERS")]
    public void every_bit_keeps_its_wire_value_and_its_wire_name(Perm bit, ulong value, string name)
    {
        Assert.Equal(value, (ulong)bit);
        Assert.Equal(name, PermNames.Name(bit));
        Assert.True(PermNames.TryParse(name, out var parsed));
        Assert.Equal(bit, parsed);
    }

    [Fact]
    public void the_scopes_partition_the_twenty_one_defined_bits()
    {
        var all = 0UL;
        foreach (var bit in AllBits)
        {
            all |= (ulong)bit;
        }

        var serverScoped = 0UL;
        foreach (var bit in ServerScopedBits)
        {
            serverScoped |= (ulong)bit;
        }

        // 1 | 4 | 8 | 32 | 64 | 128 | 1048576 | 4194304 | 16777216 = 22020333, which leaves
        // 11534098 channel-scoped.
        Assert.Equal(0x1FFFFFFUL, all);
        Assert.Equal(22020333UL, serverScoped);
        Assert.Equal(11534098UL, all & ~serverScoped);

        Assert.Equal(0x1FFFFFFUL, Perms.All);
        Assert.Equal(22020333UL, Perms.ServerScoped);
        Assert.Equal(11534098UL, Perms.ChannelScoped);
        Assert.Equal(0UL, Perms.ServerScoped & Perms.ChannelScoped);
        Assert.Equal(Perms.All, Perms.ServerScoped | Perms.ChannelScoped);
    }

    [Fact]
    public void everyone_default_is_view_send_attach_react_connect_speak_share_and_nickname()
    {
        var expected = (ulong)Perm.ViewChannel
            | (ulong)Perm.SendMessages
            | (ulong)Perm.AttachFiles
            | (ulong)Perm.AddReactions
            | (ulong)Perm.Connect
            | (ulong)Perm.Speak
            | (ulong)Perm.ShareScreen
            | (ulong)Perm.ChangeNickname
            | (ulong)Perm.Soundpad
            | (ulong)Perm.Video;

        Assert.Equal(11595520UL, expected);
        Assert.Equal(11595520UL, Perms.EveryoneDefault);
    }

    [Fact]
    public void has_asks_for_every_bit_of_its_argument_and_clean_drops_undefined_bits()
    {
        Assert.True(Perms.Has(View | Send, Perm.SendMessages));
        Assert.False(Perms.Has(View, Perm.SendMessages));
        Assert.True(Perms.Has(View | Send, (Perm)(View | Send)));
        Assert.False(Perms.Has(Send, (Perm)(View | Send)));

        Assert.Equal(Send, Perms.Clean(Send | (1UL << 25) | (1UL << 63)));
        Assert.Equal(0x1FFFFFFUL, Perms.Clean(ulong.MaxValue));
    }

    [Fact]
    public void name_has_nothing_to_say_about_a_mask_that_is_not_one_defined_bit()
    {
        Assert.Equal(string.Empty, PermNames.Name(Perm.None));
        Assert.Equal(string.Empty, PermNames.Name((Perm)(View | Send)));
        Assert.Equal(string.Empty, PermNames.Name((Perm)(1UL << 25)));
    }

    [Fact]
    public void try_parse_refuses_a_name_it_does_not_know()
    {
        Assert.False(PermNames.TryParse("send_messages", out var lower));
        Assert.Equal(Perm.None, lower);
        Assert.False(PermNames.TryParse("PERMISSION_SEND_MESSAGES", out var prefixed));
        Assert.Equal(Perm.None, prefixed);
        Assert.False(PermNames.TryParse(string.Empty, out var empty));
        Assert.Equal(Perm.None, empty);
    }

    [Fact]
    public void bits_yields_the_defined_bits_of_a_mask_in_ascending_order()
    {
        Assert.Equal(
            new[] { Perm.ManageServer, Perm.ViewChannel, Perm.ChangeNickname },
            PermNames.Bits(ManageServer | View | (ulong)Perm.ChangeNickname).ToArray());
        Assert.Equal(AllBits, PermNames.Bits(0x1FFFFFFUL).ToArray());
        Assert.Empty(PermNames.Bits(0));
        Assert.Equal(
            new[] { Perm.SendMessages },
            PermNames.Bits(Send | (1UL << 25) | (1UL << 40)).ToArray());
    }

    // ---- resolution ------------------------------------------------------------------------

    [Fact]
    public void the_owner_resolves_every_bit_however_the_overrides_deny()
    {
        var h = H(OwnerId, EveryoneRole(0), Role(LowRoleId, 1, 0));
        var owner = Member(OwnerId);
        var channel = Chan(
            ChannelId,
            false,
            RoleOverride(EveryoneId, 0, Perms.ChannelScoped),
            UserOverride(OwnerId, 0, Perms.ChannelScoped));

        var resolved = PermissionEngine.Resolve(h, owner, channel);

        Assert.Equal(0x1FFFFFFUL, resolved);
        Assert.Equal(0x1FFFFFFUL, PermissionEngine.Resolve(h, owner, null));
        foreach (var bit in AllBits)
        {
            Assert.True(Perms.Has(resolved, bit), PermNames.Name(bit));
        }
    }

    [Fact]
    public void everyone_is_the_base_of_a_member_that_holds_no_other_role()
    {
        var h = H(OwnerId, EveryoneRole(1109760));

        Assert.Equal(1109760UL, PermissionEngine.Resolve(h, Member(ActorId), null));
    }

    [Fact]
    public void the_base_is_everyone_unioned_with_every_role_the_member_holds()
    {
        // 256 | 512 | 8192 = 8960.
        var h = H(OwnerId, EveryoneRole(View), Role(LowRoleId, 1, Send), Role(HighRoleId, 2, Connect));

        Assert.Equal(8960UL, PermissionEngine.Resolve(h, Member(ActorId, LowRoleId, HighRoleId), null));
    }

    [Fact]
    public void a_role_id_that_names_no_role_is_ignored()
    {
        // 256 | 512 = 768; role 777 does not exist.
        var h = H(OwnerId, EveryoneRole(View), Role(LowRoleId, 1, Send));

        Assert.Equal(768UL, PermissionEngine.Resolve(h, Member(ActorId, LowRoleId, 777), null));
    }

    [Fact]
    public void a_server_level_resolve_never_looks_at_a_channel()
    {
        var h = H(OwnerId, EveryoneRole(View | Send));
        var member = Member(ActorId);
        var channel = Chan(ChannelId, false, RoleOverride(EveryoneId, 0, View | Send));

        Assert.Equal(768UL, PermissionEngine.Resolve(h, member, null));
        Assert.Equal(0UL, PermissionEngine.Resolve(h, member, channel));
    }

    // 25 bits x 3 layers x {allow, deny, inherit} x {base has the bit, base lacks it} = 450 cases.
    public static TheoryData<Perm, OverrideLayer, OverrideAction, bool, ulong> OverrideMatrix()
    {
        var data = new TheoryData<Perm, OverrideLayer, OverrideAction, bool, ulong>();
        foreach (var bit in AllBits)
        {
            var serverScoped = ServerScopedBits.Contains(bit);
            foreach (var layer in new[] { OverrideLayer.Everyone, OverrideLayer.Role, OverrideLayer.Member })
            {
                foreach (var action in new[] { OverrideAction.Allow, OverrideAction.Deny, OverrideAction.Inherit })
                {
                    foreach (var baseHasBit in new[] { true, false })
                    {
                        // A server-scoped bit cannot appear in an override, so only the base decides
                        // it; a channel-scoped one is whatever the single override says.
                        var present = serverScoped || action == OverrideAction.Inherit
                            ? baseHasBit
                            : action == OverrideAction.Allow;

                        // VIEW_CHANNEL is what keeps the channel-scoped half alive, and the base
                        // carries it except when it is itself the bit under test.
                        var survivingView = bit == Perm.ViewChannel ? 0UL : View;

                        data.Add(bit, layer, action, baseHasBit, (present ? (ulong)bit : 0UL) | survivingView);
                    }
                }
            }
        }

        return data;
    }

    [Theory]
    [MemberData(nameof(OverrideMatrix))]
    public void one_override_on_one_layer_resolves_exactly_as_the_protocol_says(
        Perm bit,
        OverrideLayer layer,
        OverrideAction action,
        bool baseHasBit,
        ulong expected)
    {
        var mask = (ulong)bit;
        var h = H(OwnerId, EveryoneRole(MatrixBase(bit, baseHasBit)), Role(LowRoleId, 1, 0));
        var member = Member(ActorId, LowRoleId);

        var overrides = action == OverrideAction.Inherit
            ? Array.Empty<OverrideDef>()
            : new[]
            {
                MatrixOverride(
                    layer,
                    action == OverrideAction.Allow ? mask : 0,
                    action == OverrideAction.Deny ? mask : 0),
            };

        Assert.Equal(expected, PermissionEngine.Resolve(h, member, Chan(ChannelId, false, overrides)));
    }

    [Fact]
    public void the_higher_role_wins_a_conflict_and_the_listed_order_does_not_matter()
    {
        var everyone = EveryoneRole(View | Send);
        var low = Role(LowRoleId, 1, 0);
        var high = Role(HighRoleId, 2, 0);
        var member = Member(ActorId, LowRoleId, HighRoleId);

        var lowAllows = RoleOverride(LowRoleId, Send, 0);
        var highDenies = RoleOverride(HighRoleId, 0, Send);

        // Position 2 is applied last, so SEND_MESSAGES ends up off and only VIEW_CHANNEL survives.
        Assert.Equal(
            256UL,
            PermissionEngine.Resolve(
                H(OwnerId, everyone, low, high),
                member,
                Chan(ChannelId, false, lowAllows, highDenies)));

        Assert.Equal(
            256UL,
            PermissionEngine.Resolve(
                H(OwnerId, high, everyone, low),
                member,
                Chan(ChannelId, false, highDenies, lowAllows)));

        // The reverse assignment flips the result: 256 | 512 = 768.
        Assert.Equal(
            768UL,
            PermissionEngine.Resolve(
                H(OwnerId, everyone, low, high),
                member,
                Chan(ChannelId, false, RoleOverride(LowRoleId, 0, Send), RoleOverride(HighRoleId, Send, 0))));
    }

    [Fact]
    public void two_roles_at_the_same_position_are_applied_by_ascending_id()
    {
        var everyone = EveryoneRole(View | Send);
        var h = H(OwnerId, everyone, Role(LowRoleId, 1, 0), Role(HighRoleId, 1, 0));
        var member = Member(ActorId, LowRoleId, HighRoleId);

        // Role 3 is applied after role 2, so role 3's deny is the one that stands.
        Assert.Equal(
            256UL,
            PermissionEngine.Resolve(
                h,
                member,
                Chan(ChannelId, false, RoleOverride(HighRoleId, 0, Send), RoleOverride(LowRoleId, Send, 0))));

        Assert.Equal(
            768UL,
            PermissionEngine.Resolve(
                h,
                member,
                Chan(ChannelId, false, RoleOverride(HighRoleId, Send, 0), RoleOverride(LowRoleId, 0, Send))));
    }

    [Fact]
    public void the_everyone_override_is_applied_before_the_role_overrides()
    {
        var member = Member(ActorId, LowRoleId);

        // @everyone denies first, the role allows after: 256 | 512 = 768.
        Assert.Equal(
            768UL,
            PermissionEngine.Resolve(
                H(OwnerId, EveryoneRole(View | Send), Role(LowRoleId, 1, 0)),
                member,
                Chan(ChannelId, false, RoleOverride(EveryoneId, 0, Send), RoleOverride(LowRoleId, Send, 0))));

        // The other way round the role's deny is the later one, so SEND_MESSAGES ends up off.
        Assert.Equal(
            256UL,
            PermissionEngine.Resolve(
                H(OwnerId, EveryoneRole(View), Role(LowRoleId, 1, 0)),
                member,
                Chan(ChannelId, false, RoleOverride(EveryoneId, Send, 0), RoleOverride(LowRoleId, 0, Send))));
    }

    [Fact]
    public void the_member_override_beats_a_role_deny_and_a_role_allow()
    {
        var h = H(OwnerId, EveryoneRole(View | Send), Role(LowRoleId, 1, 0));
        var member = Member(ActorId, LowRoleId);

        Assert.Equal(
            768UL,
            PermissionEngine.Resolve(
                h,
                member,
                Chan(ChannelId, false, RoleOverride(LowRoleId, 0, Send), UserOverride(ActorId, Send, 0))));

        var viewOnly = H(OwnerId, EveryoneRole(View), Role(LowRoleId, 1, 0));
        Assert.Equal(
            256UL,
            PermissionEngine.Resolve(
                viewOnly,
                member,
                Chan(ChannelId, false, RoleOverride(LowRoleId, Send, 0), UserOverride(ActorId, 0, Send))));
    }

    [Fact]
    public void without_view_channel_only_the_server_scoped_half_of_the_base_survives()
    {
        // base = 256 | 512 | 64 | 16 = 848; KICK_MEMBERS is the only server-scoped bit in it.
        var h = H(OwnerId, EveryoneRole(View | Send), Role(LowRoleId, 1, Kick | ManageMessages));
        var member = Member(ActorId, LowRoleId);

        Assert.Equal(
            64UL,
            PermissionEngine.Resolve(h, member, Chan(ChannelId, false, RoleOverride(EveryoneId, 0, View))));
    }

    [Fact]
    public void general_keeps_view_channel_even_when_everyone_is_denied_it()
    {
        var h = H(OwnerId, EveryoneRole(View | Send), Role(LowRoleId, 1, Kick | ManageMessages));
        var member = Member(ActorId, LowRoleId);

        // The deny is undone by the floor: the whole base, 848, comes back.
        Assert.Equal(
            848UL,
            PermissionEngine.Resolve(h, member, Chan(ChannelId, true, RoleOverride(EveryoneId, 0, View))));
    }

    [Fact]
    public void the_general_view_floor_applies_after_the_member_override()
    {
        var h = H(OwnerId, EveryoneRole(View | Send), Role(LowRoleId, 1, Kick | ManageMessages));
        var member = Member(ActorId, LowRoleId);

        // The member deny takes SEND_MESSAGES away for good but not VIEW_CHANNEL:
        // 16 | 256 | 64 = 336.
        Assert.Equal(
            336UL,
            PermissionEngine.Resolve(h, member, Chan(ChannelId, true, UserOverride(ActorId, 0, View | Send))));
    }

    [Fact]
    public void a_malformed_override_is_masked_rather_than_thrown_on()
    {
        var h = H(OwnerId, EveryoneRole(View), Role(LowRoleId, 1, 0));
        var member = Member(ActorId, LowRoleId);

        // Server-scoped bits in an override are masked away, and an allow that overlaps its own deny
        // still resolves: 256 | 512 = 768.
        var malformed = new OverrideDef(EveryoneId, ActorId, Send | Kick, Send | Ban);

        Assert.Equal(768UL, PermissionEngine.Resolve(h, member, Chan(ChannelId, false, malformed)));
        Assert.False(PermissionEngine.IsValidOverride(malformed));
    }

    // ---- hierarchy -------------------------------------------------------------------------

    [Fact]
    public void highest_position_is_zero_without_roles_and_the_greatest_position_held_otherwise()
    {
        var h = H(OwnerId, EveryoneRole(0), Role(2, 1, 0), Role(3, 5, 0), Role(4, 2, 0));

        Assert.Equal(0, PermissionEngine.HighestPosition(h, Member(ActorId)));
        Assert.Equal(1, PermissionEngine.HighestPosition(h, Member(ActorId, 2)));
        Assert.Equal(5, PermissionEngine.HighestPosition(h, Member(ActorId, 2, 3, 4)));
        Assert.Equal(2, PermissionEngine.HighestPosition(h, Member(ActorId, 4)));
        Assert.Equal(0, PermissionEngine.HighestPosition(h, Member(ActorId, 777)));

        // A member holding @everyone only is 0 whatever position that row claims to be at.
        var misplaced = H(OwnerId, new RoleDef(EveryoneId, 9, 0, true));
        Assert.Equal(0, PermissionEngine.HighestPosition(misplaced, Member(ActorId)));
        Assert.Equal(0, PermissionEngine.HighestPosition(misplaced, Member(ActorId, EveryoneId)));
    }

    [Fact]
    public void is_owner_compares_the_user_id_against_the_server_owner()
    {
        var h = H(OwnerId, EveryoneRole(0));

        Assert.True(PermissionEngine.IsOwner(h, Member(OwnerId)));
        Assert.False(PermissionEngine.IsOwner(h, Member(ActorId)));
    }

    [Fact]
    public void the_owner_may_manage_a_role_above_every_role_it_holds()
    {
        var target = Role(LowRoleId, 5, 0);
        var h = H(OwnerId, EveryoneRole(0), target);

        Assert.True(PermissionEngine.CanManageRole(h, Member(OwnerId), target));
    }

    [Fact]
    public void a_manage_roles_holder_may_manage_only_a_role_strictly_below_it()
    {
        var actorRole = Role(5, 5, ManageRoles);
        var below = Role(4, 4, 0);
        var equal = Role(6, 5, 0);
        var above = Role(7, 6, 0);
        var h = H(OwnerId, EveryoneRole(0), actorRole, below, equal, above);
        var actor = Member(ActorId, 5);

        Assert.True(PermissionEngine.CanManageRole(h, actor, below));
        Assert.False(PermissionEngine.CanManageRole(h, actor, equal));
        Assert.False(PermissionEngine.CanManageRole(h, actor, above));
        Assert.False(PermissionEngine.CanManageRole(h, actor, actorRole));
    }

    [Fact]
    public void everyone_is_manageable_by_any_manage_roles_holder_however_low_it_sits()
    {
        var everyone = EveryoneRole(ManageRoles);
        var other = Role(LowRoleId, 1, 0);
        var h = H(OwnerId, everyone, other);
        var actor = Member(ActorId);

        Assert.Equal(0, PermissionEngine.HighestPosition(h, actor));
        Assert.True(PermissionEngine.CanManageRole(h, actor, everyone));
        Assert.False(PermissionEngine.CanManageRole(h, actor, other));
    }

    [Fact]
    public void a_member_without_manage_roles_may_manage_nothing()
    {
        var everyone = EveryoneRole(1109760);
        var h = H(OwnerId, everyone, Role(LowRoleId, 1, 0));
        var actor = Member(ActorId, LowRoleId);

        Assert.False(PermissionEngine.CanManageRole(h, actor, everyone));
        Assert.False(PermissionEngine.CanManageRole(h, actor, Role(99, 0, 0)));
    }

    [Fact]
    public void a_caller_may_grant_only_bits_it_holds_at_server_level()
    {
        var h = H(OwnerId, EveryoneRole(1109760));
        var actor = Member(ActorId);

        Assert.True(PermissionEngine.CanGrant(h, actor, View | Send));
        Assert.True(PermissionEngine.CanGrant(h, actor, 1109760));
        Assert.True(PermissionEngine.CanGrant(h, actor, 0));
        Assert.Null(PermissionEngine.MissingGrant(h, actor, 1109760));

        Assert.False(PermissionEngine.CanGrant(h, actor, Send | ManageServer));
        Assert.False(PermissionEngine.CanGrant(h, actor, ManageRoles | Ban));
    }

    [Fact]
    public void missing_grant_names_the_lowest_bit_the_caller_lacks()
    {
        var h = H(OwnerId, EveryoneRole(1109760));
        var actor = Member(ActorId);

        var twoMissing = PermissionEngine.MissingGrant(h, actor, ManageRoles | Ban);
        Assert.NotNull(twoMissing);
        Assert.Equal(Perm.ManageRoles, twoMissing.Value);
        Assert.Equal("MANAGE_ROLES", PermNames.Name(twoMissing.Value));

        var oneMissing = PermissionEngine.MissingGrant(h, actor, Send | ManageServer);
        Assert.NotNull(oneMissing);
        Assert.Equal(Perm.ManageServer, oneMissing.Value);
    }

    [Fact]
    public void the_owner_may_grant_every_bit()
    {
        var h = H(OwnerId, EveryoneRole(0));
        var owner = Member(OwnerId);

        Assert.True(PermissionEngine.CanGrant(h, owner, 0x1FFFFFF));
        Assert.Null(PermissionEngine.MissingGrant(h, owner, 0x1FFFFFF));
    }

    [Fact]
    public void an_undefined_bit_is_ignored_by_both_can_grant_and_missing_grant()
    {
        var h = H(OwnerId, EveryoneRole(1109760));
        var actor = Member(ActorId);

        // Cleaned away rather than refused, so the two answers can never disagree.
        Assert.True(PermissionEngine.CanGrant(h, actor, 1UL << 25));
        Assert.Null(PermissionEngine.MissingGrant(h, actor, 1UL << 25));

        foreach (var bits in new[] { 0UL, View, ManageServer, 1109760UL, 0x1FFFFFFUL, ulong.MaxValue })
        {
            Assert.Equal(
                PermissionEngine.CanGrant(h, actor, bits),
                PermissionEngine.MissingGrant(h, actor, bits) is null);
        }
    }

    [Fact]
    public void the_owner_is_never_a_target_and_neither_is_yourself()
    {
        var h = H(OwnerId, EveryoneRole(0), Role(LowRoleId, 1, 0), Role(HighRoleId, 2, 0));
        var actor = Member(ActorId, HighRoleId);

        Assert.Equal(TargetVerdict.IsOwner, PermissionEngine.CanTarget(h, actor, Member(OwnerId)));
        Assert.Equal(TargetVerdict.IsSelf, PermissionEngine.CanTarget(h, actor, Member(ActorId, HighRoleId)));
        Assert.Equal(TargetVerdict.IsOwner, PermissionEngine.CanTarget(h, Member(OwnerId), Member(OwnerId)));
    }

    [Fact]
    public void a_target_must_sit_strictly_below_the_caller()
    {
        var h = H(OwnerId, EveryoneRole(0), Role(LowRoleId, 1, 0), Role(HighRoleId, 2, 0));

        Assert.Equal(
            TargetVerdict.Allowed,
            PermissionEngine.CanTarget(h, Member(ActorId, HighRoleId), Member(OtherId, LowRoleId)));
        Assert.Equal(
            TargetVerdict.Outranked,
            PermissionEngine.CanTarget(h, Member(ActorId, LowRoleId), Member(OtherId, LowRoleId)));
        Assert.Equal(
            TargetVerdict.Outranked,
            PermissionEngine.CanTarget(h, Member(ActorId), Member(OtherId)));
        Assert.Equal(
            TargetVerdict.Outranked,
            PermissionEngine.CanTarget(h, Member(ActorId, LowRoleId), Member(OtherId, HighRoleId)));
        Assert.Equal(
            TargetVerdict.Allowed,
            PermissionEngine.CanTarget(h, Member(OwnerId), Member(OtherId, HighRoleId)));
    }

    [Fact]
    public void an_override_targets_exactly_one_role_or_one_member()
    {
        Assert.True(PermissionEngine.IsValidOverride(RoleOverride(EveryoneId, Send, View)));
        Assert.True(PermissionEngine.IsValidOverride(UserOverride(ActorId, Send, View)));
        Assert.False(PermissionEngine.IsValidOverride(new OverrideDef(EveryoneId, ActorId, Send, 0)));
        Assert.False(PermissionEngine.IsValidOverride(new OverrideDef(null, null, Send, 0)));
        Assert.False(PermissionEngine.IsValidOverride(RoleOverride(EveryoneId, Send, Send)));

        // Both sides are masked first, so an overlap that is only server-scoped is not an overlap.
        Assert.True(PermissionEngine.IsValidOverride(RoleOverride(EveryoneId, Kick, Kick)));
    }

    [Fact]
    public void only_everyone_losing_view_on_general_is_refused_at_write_time()
    {
        var general = Chan(ChannelId, true);
        var plain = Chan(ChannelId, false);

        Assert.True(PermissionEngine.DeniesGeneralView(general, RoleOverride(EveryoneId, 0, View), EveryoneId));
        Assert.True(PermissionEngine.DeniesGeneralView(general, RoleOverride(EveryoneId, 0, View | Send), EveryoneId));
        Assert.False(PermissionEngine.DeniesGeneralView(plain, RoleOverride(EveryoneId, 0, View), EveryoneId));
        Assert.False(PermissionEngine.DeniesGeneralView(general, RoleOverride(LowRoleId, 0, View), EveryoneId));
        Assert.False(PermissionEngine.DeniesGeneralView(general, RoleOverride(EveryoneId, 0, Send), EveryoneId));
        Assert.False(PermissionEngine.DeniesGeneralView(general, UserOverride(ActorId, 0, View), EveryoneId));
    }

    [Fact]
    public void a_new_role_goes_in_at_the_creators_own_position_or_just_above_everyone()
    {
        var h = H(OwnerId, EveryoneRole(0), Role(LowRoleId, 1, 0), Role(HighRoleId, 2, 0));

        Assert.Equal(2, PermissionEngine.InsertPosition(h, Member(ActorId, HighRoleId)));
        Assert.Equal(1, PermissionEngine.InsertPosition(h, Member(ActorId, LowRoleId)));
        Assert.Equal(1, PermissionEngine.InsertPosition(h, Member(ActorId)));
        Assert.Equal(1, PermissionEngine.InsertPosition(h, Member(OwnerId)));
    }

    // ---- helpers ---------------------------------------------------------------------------

    private static ulong MatrixBase(Perm bit, bool baseHasBit)
        => (baseHasBit ? (ulong)bit : 0UL) | (bit == Perm.ViewChannel ? 0UL : View);

    private static OverrideDef MatrixOverride(OverrideLayer layer, ulong allow, ulong deny) => layer switch
    {
        OverrideLayer.Everyone => RoleOverride(EveryoneId, allow, deny),
        OverrideLayer.Role => RoleOverride(LowRoleId, allow, deny),
        _ => UserOverride(ActorId, allow, deny),
    };

    private static RoleDef EveryoneRole(ulong permissions) => new(EveryoneId, 0, permissions, true);

    private static RoleDef Role(long id, int position, ulong permissions) => new(id, position, permissions, false);

    private static MemberDef Member(long userId, params long[] roleIds) => new(userId, roleIds);

    private static ChannelDef Chan(long id, bool isGeneral, params OverrideDef[] overrides)
        => new(id, isGeneral, overrides);

    private static Hierarchy H(long ownerId, params RoleDef[] roles) => new(ownerId, roles);

    private static OverrideDef RoleOverride(long roleId, ulong allow, ulong deny) => new(roleId, null, allow, deny);

    private static OverrideDef UserOverride(long userId, ulong allow, ulong deny) => new(null, userId, allow, deny);
}

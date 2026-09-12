using System.Numerics;

namespace Vorcall.Server.Permissions;

// Why a member may not act on another: the verdicts of PROTOCOL.md § Hierarchy, which the caller
// turns into FORBIDDEN (owner, self) or HIERARCHY (outranked).
public enum TargetVerdict
{
    Allowed = 0,
    IsOwner = 1,
    IsSelf = 2,
    Outranked = 3,
}

// The ten resolution steps and the hierarchy rules of PROTOCOL.md § Roles and permissions, which
// vorcall-core/src/permissions.rs mirrors and the same matrix tests on both sides. Pure: no clock,
// no I/O, and nothing here throws on malformed input — an override naming two targets or overlapping
// allow with deny is refused at write time by IsValidOverride and merely masked when resolving.
public static class PermissionEngine
{
    public static ulong Resolve(Hierarchy h, MemberDef member, ChannelDef? channel)
    {
        // 1. The owner bypasses every check, whatever the overrides say.
        if (IsOwner(h, member))
        {
            return Perms.All;
        }

        var everyone = EveryoneRole(h);
        var held = HeldRoles(h, member);

        // 2. @everyone plus every role the member holds; an id that names no role is ignored.
        var baseBits = everyone?.Permissions ?? 0UL;
        for (var i = 0; i < held.Count; i++)
        {
            baseBits |= held[i].Permissions;
        }

        baseBits = Perms.Clean(baseBits);

        // 3. A server-level question never looks at a channel.
        if (channel is null)
        {
            return baseBits;
        }

        // 4. The server-scoped half is kept aside until step 10.
        var serverHalf = baseBits & Perms.ServerScoped;
        var p = baseBits & Perms.ChannelScoped;

        // 5. The @everyone override of this channel.
        if (everyone is not null)
        {
            p = Apply(p, FindRoleOverride(channel, everyone.Id));
        }

        // 6. The member's own roles, lowest position first, each applied to the result of the last:
        // that is what lets a higher role win a conflict with a lower one.
        for (var i = 0; i < held.Count; i++)
        {
            p = Apply(p, FindRoleOverride(channel, held[i].Id));
        }

        // 7. The member override wins over every role.
        p = Apply(p, FindUserOverride(channel, member.UserId));

        // 8. general can never be hidden.
        if (channel.IsGeneral)
        {
            p |= (ulong)Perm.ViewChannel;
        }

        // 9. Without VIEW_CHANNEL the channel grants nothing at all.
        if ((p & (ulong)Perm.ViewChannel) == 0)
        {
            return serverHalf;
        }

        // 10. A channel never removes a server-scoped bit.
        return p | serverHalf;
    }

    // The greatest position among the member's roles, and 0 when it holds @everyone only — that role
    // is pinned at position 0.
    public static int HighestPosition(Hierarchy h, MemberDef member)
    {
        var highest = 0;
        for (var i = 0; i < h.Roles.Count; i++)
        {
            var role = h.Roles[i];
            if (!role.Everyone && role.Position > highest && member.RoleIds.Contains(role.Id))
            {
                highest = role.Position;
            }
        }

        return highest;
    }

    public static bool IsOwner(Hierarchy h, MemberDef member) => member.UserId == h.OwnerId;

    // Create, edit, delete, assign or reorder a role. @everyone is the exception: its permissions are
    // editable by any MANAGE_ROLES holder, however low that holder sits.
    public static bool CanManageRole(Hierarchy h, MemberDef actor, RoleDef target)
    {
        if (IsOwner(h, actor))
        {
            return true;
        }

        if (!Perms.Has(Resolve(h, actor, null), Perm.ManageRoles))
        {
            return false;
        }

        return target.Everyone || target.Position < HighestPosition(h, actor);
    }

    // A caller may put into a role or an override only bits it holds itself at server level.
    public static bool CanGrant(Hierarchy h, MemberDef actor, ulong bits)
    {
        if (IsOwner(h, actor))
        {
            return true;
        }

        return (Perms.Clean(bits) & ~Resolve(h, actor, null)) == 0;
    }

    // The bit to name in PERMISSION_DENIED.detail, or null when the grant is allowed. Cleaned like
    // CanGrant, so "cannot grant" and "has a missing bit to name" are always the same answer.
    public static Perm? MissingGrant(Hierarchy h, MemberDef actor, ulong bits)
    {
        if (IsOwner(h, actor))
        {
            return null;
        }

        var missing = Perms.Clean(bits) & ~Resolve(h, actor, null);
        if (missing == 0)
        {
            return null;
        }

        return (Perm)(1UL << BitOperations.TrailingZeroCount(missing));
    }

    // Kick, ban, nickname, server mute or deafen, move, role change. The owner check comes first, so
    // the owner acting on itself is IsOwner rather than IsSelf; either way it is refused.
    public static TargetVerdict CanTarget(Hierarchy h, MemberDef actor, MemberDef target)
    {
        if (IsOwner(h, target))
        {
            return TargetVerdict.IsOwner;
        }

        if (target.UserId == actor.UserId)
        {
            return TargetVerdict.IsSelf;
        }

        if (IsOwner(h, actor))
        {
            return TargetVerdict.Allowed;
        }

        return HighestPosition(h, actor) > HighestPosition(h, target)
            ? TargetVerdict.Allowed
            : TargetVerdict.Outranked;
    }

    // An override targets exactly one role or one member and may not allow and deny the same bit.
    // Server-scoped bits are masked away first, so a pair that only overlaps there is still valid.
    public static bool IsValidOverride(OverrideDef o)
    {
        if ((o.RoleId is not null) == (o.UserId is not null))
        {
            return false;
        }

        return ((o.Allow & Perms.ChannelScoped) & (o.Deny & Perms.ChannelScoped)) == 0;
    }

    // The one override the write path refuses outright: @everyone losing VIEW_CHANNEL on general.
    public static bool DeniesGeneralView(ChannelDef channel, OverrideDef candidate, long everyoneRoleId)
        => channel.IsGeneral
            && candidate.RoleId == everyoneRoleId
            && (candidate.Deny & (ulong)Perm.ViewChannel) != 0;

    // A new role goes in at the creator's own highest position; the owner, who may hold no role at
    // all, creates it just above @everyone. Callers renumber whatever sat there.
    public static int InsertPosition(Hierarchy h, MemberDef actor)
    {
        var highest = HighestPosition(h, actor);
        return highest > 0 ? highest : 1;
    }

    private static ulong Apply(ulong p, OverrideDef? o)
    {
        if (o is null)
        {
            return p;
        }

        var deny = o.Deny & Perms.ChannelScoped;
        var allow = o.Allow & Perms.ChannelScoped;
        return (p & ~deny) | allow;
    }

    private static RoleDef? EveryoneRole(Hierarchy h)
    {
        for (var i = 0; i < h.Roles.Count; i++)
        {
            if (h.Roles[i].Everyone)
            {
                return h.Roles[i];
            }
        }

        return null;
    }

    // The member's roles, @everyone excluded, in the order step 6 applies them.
    private static List<RoleDef> HeldRoles(Hierarchy h, MemberDef member)
    {
        var held = new List<RoleDef>();
        for (var i = 0; i < h.Roles.Count; i++)
        {
            var role = h.Roles[i];
            if (!role.Everyone && member.RoleIds.Contains(role.Id))
            {
                held.Add(role);
            }
        }

        // Ids break a tie so the result never depends on the order the caller listed the roles in.
        held.Sort(static (a, b) => a.Position == b.Position
            ? a.Id.CompareTo(b.Id)
            : a.Position.CompareTo(b.Position));

        return held;
    }

    private static OverrideDef? FindRoleOverride(ChannelDef channel, long roleId)
    {
        for (var i = 0; i < channel.Overrides.Count; i++)
        {
            if (channel.Overrides[i].RoleId == roleId)
            {
                return channel.Overrides[i];
            }
        }

        return null;
    }

    private static OverrideDef? FindUserOverride(ChannelDef channel, long userId)
    {
        for (var i = 0; i < channel.Overrides.Count; i++)
        {
            if (channel.Overrides[i].UserId == userId)
            {
                return channel.Overrides[i];
            }
        }

        return null;
    }
}

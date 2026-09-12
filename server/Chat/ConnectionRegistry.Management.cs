using System.Net.WebSockets;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

// Why one management or moderation operation was refused. The socket handler turns each of these
// into the ErrorCode PROTOCOL.md names for it; NotLive is the one verdict that answers nothing at
// all, because the connection it concerns has already been replaced.
public enum OpStatus
{
    Ok,
    PermissionDenied,
    Hierarchy,
    Forbidden,
    InvalidArgument,
    UnknownChannel,
    UnknownCategory,
    UnknownRole,
    UnknownUser,
    UnknownImage,
    NotInVoice,
    NotLive,
}

// Detail is the wire's Error.detail: the missing permission's name for PermissionDenied, the
// offending field's name for InvalidArgument, and empty for every other verdict.
public readonly record struct OpResult(OpStatus Status, string Detail = "")
{
    public static OpResult Ok { get; } = new(OpStatus.Ok);

    public bool IsOk => Status == OpStatus.Ok;

    public static OpResult Denied(Perm bit) => new(OpStatus.PermissionDenied, PermNames.Name(bit));

    public static OpResult Invalid(string field) => new(OpStatus.InvalidArgument, field);

    public static OpResult Of(OpStatus status) => new(status);
}

// The management and moderation half of the registry. Every operation here is the same five steps:
// check against the mirror under the lock, write the row through a directory, mirror the result
// under the lock again, queue the frames PROTOCOL.md owes, and re-resolve whatever the change can
// have moved. The lock is never held across an await, so everything is looked up again after the
// write rather than assumed; the persisted row is the truth either way.
//
// Nothing here parses a frame or validates a grammar: names, topics, descriptions, reasons and
// emoji arrive normalised, and a user_id of 0 ("self") is resolved before it gets here.
public sealed partial class ConnectionRegistry
{
    // The caps of PROTOCOL.md § Server, channels and categories and § Roles and permissions,
    // counted against the mirror rather than the database: the mirror holds the same rows, and the
    // count has to sit in the same critical section as the check that follows it.
    private const int MaxChannels = 200;
    private const int MaxCategories = 50;
    private const int MaxOverridesPerChannel = 100;
    private const int MaxRoles = 100;

    // Every colour on the wire is a uint32 holding 0xRRGGBB, 0 meaning none: PROTOCOL.md § Roles
    // and permissions.
    private const uint MaxColor = 0xFFFFFF;

    public async Task<OpResult> CreateChannelAsync(
        long actorId,
        Data.ChannelKind kind,
        string name,
        string topic,
        long categoryId,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            // A DM is opened with OpenDm and never created here.
            if (kind is not (Data.ChannelKind.Text or Data.ChannelKind.Voice))
            {
                return OpResult.Invalid("kind");
            }

            if (categoryId != 0 && !_categories.ContainsKey(categoryId))
            {
                return OpResult.Of(OpStatus.UnknownCategory);
            }

            if (NonDmChannelCountLocked() >= MaxChannels)
            {
                return OpResult.Invalid("channels");
            }
        }

        var record = await _channels.CreateChannelAsync(kind, name, topic, categoryId == 0 ? null : categoryId, ct);

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            var state = new ChannelState(record);
            _channelById[record.Id] = state;
            RebuildDefLocked(state);

            // The sweep is what hands the channel to each online member who may view it, Visible
            // and ChannelUpserted together.
            ReevaluateLocked([record.Id], null, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> UpdateChannelAsync(
        long actorId,
        long channelId,
        string name,
        string topic,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!VisibleLocked(actor, channelId, out var channel))
            {
                return OpResult.Of(OpStatus.UnknownChannel);
            }

            if (channel.IsDm)
            {
                return OpResult.Invalid("channel");
            }

            var allowed = CheckLocked(actor, channel, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        var record = await _channels.UpdateChannelAsync(channelId, name, topic, ct);
        if (record is null)
        {
            return OpResult.Of(OpStatus.UnknownChannel);
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                channel.Record = record;
                BroadcastToChannelLocked(channel, ChannelUpsertedOf(channel), except: null, ref slow);
            }
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> DeleteChannelAsync(
        long actorId,
        long channelId,
        AttachmentStore attachments,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!VisibleLocked(actor, channelId, out var channel))
            {
                return OpResult.Of(OpStatus.UnknownChannel);
            }

            if (channel.IsGeneral)
            {
                return OpResult.Of(OpStatus.Forbidden);
            }

            if (channel.IsDm)
            {
                return OpResult.Invalid("channel");
            }

            var allowed = CheckLocked(actor, channel, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        var outcome = await _channels.DeleteChannelAsync(channelId, ct);
        if (!outcome.Found)
        {
            return OpResult.Of(OpStatus.UnknownChannel);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        lock (_gate)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                foreach (var userId in channel.Voice.Keys.ToList())
                {
                    RemoveVoiceLocked(channel, userId, ref slow, removed);
                    SendToLocked(userId, VoiceMovedOf(0), ref slow);
                }

                // Before Visible loses the channel: that set is the audience of the frame.
                BroadcastToChannelLocked(channel, ChannelDeletedOf(channelId), except: null, ref slow);
                _channelById.Remove(channelId);
                foreach (var member in _memberById.Values)
                {
                    member.Visible.Remove(channelId);
                }
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        attachments.DeleteFiles(outcome.AttachmentFiles);
        return OpResult.Ok;
    }

    public async Task<OpResult> CreateCategoryAsync(long actorId, string name, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            if (_categories.Count >= MaxCategories)
            {
                return OpResult.Invalid("categories");
            }
        }

        var record = await _channels.CreateCategoryAsync(name, ct);

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            _categories[record.Id] = record;
            BroadcastToAllLocked(CategoryUpsertedOf(record), except: null, ref slow);
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> UpdateCategoryAsync(long actorId, long categoryId, string name, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_categories.ContainsKey(categoryId))
            {
                return OpResult.Of(OpStatus.UnknownCategory);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        var record = await _channels.UpdateCategoryAsync(categoryId, name, ct);
        if (record is null)
        {
            return OpResult.Of(OpStatus.UnknownCategory);
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            _categories[record.Id] = record;
            BroadcastToAllLocked(CategoryUpsertedOf(record), except: null, ref slow);
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> DeleteCategoryAsync(long actorId, long categoryId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_categories.ContainsKey(categoryId))
            {
                return OpResult.Of(OpStatus.UnknownCategory);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        var orphans = await _channels.DeleteCategoryAsync(categoryId, ct);

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            _categories.Remove(categoryId);
            foreach (var orphan in orphans)
            {
                if (_channelById.TryGetValue(orphan.Id, out var channel))
                {
                    channel.Record = orphan;
                }
            }

            BroadcastToAllLocked(CategoryDeletedOf(categoryId), except: null, ref slow);

            // The orphans moved to "no category", so the whole sidebar order is re-sent rather
            // than one channel at a time: PROTOCOL.md § Server, channels and categories.
            BroadcastToAllLocked(ChannelOrderOf(), except: null, ref slow);
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> ReorderChannelsAsync(
        long actorId,
        IReadOnlyList<(long Id, long CategoryId, int Position)> positions,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            if (positions.Count == 0)
            {
                return OpResult.Invalid("positions");
            }

            var seen = new HashSet<long>();
            foreach (var (id, categoryId, _) in positions)
            {
                if (!seen.Add(id))
                {
                    return OpResult.Invalid("positions");
                }

                if (!_channelById.TryGetValue(id, out var channel) || channel.IsDm)
                {
                    return OpResult.Of(OpStatus.UnknownChannel);
                }

                if (categoryId != 0 && !_categories.ContainsKey(categoryId))
                {
                    return OpResult.Of(OpStatus.UnknownCategory);
                }
            }

            // The frame carries every channel, so a short one would leave the rest at positions
            // that no longer mean anything. DMs have no position and are not in it.
            if (positions.Count != NonDmChannelCountLocked())
            {
                return OpResult.Invalid("positions");
            }
        }

        var rows = new List<(long Id, long? CategoryId, int Position)>(positions.Count);
        foreach (var (id, categoryId, position) in positions)
        {
            rows.Add((id, categoryId == 0 ? null : categoryId, position));
        }

        if (!await _channels.ReorderChannelsAsync(rows, ct))
        {
            return OpResult.Invalid("positions");
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            foreach (var (id, categoryId, position) in rows)
            {
                if (_channelById.TryGetValue(id, out var channel))
                {
                    channel.Record = channel.Record with { CategoryId = categoryId, Position = position };
                }
            }

            BroadcastToAllLocked(ChannelOrderOf(), except: null, ref slow);
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> ReorderCategoriesAsync(long actorId, IReadOnlyList<long> ids, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageChannels);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            if (ids.Count == 0)
            {
                return OpResult.Invalid("ids");
            }

            var seen = new HashSet<long>();
            foreach (var id in ids)
            {
                if (!seen.Add(id))
                {
                    return OpResult.Invalid("ids");
                }

                if (!_categories.ContainsKey(id))
                {
                    return OpResult.Of(OpStatus.UnknownCategory);
                }
            }

            // The frame carries the full list, so a short one would leave the rest at positions
            // that no longer mean anything.
            if (ids.Count != _categories.Count)
            {
                return OpResult.Invalid("ids");
            }
        }

        if (!await _channels.ReorderCategoriesAsync(ids, ct))
        {
            return OpResult.Invalid("ids");
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            // There is no category-order frame: each category whose position moved is upserted.
            for (var index = 0; index < ids.Count; index++)
            {
                if (!_categories.TryGetValue(ids[index], out var category) || category.Position == index)
                {
                    continue;
                }

                var moved = category with { Position = index };
                _categories[moved.Id] = moved;
                BroadcastToAllLocked(CategoryUpsertedOf(moved), except: null, ref slow);
            }
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> SetOverrideAsync(
        long actorId,
        long channelId,
        long roleId,
        long userId,
        ulong allow,
        ulong deny,
        CancellationToken ct)
    {
        var kind = roleId != 0 ? Data.OverrideTarget.Role : Data.OverrideTarget.User;
        var targetId = roleId != 0 ? roleId : userId;
        var allowed = Perms.Clean(allow) & Perms.ChannelScoped;
        var denied = Perms.Clean(deny) & Perms.ChannelScoped;

        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!VisibleLocked(actor, channelId, out var channel))
            {
                return OpResult.Of(OpStatus.UnknownChannel);
            }

            // A DM's two members see it by membership, so there is nothing an override could
            // decide there.
            if (channel.IsDm)
            {
                return OpResult.Invalid("channel");
            }

            var manage = CheckLocked(actor, channel, Perm.ManageChannels);
            if (!manage.IsOk)
            {
                return manage;
            }

            if ((roleId != 0) == (userId != 0))
            {
                return OpResult.Invalid("override");
            }

            if (roleId != 0 && !_roleById.ContainsKey(roleId))
            {
                return OpResult.Of(OpStatus.UnknownRole);
            }

            if (userId != 0 && !_memberById.ContainsKey(userId))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            var candidate = new OverrideDef(
                roleId != 0 ? roleId : null,
                userId != 0 ? userId : null,
                allowed,
                denied);
            if (!PermissionEngine.IsValidOverride(candidate))
            {
                return OpResult.Invalid("override");
            }

            if (PermissionEngine.DeniesGeneralView(channel.Def, candidate, _everyoneRoleId))
            {
                return OpResult.Invalid("deny");
            }

            if (PermissionEngine.MissingGrant(_hierarchy, DefOf(actor), allowed | denied) is { } missing)
            {
                return OpResult.Denied(missing);
            }

            // A pair that masks away to nothing deletes the row instead of writing one, so it is
            // not a create and the cap does not apply to it.
            if ((allowed | denied) != 0
                && !channel.Overrides.ContainsKey((kind, targetId))
                && channel.Overrides.Count >= MaxOverridesPerChannel)
            {
                return OpResult.Invalid("overrides");
            }
        }

        if (!await _channels.SetOverrideAsync(channelId, kind, targetId, allowed, denied, ct))
        {
            return OpResult.Of(OpStatus.UnknownChannel);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            if (_channelById.TryGetValue(channelId, out var channel))
            {
                if ((allowed | denied) == 0)
                {
                    channel.Overrides.Remove((kind, targetId));
                }
                else
                {
                    channel.Overrides[(kind, targetId)] = new OverrideRecord(channelId, kind, targetId, allowed, denied);
                }

                RebuildDefLocked(channel);

                // The sweep first, so the members the override just gained or lost sight of are
                // the audience the upsert is measured against.
                ReevaluateLocked([channelId], null, ref slow, removed, silenced);
                BroadcastToChannelLocked(channel, ChannelUpsertedOf(channel), except: null, ref slow);
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> CreateRoleAsync(
        long actorId,
        string name,
        uint color,
        string iconEmoji,
        long iconImageId,
        ulong permissions,
        bool hoist,
        ImageStore images,
        CancellationToken ct)
    {
        if (color > MaxColor)
        {
            return OpResult.Invalid("color");
        }

        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageRoles);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            if (_roleById.Count >= MaxRoles)
            {
                return OpResult.Invalid("roles");
            }

            if (PermissionEngine.MissingGrant(_hierarchy, DefOf(actor), permissions) is { } missing)
            {
                return OpResult.Denied(missing);
            }

            // A role shows at most one icon.
            if (iconEmoji.Length > 0 && iconImageId != 0)
            {
                return OpResult.Invalid("icon");
            }
        }

        if (iconImageId != 0 && !await images.ResolveForAsync(iconImageId, Data.ImagePurpose.RoleIcon, null, ct))
        {
            return OpResult.Of(OpStatus.UnknownImage);
        }

        int position;
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            position = PermissionEngine.InsertPosition(_hierarchy, DefOf(actor));
        }

        var record = await _roles.CreateAsync(
            name,
            color == 0 ? null : (int)color,
            iconEmoji,
            iconImageId == 0 ? null : iconImageId,
            Perms.Clean(permissions),
            hoist,
            position,
            ct);

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            // The directory shifted the rows at or above the insertion point; the mirror follows.
            var shifted = false;
            foreach (var existing in _roleById.Values.ToList())
            {
                if (!existing.IsEveryone && existing.Position >= record.Position)
                {
                    _roleById[existing.Id] = existing with { Position = existing.Position + 1 };
                    shifted = true;
                }
            }

            _roleById[record.Id] = record;
            RebuildHierarchyLocked();

            BroadcastToAllLocked(RoleUpsertedOf(record), except: null, ref slow);
            if (shifted)
            {
                // A create that renumbered the rest is a reorder as far as a client is concerned.
                BroadcastToAllLocked(RoleOrderOf(), except: null, ref slow);
            }
        }

        CloseSlow(slow);

        // Nobody holds the new role yet, so nothing anybody may see or do has changed.
        return OpResult.Ok;
    }

    public async Task<OpResult> UpdateRoleAsync(long actorId, Protocol.Role role, ImageStore images, CancellationToken ct)
    {
        if (role.Color > MaxColor)
        {
            return OpResult.Invalid("color");
        }

        var permissions = Perms.Clean(role.Permissions);
        var color = role.Color == 0 ? (int?)null : (int)role.Color;
        var iconImageId = role.IconImageId == 0 ? (long?)null : role.IconImageId;

        RoleRecord wanted;
        long? previousIcon;
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_roleById.TryGetValue(role.Id, out var current))
            {
                return OpResult.Of(OpStatus.UnknownRole);
            }

            var manage = ManageRoleLocked(actor, current);
            if (!manage.IsOk)
            {
                return manage;
            }

            // On @everyone only the permissions are editable, so anything else the frame carries
            // has to match what is stored.
            if (current.IsEveryone
                && (role.Name != current.Name
                    || color != current.Color
                    || role.IconEmoji != current.IconEmoji
                    || iconImageId != current.IconImageId
                    || role.Hoist != current.Hoist))
            {
                return OpResult.Invalid("role");
            }

            // Only the bits being added have to be held: taking one away needs nothing.
            if (PermissionEngine.MissingGrant(_hierarchy, DefOf(actor), permissions & ~current.Permissions) is { } missing)
            {
                return OpResult.Denied(missing);
            }

            if (role.IconEmoji.Length > 0 && iconImageId is not null)
            {
                return OpResult.Invalid("icon");
            }

            previousIcon = current.IconImageId;
            wanted = current with
            {
                Name = role.Name,
                Color = color,
                IconEmoji = role.IconEmoji,
                IconImageId = iconImageId,
                Permissions = permissions,
                Hoist = role.Hoist,
            };
        }

        if (iconImageId is { } icon
            && icon != previousIcon
            && !await images.ResolveForAsync(icon, Data.ImagePurpose.RoleIcon, null, ct))
        {
            return OpResult.Of(OpStatus.UnknownImage);
        }

        var (found, replacedIcon) = await _roles.UpdateAsync(wanted, ct);
        if (!found)
        {
            return OpResult.Of(OpStatus.UnknownRole);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            if (_roleById.TryGetValue(role.Id, out var latest))
            {
                // Position is whatever the mirror has: reordering is its own frame.
                var updated = latest with
                {
                    Name = wanted.Name,
                    Color = wanted.Color,
                    IconEmoji = wanted.IconEmoji,
                    IconImageId = wanted.IconImageId,
                    Permissions = wanted.Permissions,
                    Hoist = wanted.Hoist,
                };
                _roleById[updated.Id] = updated;
                RebuildHierarchyLocked();

                BroadcastToAllLocked(RoleUpsertedOf(updated), except: null, ref slow);

                // Everyone holding the role may resolve differently now.
                ReevaluateLocked(null, null, ref slow, removed, silenced);
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);

        // The write saw the whole picture: an icon the update left referenced by nothing goes now
        // rather than at the sweeper's next pass.
        if (replacedIcon is { } dropped)
        {
            await images.DeleteAsync([dropped], ct);
        }

        return OpResult.Ok;
    }

    public async Task<OpResult> DeleteRoleAsync(long actorId, long roleId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_roleById.TryGetValue(roleId, out var current))
            {
                return OpResult.Of(OpStatus.UnknownRole);
            }

            if (current.IsEveryone)
            {
                return OpResult.Invalid("role");
            }

            var manage = ManageRoleLocked(actor, current);
            if (!manage.IsOk)
            {
                return manage;
            }
        }

        if (!await _roles.DeleteAsync(roleId, ct))
        {
            return OpResult.Of(OpStatus.UnknownRole);
        }

        var affected = await _channels.RemoveOverridesForRoleAsync(roleId, ct);

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            _roleById.Remove(roleId);

            // Dense positions, renumbered exactly as the directory did, so the next insert lands
            // where its creator meant it to.
            var next = 1;
            foreach (var role in _roleById.Values
                .Where(role => !role.IsEveryone)
                .OrderBy(role => role.Position)
                .ThenBy(role => role.Id)
                .ToList())
            {
                _roleById[role.Id] = role with { Position = next++ };
            }

            var holders = new List<MemberState>();
            foreach (var member in _memberById.Values)
            {
                if (member.Roles.Remove(roleId))
                {
                    holders.Add(member);
                }
            }

            var touched = new List<ChannelState>();
            foreach (var channelId in affected)
            {
                if (_channelById.TryGetValue(channelId, out var channel)
                    && channel.Overrides.Remove((Data.OverrideTarget.Role, roleId)))
                {
                    RebuildDefLocked(channel);
                    touched.Add(channel);
                }
            }

            RebuildHierarchyLocked();

            BroadcastToAllLocked(RoleDeletedOf(roleId), except: null, ref slow);
            BroadcastToAllLocked(RoleOrderOf(), except: null, ref slow);
            foreach (var channel in touched)
            {
                BroadcastToChannelLocked(channel, ChannelUpsertedOf(channel), except: null, ref slow);
            }

            foreach (var member in holders)
            {
                BroadcastToAllLocked(MemberUpdatedOf(member), except: null, ref slow);
            }

            ReevaluateLocked(null, null, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> ReorderRolesAsync(long actorId, IReadOnlyList<long> idsBottomFirst, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageRoles);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            if (idsBottomFirst.Count == 0)
            {
                return OpResult.Invalid("ids");
            }

            var seen = new HashSet<long>();
            foreach (var id in idsBottomFirst)
            {
                if (!seen.Add(id))
                {
                    return OpResult.Invalid("ids");
                }

                if (!_roleById.ContainsKey(id))
                {
                    return OpResult.Of(OpStatus.UnknownRole);
                }

                // @everyone is pinned at position 0 and is never part of the order.
                if (id == _everyoneRoleId)
                {
                    return OpResult.Invalid("ids");
                }
            }

            if (idsBottomFirst.Count != _roleById.Values.Count(role => !role.IsEveryone))
            {
                return OpResult.Invalid("ids");
            }

            var actorDef = DefOf(actor);
            if (!PermissionEngine.IsOwner(_hierarchy, actorDef))
            {
                // A caller may only move roles that are below it, and only to positions that stay
                // below it: PROTOCOL.md § Hierarchy.
                var highest = PermissionEngine.HighestPosition(_hierarchy, actorDef);
                for (var index = 0; index < idsBottomFirst.Count; index++)
                {
                    var role = _roleById[idsBottomFirst[index]];
                    var position = index + 1;
                    if (role.Position != position && (role.Position >= highest || position >= highest))
                    {
                        return OpResult.Of(OpStatus.Hierarchy);
                    }
                }
            }
        }

        if (!await _roles.ReorderAsync(idsBottomFirst, ct))
        {
            return OpResult.Invalid("ids");
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            for (var index = 0; index < idsBottomFirst.Count; index++)
            {
                if (_roleById.TryGetValue(idsBottomFirst[index], out var role))
                {
                    _roleById[role.Id] = role with { Position = index + 1 };
                }
            }

            RebuildHierarchyLocked();

            var order = new RoleOrder();
            order.Ids.AddRange(idsBottomFirst);
            BroadcastToAllLocked(new ServerFrame { RoleOrder = order }, except: null, ref slow);

            // The order decides which override wins a conflict, so everything resolves again.
            ReevaluateLocked(null, null, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> SetMemberRolesAsync(
        long actorId,
        long userId,
        IReadOnlyList<long> roleIds,
        CancellationToken ct)
    {
        var wanted = new HashSet<long>();
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_memberById.TryGetValue(userId, out var target))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageRoles);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            foreach (var roleId in roleIds)
            {
                // @everyone is held implicitly and is never assigned.
                if (roleId == _everyoneRoleId)
                {
                    return OpResult.Invalid("role_ids");
                }

                if (!_roleById.ContainsKey(roleId))
                {
                    return OpResult.Of(OpStatus.UnknownRole);
                }

                wanted.Add(roleId);
            }

            // PROTOCOL.md § Hierarchy: assigning itself a role it may manage is one of the two
            // things a member may do to itself, so on self there is no target check at all — the
            // engine reads the owner acting on itself as IsOwner, and a member acting on itself as
            // IsSelf — and the per-role check below is the whole rule: strictly below its own
            // highest position, the owner exempt.
            if (userId != actorId)
            {
                var verdict = PermissionEngine.CanTarget(_hierarchy, DefOf(actor), DefOf(target));
                if (verdict is TargetVerdict.IsOwner)
                {
                    return OpResult.Of(OpStatus.Forbidden);
                }

                if (verdict is TargetVerdict.Outranked)
                {
                    return OpResult.Of(OpStatus.Hierarchy);
                }
            }

            foreach (var roleId in wanted.Except(target.Roles).Concat(target.Roles.Except(wanted)))
            {
                if (_roleById.TryGetValue(roleId, out var role)
                    && !PermissionEngine.CanManageRole(_hierarchy, DefOf(actor), RoleDefOf(role)))
                {
                    return OpResult.Of(OpStatus.Hierarchy);
                }
            }
        }

        await _roles.SetMemberRolesAsync(userId, wanted, ct);

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            if (_memberById.TryGetValue(userId, out var target))
            {
                target.Roles.Clear();
                foreach (var roleId in wanted)
                {
                    if (_roleById.TryGetValue(roleId, out var role) && !role.IsEveryone)
                    {
                        target.Roles.Add(roleId);
                    }
                }

                BroadcastToAllLocked(MemberUpdatedOf(target), except: null, ref slow);
                ReevaluateLocked(null, [userId], ref slow, removed, silenced);
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> SetNicknameAsync(long actorId, long userId, string nickname, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_memberById.TryGetValue(userId, out var target))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            if (userId == actorId)
            {
                var own = CheckLocked(actor, null, Perm.ChangeNickname);
                if (!own.IsOk)
                {
                    return own;
                }
            }
            else
            {
                var allowed = CheckLocked(actor, null, Perm.ManageMembers);
                if (!allowed.IsOk)
                {
                    return allowed;
                }

                var reachable = TargetLocked(actor, target);
                if (!reachable.IsOk)
                {
                    return reachable;
                }
            }
        }

        if (!await _members.SetNicknameAsync(userId, nickname, ct))
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_memberById.TryGetValue(userId, out var target))
            {
                target.Profile = target.Profile with { Nickname = nickname.Length == 0 ? null : nickname };
                BroadcastToAllLocked(MemberUpdatedOf(target), except: null, ref slow);
            }
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> UpdateProfileAsync(
        long actorId,
        string description,
        uint accentColor,
        long avatarImageId,
        long bannerImageId,
        ImageStore images,
        CancellationToken ct)
    {
        if (accentColor > MaxColor)
        {
            return OpResult.Invalid("accent_color");
        }

        lock (_gate)
        {
            if (!_memberById.ContainsKey(actorId))
            {
                return OpResult.Of(OpStatus.NotLive);
            }
        }

        var (avatarResult, avatar) = await ResolveImageAsync(
            images,
            avatarImageId,
            Data.ImagePurpose.Avatar,
            actorId,
            "avatar_image_id",
            ct);
        if (!avatarResult.IsOk)
        {
            return avatarResult;
        }

        var (bannerResult, banner) = await ResolveImageAsync(
            images,
            bannerImageId,
            Data.ImagePurpose.Banner,
            actorId,
            "banner_image_id",
            ct);
        if (!bannerResult.IsOk)
        {
            return bannerResult;
        }

        var accent = (int)accentColor;
        var (ok, replaced) = await _members.UpdateProfileAsync(actorId, description, accent, avatar, banner, ct);
        if (!ok)
        {
            return OpResult.Of(OpStatus.NotLive);
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (_memberById.TryGetValue(actorId, out var member))
            {
                var profile = member.Profile with
                {
                    Description = description,
                    AccentColor = accent == 0 ? null : accent,
                };

                if (avatar is { } avatarWrite)
                {
                    profile = profile with { AvatarImageId = avatarWrite == 0 ? null : avatarWrite };
                }

                if (banner is { } bannerWrite)
                {
                    profile = profile with { BannerImageId = bannerWrite == 0 ? null : bannerWrite };
                }

                member.Profile = profile;
                BroadcastToAllLocked(MemberUpdatedOf(member), except: null, ref slow);
            }
        }

        CloseSlow(slow);

        if (replaced.Count > 0)
        {
            await images.DeleteAsync(replaced, ct);
        }

        return OpResult.Ok;
    }

    public async Task<OpResult> KickAsync(long actorId, long userId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_memberById.TryGetValue(userId, out var target))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            var allowed = CheckLocked(actor, null, Perm.KickMembers);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            var reachable = TargetLocked(actor, target);
            if (!reachable.IsOk)
            {
                return reachable;
            }
        }

        await _members.KickAsync(userId, DateTime.UtcNow, ct);

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        ClientConnection? kicked;
        lock (_gate)
        {
            kicked = EndSessionLocked(userId, ref slow, removed);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        if (kicked is not null)
        {
            FailLater(kicked, ErrorCode.Kicked, "kicked");
        }

        return OpResult.Ok;
    }

    public async Task<OpResult> BanAsync(
        long actorId,
        long userId,
        string reason,
        AttachmentStore attachments,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_memberById.TryGetValue(userId, out var target))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            var allowed = CheckLocked(actor, null, Perm.BanMembers);
            if (!allowed.IsOk)
            {
                return allowed;
            }

            var reachable = TargetLocked(actor, target);
            if (!reachable.IsOk)
            {
                return reachable;
            }
        }

        var outcome = await _members.BanAsync(userId, actorId, reason, DateTime.UtcNow, ct);
        if (!outcome.Found || outcome.AlreadyBanned)
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        ClientConnection? banned;
        lock (_gate)
        {
            // The tombstones first, while every viewer of their channels still holds it in Visible.
            foreach (var (channelId, messageId) in outcome.Tombstoned)
            {
                if (_channelById.TryGetValue(channelId, out var channel))
                {
                    BroadcastToChannelLocked(channel, MessageDeletedOf(channelId, messageId), except: null, ref slow);
                }
            }

            banned = EndSessionLocked(userId, ref slow, removed);

            // The ban's own transaction dropped these rows, so the mirror only follows them.
            foreach (var channelId in outcome.OverrideChannels)
            {
                if (_channelById.TryGetValue(channelId, out var channel)
                    && channel.Overrides.Remove((Data.OverrideTarget.User, userId)))
                {
                    RebuildDefLocked(channel);
                    BroadcastToChannelLocked(channel, ChannelUpsertedOf(channel), except: null, ref slow);
                }
            }

            // A banned account is no longer a member: nothing about it resolves from here on.
            _memberById.Remove(userId);
            BroadcastToAllLocked(MemberRemovedOf(userId), except: null, ref slow);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        if (banned is not null)
        {
            FailLater(banned, ErrorCode.Banned, "banned");
        }

        attachments.DeleteFiles(outcome.AttachmentFiles);
        return OpResult.Ok;
    }

    public async Task<OpResult> UnbanAsync(long actorId, long userId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            // No hierarchy check: the target is not a member, so it outranks nobody.
            var allowed = CheckLocked(actor, null, Perm.BanMembers);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        if (!await _members.UnbanAsync(userId, ct))
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        var record = await _members.GetAsync(userId, ct);
        if (record is null)
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        var roleIds = await _roles.RolesOfAsync(userId, ct);

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_memberById.TryGetValue(userId, out var member))
            {
                member = new MemberState(record);
                _memberById[userId] = member;
            }
            else
            {
                member.Profile = record;
            }

            member.Roles.Clear();
            foreach (var roleId in roleIds)
            {
                if (_roleById.TryGetValue(roleId, out var role) && !role.IsEveryone)
                {
                    member.Roles.Add(roleId);
                }
            }

            BroadcastToAllLocked(MemberUpdatedOf(member), except: null, ref slow);
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    public async Task<OpResult> VoiceModerateAsync(
        long actorId,
        long userId,
        long channelId,
        bool? muted,
        bool? deafened,
        long? moveTo,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (!_memberById.TryGetValue(userId, out var target))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            if (!VisibleLocked(actor, channelId, out var channel))
            {
                return OpResult.Of(OpStatus.UnknownChannel);
            }

            if (!channel.Voice.ContainsKey(userId))
            {
                return OpResult.Of(OpStatus.NotInVoice);
            }

            var reachable = TargetLocked(actor, target);
            if (!reachable.IsOk)
            {
                return reachable;
            }

            if (muted is null && deafened is null && moveTo is null)
            {
                return OpResult.Invalid("voice_moderate");
            }

            // The bits are resolved in the channel the target is in: PROTOCOL.md § Moderation.
            if (muted is not null)
            {
                var allowed = CheckLocked(actor, channel, Perm.MuteMembers);
                if (!allowed.IsOk)
                {
                    return allowed;
                }
            }

            if (deafened is not null)
            {
                var allowed = CheckLocked(actor, channel, Perm.DeafenMembers);
                if (!allowed.IsOk)
                {
                    return allowed;
                }
            }

            if (moveTo is { } destination)
            {
                var allowed = CheckLocked(actor, channel, Perm.MoveMembers);
                if (!allowed.IsOk)
                {
                    return allowed;
                }

                // 0 disconnects instead of moving, and needs nothing of the channel it leaves.
                if (destination != 0)
                {
                    if (!VisibleLocked(actor, destination, out var into))
                    {
                        return OpResult.Of(OpStatus.UnknownChannel);
                    }

                    if (into.Record.Kind is not (Data.ChannelKind.Voice or Data.ChannelKind.Dm)
                        || destination == channelId)
                    {
                        return OpResult.Invalid("move_to");
                    }

                    var allowedThere = CheckLocked(actor, into, Perm.MoveMembers);
                    if (!allowedThere.IsOk)
                    {
                        return allowedThere;
                    }

                    if (!Perms.Has(ResolveLocked(target, into), Perm.Connect))
                    {
                        return OpResult.Invalid("move_to");
                    }
                }
            }
        }

        if ((muted is not null || deafened is not null)
            && !await _members.SetVoiceFlagsAsync(userId, muted, deafened, ct))
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            if (_memberById.TryGetValue(userId, out var target))
            {
                var flagged = muted is not null || deafened is not null;
                if (flagged)
                {
                    target.Profile = target.Profile with
                    {
                        ServerMuted = muted ?? target.Profile.ServerMuted,
                        ServerDeafened = deafened ?? target.Profile.ServerDeafened,
                    };
                }

                var channel = _channelById.GetValueOrDefault(channelId);

                // A move-only frame changes no flag, so the session it is about to end is left
                // alone rather than restated to the channel first.
                if (flagged && channel is not null && channel.Voice.TryGetValue(userId, out var slot))
                {
                    // A member without SPEAK stays muted whatever the moderators asked for.
                    slot.Muted = target.Profile.ServerMuted
                        || !Perms.Has(ResolveLocked(target, channel), Perm.Speak);
                    slot.Deafened = target.Profile.ServerDeafened;

                    // SetModeration takes only the relay's own lock and returns whether the mute
                    // ended a talk spurt; the Speaking frame for it goes out after this lock.
                    if (_relay.SetModeration(slot.Session.Ssrc, slot.Muted, slot.Deafened, slot.Priority))
                    {
                        silenced.Add((channelId, userId));
                    }

                    BroadcastToChannelLocked(channel, VoiceStateOf(channel), except: null, ref slow);
                }

                if (flagged)
                {
                    BroadcastToAllLocked(MemberUpdatedOf(target), except: null, ref slow);
                }

                // After the flags, because they are persisted: the target rejoins with
                // JoinVoice{move_to} itself and picks them up there.
                if (moveTo is { } destination && channel is not null && channel.Voice.ContainsKey(userId))
                {
                    RemoveVoiceLocked(channel, userId, ref slow, removed);
                    SendToLocked(userId, VoiceMovedOf(destination), ref slow);
                }
            }
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    public async Task<OpResult> UpdateServerAsync(
        long actorId,
        string name,
        string description,
        long iconImageId,
        ImageStore images,
        CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.TryGetValue(actorId, out var actor))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            var allowed = CheckLocked(actor, null, Perm.ManageServer);
            if (!allowed.IsOk)
            {
                return allowed;
            }
        }

        if (iconImageId < 0)
        {
            return OpResult.Invalid("icon_image_id");
        }

        if (iconImageId != 0 && !await images.ResolveForAsync(iconImageId, Data.ImagePurpose.ServerIcon, null, ct))
        {
            return OpResult.Of(OpStatus.UnknownImage);
        }

        // The frame always carries the field, so the directory is never asked to keep the icon:
        // 0 clears it (PROTOCOL.md § Moderation).
        var (server, replacedIcon) = await _server.UpdateAsync(name, description, iconImageId, ct);

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            _facts.Name = server.Name;
            _facts.Description = server.Description;
            _facts.IconImageId = server.IconImageId;
            BroadcastToAllLocked(ServerUpdatedOf(), except: null, ref slow);
        }

        CloseSlow(slow);

        if (replacedIcon is { } dropped)
        {
            await images.DeleteAsync([dropped], ct);
        }

        return OpResult.Ok;
    }

    public async Task<OpResult> TransferOwnershipAsync(long actorId, long userId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.ContainsKey(actorId))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            if (_facts.OwnerId is not { } owner || owner != actorId)
            {
                return OpResult.Of(OpStatus.Forbidden);
            }

            if (!_memberById.ContainsKey(userId))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }

            if (userId == owner)
            {
                return OpResult.Of(OpStatus.Forbidden);
            }
        }

        if (!await _server.SetOwnerAsync(userId, ct))
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            _facts.OwnerId = userId;
            RebuildHierarchyLocked();
            BroadcastToAllLocked(ServerUpdatedOf(), except: null, ref slow);

            // The old owner loses its bypass and the new one gains it.
            ReevaluateLocked(null, null, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
        return OpResult.Ok;
    }

    // The registration endpoint's follow-up to MemberRegistered, for a server nobody owns yet.
    // Unlike TransferOwnershipAsync there is no actor to authorise: the database decides, and it
    // only says yes while the owner column is still null.
    public async Task ClaimOwnershipIfUnownedAsync(long userId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (_facts.OwnerId is not null)
            {
                return;
            }
        }

        if (!await _server.ClaimOwnerAsync(userId, ct))
        {
            return;
        }

        List<ClientConnection>? slow = null;
        List<uint> removed = [];
        List<(long ChannelId, long UserId)> silenced = [];
        lock (_gate)
        {
            _facts.OwnerId = userId;
            RebuildHierarchyLocked();
            BroadcastToAllLocked(ServerUpdatedOf(), except: null, ref slow);

            // The new owner gains the bypass over every check.
            ReevaluateLocked(null, null, ref slow, removed, silenced);
        }

        CloseSlow(slow);
        ReleaseVoice(removed);
        AnnounceSilenced(silenced);
    }

    public async Task<OpResult> OpenDmAsync(long callerId, long otherId, CancellationToken ct)
    {
        lock (_gate)
        {
            if (!_memberById.ContainsKey(callerId))
            {
                return OpResult.Of(OpStatus.NotLive);
            }

            // A DM with yourself is UNKNOWN_USER, like a stranger or a banned account.
            if (callerId == otherId || !_memberById.ContainsKey(otherId))
            {
                return OpResult.Of(OpStatus.UnknownUser);
            }
        }

        var outcome = await _channels.OpenDmAsync(callerId, otherId, ct);
        if (outcome.Channel is not { } record)
        {
            return OpResult.Of(OpStatus.UnknownUser);
        }

        List<ClientConnection>? slow = null;
        lock (_gate)
        {
            if (!_channelById.TryGetValue(record.Id, out var channel))
            {
                channel = new ChannelState(record);
                _channelById[record.Id] = channel;
                RebuildDefLocked(channel);
            }

            // The sweep skips DMs — their visibility is membership, not resolution — so the two
            // parties' own Visible sets are maintained here.
            foreach (var partyId in new[] { callerId, otherId })
            {
                if (_online.ContainsKey(partyId) && _memberById.TryGetValue(partyId, out var party))
                {
                    party.Visible.Add(record.Id);
                }
            }

            if (outcome.Status == DmOutcome.Kind.Opened)
            {
                foreach (var partyId in new[] { callerId, otherId })
                {
                    SendToLocked(partyId, ChannelUpsertedOf(channel), ref slow);
                    SendToLocked(partyId, VoiceStateOf(channel), ref slow);
                }
            }
            else
            {
                // An idempotent resync: the other party already knows about the channel.
                SendToLocked(callerId, ChannelUpsertedOf(channel), ref slow);
                SendToLocked(callerId, VoiceStateOf(channel), ref slow);
            }
        }

        CloseSlow(slow);
        return OpResult.Ok;
    }

    private static RoleDef RoleDefOf(RoleRecord role) => new(role.Id, role.Position, role.Permissions, role.IsEveryone);

    // The wire's sentinels for a profile image: 0 keeps what is there, -1 clears it, and anything
    // else must name an image of that purpose the caller may use. The write is coded the way
    // MemberDirectory wants it, where null keeps and 0 clears.
    private static async Task<(OpResult Result, long? Write)> ResolveImageAsync(
        ImageStore images,
        long raw,
        Data.ImagePurpose purpose,
        long? uploader,
        string field,
        CancellationToken ct)
    {
        if (raw == 0)
        {
            return (OpResult.Ok, null);
        }

        if (raw == -1)
        {
            return (OpResult.Ok, 0);
        }

        if (raw < 0)
        {
            return (OpResult.Invalid(field), null);
        }

        return await images.ResolveForAsync(raw, purpose, uploader, ct)
            ? (OpResult.Ok, raw)
            : (OpResult.Of(OpStatus.UnknownImage), null);
    }

    private static ServerFrame RoleUpsertedOf(RoleRecord role)
        => new() { RoleUpserted = new RoleUpserted { Role = RoleOf(role) } };

    private static ServerFrame RoleDeletedOf(long roleId)
        => new() { RoleDeleted = new RoleDeleted { Id = roleId } };

    private static ServerFrame CategoryUpsertedOf(CategoryRecord category)
        => new() { CategoryUpserted = new CategoryUpserted { Category = CategoryOf(category) } };

    private static ServerFrame CategoryDeletedOf(long categoryId)
        => new() { CategoryDeleted = new CategoryDeleted { Id = categoryId } };

    private static ServerFrame MemberRemovedOf(long userId)
        => new() { MemberRemoved = new MemberRemoved { UserId = userId } };

    private static ServerFrame MessageDeletedOf(long channelId, long messageId)
        => new() { MessageDeleted = new MessageDeleted { ChannelId = channelId, Id = messageId } };

    private ServerFrame ServerUpdatedOf() => new() { ServerUpdated = new ServerUpdated { Server = ServerOf() } };

    private OpResult CheckLocked(MemberState actor, ChannelState? channel, Perm bit)
        => Perms.Has(ResolveLocked(actor, channel), bit) ? OpResult.Ok : OpResult.Denied(bit);

    private OpResult TargetLocked(MemberState actor, MemberState target)
        => PermissionEngine.CanTarget(_hierarchy, DefOf(actor), DefOf(target)) switch
        {
            TargetVerdict.Allowed => OpResult.Ok,
            TargetVerdict.Outranked => OpResult.Of(OpStatus.Hierarchy),

            // The owner and the caller itself.
            _ => OpResult.Of(OpStatus.Forbidden),
        };

    private OpResult ManageRoleLocked(MemberState actor, RoleRecord target)
    {
        if (!Perms.Has(ResolveLocked(actor, null), Perm.ManageRoles))
        {
            return OpResult.Denied(Perm.ManageRoles);
        }

        return PermissionEngine.CanManageRole(_hierarchy, DefOf(actor), RoleDefOf(target))
            ? OpResult.Ok
            : OpResult.Of(OpStatus.Hierarchy);
    }

    // DMs are opened, not created, so they are outside the channel cap.
    private int NonDmChannelCountLocked() => _channelById.Values.Count(channel => !channel.IsDm);

    // Every role but @everyone, bottom first, which is the order PROTOCOL.md § Roles and
    // permissions gives RoleOrder.
    private ServerFrame RoleOrderOf()
    {
        var order = new RoleOrder();
        foreach (var role in _roleById.Values
            .Where(role => !role.IsEveryone)
            .OrderBy(role => role.Position)
            .ThenBy(role => role.Id))
        {
            order.Ids.Add(role.Id);
        }

        return new ServerFrame { RoleOrder = order };
    }

    // Every non-DM channel in sidebar order; a DM has no position anybody sorts by.
    private ServerFrame ChannelOrderOf()
    {
        var channels = new List<ChannelState>(_channelById.Count);
        foreach (var channel in _channelById.Values)
        {
            if (!channel.IsDm)
            {
                channels.Add(channel);
            }
        }

        channels.Sort(CompareChannelsLocked);

        var order = new ChannelOrder();
        foreach (var channel in channels)
        {
            order.Positions.Add(new ChannelPosition
            {
                Id = channel.Record.Id,
                CategoryId = channel.Record.CategoryId ?? 0,
                Position = channel.Record.Position,
            });
        }

        return new ServerFrame { ChannelOrder = order };
    }

    // The presence half of a kick or a ban: the voice sessions end, the account stops being online
    // and every other member hears about it. The connection is returned for the caller to fail
    // after the lock; null when the account was not online.
    private ClientConnection? EndSessionLocked(long userId, ref List<ClientConnection>? slow, List<uint> removed)
    {
        if (!_online.TryGetValue(userId, out var connection))
        {
            return null;
        }

        // VoiceMemberLeft while this connection is still a viewer of the channels it was in.
        RemoveVoiceEverywhereLocked(userId, ref slow, removed);
        _online.Remove(userId);

        if (_memberById.TryGetValue(userId, out var member))
        {
            member.Visible.Clear();
            BroadcastToAllLocked(MemberUpdatedOf(member), except: userId, ref slow);
        }

        return connection;
    }

    // Fire-and-forget, because the frame and the close go to a socket this caller no longer owns
    // and must not wait on.
    private void FailLater(ClientConnection connection, ErrorCode code, string reason)
        => _ = FailQuietlyAsync(connection, code, reason);

    private async Task FailQuietlyAsync(ClientConnection connection, ErrorCode code, string reason)
    {
        try
        {
            await connection.FailAsync(
                new Error { Code = code, Fatal = true },
                WebSocketCloseStatus.PolicyViolation,
                reason);
        }
        catch (Exception ex)
        {
            _logger.LogDebug(ex, "Connection {ConnectionId}: failed to close with {ErrorCode}", connection.Id, code);
        }
    }
}

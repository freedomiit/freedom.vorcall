using Microsoft.EntityFrameworkCore;
using Npgsql;
using Vorcall.Server.Attachments;
using Vorcall.Server.Data;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Chat;

// The persisted facts about one channel. DmLow and DmHigh are the two members of a DM, lower id
// first, and null for every other kind.
public sealed record ChannelRecord(
    long Id,
    ChannelKind Kind,
    string Name,
    string Topic,
    long? CategoryId,
    int Position,
    long? DmLow,
    long? DmHigh);

public sealed record CategoryRecord(long Id, string Name, int Position);

// Allow and Deny are the wire's uint64 and carry channel-scoped bits only; the entity stores the
// same numbers as bigint.
public sealed record OverrideRecord(long ChannelId, OverrideTarget TargetKind, long TargetId, ulong Allow, ulong Deny);

// One reader's counters for one channel. The cursor itself never leaves the database: a client is
// only ever told what it has left to read.
public sealed record ReadStateRecord(long ChannelId, long Unread, long Mentions, long LastMessageId);

// AttachmentFiles are the paths the cascade orphaned, for the caller to delete once it has
// broadcast the deletion.
public sealed record DeleteChannelOutcome(bool Found, List<string> AttachmentFiles)
{
    public static DeleteChannelOutcome NotFound() => new(false, []);
}

// Channel is set when Status is Opened or Existing.
public sealed record DmOutcome(DmOutcome.Kind Status, ChannelRecord? Channel)
{
    public enum Kind
    {
        Opened,
        Existing,
        UnknownUser,
        Self,
        Banned,
    }

    public static DmOutcome UnknownUser { get; } = new(Kind.UnknownUser, null);

    public static DmOutcome Self { get; } = new(Kind.Self, null);

    public static DmOutcome Banned { get; } = new(Kind.Banned, null);

    public static DmOutcome Opened(ChannelRecord channel) => new(Kind.Opened, channel);

    public static DmOutcome Existing(ChannelRecord channel) => new(Kind.Existing, channel);
}

// Every persistent channel, category, override and read-cursor write goes through here. The
// connection registry mirrors the result in memory for visibility and presence; nothing in this
// class touches that mirror, and nothing here checks a permission or a cap — the caller does.
public sealed class ChannelDirectory(
    IDbContextFactory<AppDbContext> contexts,
    AttachmentStore attachments,
    ILogger<ChannelDirectory> logger)
{
    public async Task<(List<CategoryRecord> Categories, List<ChannelRecord> Channels, List<OverrideRecord> Overrides)> LoadAllAsync(
        CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);

        var categories = await db.Categories
            .AsNoTracking()
            .OrderBy(c => c.Position)
            .ThenBy(c => c.Id)
            .ToListAsync(ct);

        var channels = await db.Channels
            .AsNoTracking()
            .OrderBy(c => c.Position)
            .ThenBy(c => c.Id)
            .ToListAsync(ct);

        var overrides = await db.ChannelOverrides
            .AsNoTracking()
            .OrderBy(o => o.ChannelId)
            .ThenBy(o => o.TargetKind)
            .ThenBy(o => o.TargetId)
            .ToListAsync(ct);

        return (
            categories.Select(ToRecord).ToList(),
            channels.Select(ToRecord).ToList(),
            overrides.Select(ToRecord).ToList());
    }

    // Appended to its category. DMs carry no category and no meaningful position, so they stay
    // out of the maximum that decides where the new channel lands.
    public async Task<ChannelRecord> CreateChannelAsync(
        ChannelKind kind,
        string name,
        string topic,
        long? categoryId,
        CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var last = await db.Channels
            .Where(c => c.Kind != ChannelKind.Dm && c.CategoryId == categoryId)
            .MaxAsync(c => (int?)c.Position, ct);

        var channel = new Data.Channel
        {
            Kind = kind,
            Name = name,
            Topic = topic,
            CategoryId = categoryId,
            Position = (last ?? -1) + 1,
            CreatedAt = DateTime.UtcNow,
        };

        db.Channels.Add(channel);
        await db.SaveChangesAsync(ct);
        logger.LogDebug("Channel {ChannelId} created ({Kind})", channel.Id, kind);
        return ToRecord(channel);
    }

    public async Task<ChannelRecord?> UpdateChannelAsync(long id, string name, string topic, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var channel = await db.Channels.FirstOrDefaultAsync(c => c.Id == id, ct);

        // A DM's name is always empty and it has no topic, so it reads as unknown here rather
        // than being renamed into something the protocol says it cannot have.
        if (channel is null || channel.Kind == ChannelKind.Dm)
        {
            return null;
        }

        channel.Name = name;
        channel.Topic = topic;
        await db.SaveChangesAsync(ct);
        return ToRecord(channel);
    }

    // The row delete cascades the channel's messages, attachments, reactions, read cursors and
    // overrides, so the attachment paths are read while their rows are still there.
    public async Task<DeleteChannelOutcome> DeleteChannelAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);
        if (!await db.Channels.AnyAsync(c => c.Id == id, ct))
        {
            return DeleteChannelOutcome.NotFound();
        }

        var files = await FilesOfAsync(db.Attachments.Where(a => a.ChannelId == id), ct);
        await db.Channels.Where(c => c.Id == id).ExecuteDeleteAsync(ct);
        await transaction.CommitAsync(ct);
        logger.LogDebug("Channel {ChannelId} deleted with {FileCount} attachment files", id, files.Count);
        return new DeleteChannelOutcome(true, files);
    }

    public async Task<CategoryRecord> CreateCategoryAsync(string name, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var last = await db.Categories.MaxAsync(c => (int?)c.Position, ct);
        var category = new Data.Category { Name = name, Position = (last ?? -1) + 1 };
        db.Categories.Add(category);
        await db.SaveChangesAsync(ct);
        return ToRecord(category);
    }

    public async Task<CategoryRecord?> UpdateCategoryAsync(long id, string name, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var category = await db.Categories.FirstOrDefaultAsync(c => c.Id == id, ct);
        if (category is null)
        {
            return null;
        }

        category.Name = name;
        await db.SaveChangesAsync(ct);
        return ToRecord(category);
    }

    // The channels outlive the category: they move to "no category", appended after the channels
    // that already had none. An unknown id moves nothing, which the caller reads as an empty list.
    public async Task<List<ChannelRecord>> DeleteCategoryAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);
        if (!await db.Categories.AnyAsync(c => c.Id == id, ct))
        {
            return [];
        }

        var last = await db.Channels
            .Where(c => c.Kind != ChannelKind.Dm && c.CategoryId == null)
            .MaxAsync(c => (int?)c.Position, ct);

        var orphans = await db.Channels
            .Where(c => c.CategoryId == id)
            .OrderBy(c => c.Position)
            .ThenBy(c => c.Id)
            .ToListAsync(ct);

        var next = (last ?? -1) + 1;
        foreach (var channel in orphans)
        {
            channel.CategoryId = null;
            channel.Position = next++;
        }

        await db.SaveChangesAsync(ct);
        await db.Categories.Where(c => c.Id == id).ExecuteDeleteAsync(ct);
        await transaction.CommitAsync(ct);
        return orphans.Select(ToRecord).ToList();
    }

    // The caller sends the list it wants stored verbatim; this validates every id, every category
    // and the kinds, because a position written for a channel nobody can place is a sidebar that
    // cannot be sorted.
    public async Task<bool> ReorderChannelsAsync(
        IReadOnlyList<(long Id, long? CategoryId, int Position)> positions,
        CancellationToken ct)
    {
        if (positions.Count == 0)
        {
            return true;
        }

        var ids = positions.Select(p => p.Id).ToList();
        if (ids.Distinct().Count() != ids.Count)
        {
            return false;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var channels = await db.Channels.Where(c => ids.Contains(c.Id)).ToListAsync(ct);
        if (channels.Count != ids.Count || channels.Exists(c => c.Kind == ChannelKind.Dm))
        {
            return false;
        }

        var categoryIds = positions.Select(p => p.CategoryId).OfType<long>().Distinct().ToList();
        if (categoryIds.Count > 0
            && await db.Categories.CountAsync(c => categoryIds.Contains(c.Id), ct) != categoryIds.Count)
        {
            return false;
        }

        var byId = channels.ToDictionary(c => c.Id);
        foreach (var (id, categoryId, position) in positions)
        {
            var channel = byId[id];
            channel.CategoryId = categoryId;
            channel.Position = position;
        }

        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
        return true;
    }

    public async Task<bool> ReorderCategoriesAsync(IReadOnlyList<long> ids, CancellationToken ct)
    {
        if (ids.Count == 0)
        {
            return true;
        }

        var wanted = ids.ToList();
        if (wanted.Distinct().Count() != wanted.Count)
        {
            return false;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var categories = await db.Categories.Where(c => wanted.Contains(c.Id)).ToListAsync(ct);
        if (categories.Count != wanted.Count)
        {
            return false;
        }

        var byId = categories.ToDictionary(c => c.Id);
        for (var index = 0; index < wanted.Count; index++)
        {
            byId[wanted[index]].Position = index;
        }

        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
        return true;
    }

    // Server-scoped bits never live in an override (PROTOCOL.md § Roles and permissions), so a
    // pair that carried nothing else clears the row like an empty one would.
    public async Task<bool> SetOverrideAsync(
        long channelId,
        OverrideTarget kind,
        long targetId,
        ulong allow,
        ulong deny,
        CancellationToken ct)
    {
        var allowed = (long)(Perms.Clean(allow) & Perms.ChannelScoped);
        var denied = (long)(Perms.Clean(deny) & Perms.ChannelScoped);

        await using var db = await contexts.CreateDbContextAsync(ct);
        if (!await db.Channels.AnyAsync(c => c.Id == channelId, ct))
        {
            return false;
        }

        if (allowed == 0 && denied == 0)
        {
            await db.ChannelOverrides
                .Where(o => o.ChannelId == channelId && o.TargetKind == kind && o.TargetId == targetId)
                .ExecuteDeleteAsync(ct);
            return true;
        }

        var updated = await db.ChannelOverrides
            .Where(o => o.ChannelId == channelId && o.TargetKind == kind && o.TargetId == targetId)
            .ExecuteUpdateAsync(
                setters => setters
                    .SetProperty(o => o.Allow, allowed)
                    .SetProperty(o => o.Deny, denied),
                ct);
        if (updated > 0)
        {
            return true;
        }

        db.ChannelOverrides.Add(new Data.ChannelOverride
        {
            ChannelId = channelId,
            TargetKind = kind,
            TargetId = targetId,
            Allow = allowed,
            Deny = denied,
        });

        try
        {
            await db.SaveChangesAsync(ct);
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            // Another connection inserted the same override between the update and the insert;
            // the pair the caller asked for is still the one that must stand.
            await using var reread = await contexts.CreateDbContextAsync(ct);
            await reread.ChannelOverrides
                .Where(o => o.ChannelId == channelId && o.TargetKind == kind && o.TargetId == targetId)
                .ExecuteUpdateAsync(
                    setters => setters
                        .SetProperty(o => o.Allow, allowed)
                        .SetProperty(o => o.Deny, denied),
                    ct);
        }

        return true;
    }

    // The channels whose override set changed, so the caller can re-broadcast each of them. The
    // member variant lives in MemberDirectory.BanAsync, whose transaction it belongs to.
    public Task<List<long>> RemoveOverridesForRoleAsync(long roleId, CancellationToken ct)
        => RemoveOverridesAsync(OverrideTarget.Role, roleId, ct);

    public async Task<DmOutcome> OpenDmAsync(long callerId, long otherId, CancellationToken ct)
    {
        if (callerId == otherId)
        {
            return DmOutcome.Self;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        if (!await db.Users.AnyAsync(u => u.Id == otherId, ct))
        {
            return DmOutcome.UnknownUser;
        }

        if (await db.Bans.AnyAsync(b => b.UserId == otherId, ct))
        {
            return DmOutcome.Banned;
        }

        var low = Math.Min(callerId, otherId);
        var high = Math.Max(callerId, otherId);
        if (await FindDmAsync(db, low, high, ct) is { } existing)
        {
            return DmOutcome.Existing(existing);
        }

        var channel = new Data.Channel
        {
            Kind = ChannelKind.Dm,
            Name = string.Empty,
            Topic = string.Empty,
            CategoryId = null,
            Position = 0,
            CreatedAt = DateTime.UtcNow,
            DmLow = low,
            DmHigh = high,
        };

        db.Channels.Add(channel);
        try
        {
            await db.SaveChangesAsync(ct);
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            // Both sides opened the same DM at once. The pair is unique, so the row that won is
            // the one this caller wanted; a fresh context reads it, because the failed insert
            // leaves the Added entity behind in this one.
            await using var reread = await contexts.CreateDbContextAsync(ct);
            if (await FindDmAsync(reread, low, high, ct) is not { } raced)
            {
                throw;
            }

            return DmOutcome.Existing(raced);
        }

        logger.LogDebug("DM channel {ChannelId} opened by {UserId}", channel.Id, callerId);
        return DmOutcome.Opened(ToRecord(channel));
    }

    // The cursor only moves forward and never past the newest message of the channel, so a stale
    // MarkRead from a reconnecting client is a no-op.
    public async Task MarkReadAsync(long channelId, long userId, long messageId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var capped = Math.Min(messageId, await NewestMessageIdAsync(db, channelId, ct));
        if (capped <= 0)
        {
            return;
        }

        var updated = await db.ChannelReads
            .Where(r => r.ChannelId == channelId && r.UserId == userId && r.LastReadMessageId < capped)
            .ExecuteUpdateAsync(setters => setters.SetProperty(r => r.LastReadMessageId, capped), ct);
        if (updated > 0)
        {
            return;
        }

        if (await db.ChannelReads.AnyAsync(r => r.ChannelId == channelId && r.UserId == userId, ct))
        {
            return;
        }

        // No row yet: a DM, or a channel created after the account was. A missing row means the
        // whole channel is unread, so the first MarkRead is what creates it.
        db.ChannelReads.Add(new Data.ChannelRead
        {
            ChannelId = channelId,
            UserId = userId,
            LastReadMessageId = capped,
        });

        try
        {
            await db.SaveChangesAsync(ct);
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            // Two connections of this account created the cursor at once; the row that won only
            // has to be nudged forward.
            await using var reread = await contexts.CreateDbContextAsync(ct);
            await reread.ChannelReads
                .Where(r => r.ChannelId == channelId && r.UserId == userId && r.LastReadMessageId < capped)
                .ExecuteUpdateAsync(setters => setters.SetProperty(r => r.LastReadMessageId, capped), ct);
        }
    }

    public async Task<List<ReadStateRecord>> ReadStatesForAsync(
        long userId,
        IReadOnlyCollection<long> channelIds,
        CancellationToken ct)
    {
        var ids = channelIds.Distinct().ToList();
        if (ids.Count == 0)
        {
            return [];
        }

        await using var db = await contexts.CreateDbContextAsync(ct);

        var cursors = await db.ChannelReads
            .AsNoTracking()
            .Where(r => r.UserId == userId && ids.Contains(r.ChannelId))
            .ToDictionaryAsync(r => r.ChannelId, r => r.LastReadMessageId, ct);

        var lastMessageIds = await db.Messages
            .AsNoTracking()
            .Where(m => ids.Contains(m.ChannelId))
            .GroupBy(m => m.ChannelId)
            .Select(g => new { ChannelId = g.Key, LastId = g.Max(m => m.Id) })
            .ToDictionaryAsync(x => x.ChannelId, x => x.LastId, ct);

        // Two queries rather than one correlated subquery: a channel the reader holds no cursor
        // row for is wholly unread, which is a different WHERE and not a different value.
        var withCursor = ids.Where(cursors.ContainsKey).ToList();
        var withoutCursor = ids.Where(id => !cursors.ContainsKey(id)).ToList();

        var unread = new Dictionary<long, long>();
        var mentions = new Dictionary<long, long>();

        if (withCursor.Count > 0)
        {
            // Unread is everything above the reader's cursor that is neither a tombstone nor the
            // reader's own. A null user id is the history that predates accounts: not the
            // reader's, so it counts.
            var counted = from message in db.Messages.AsNoTracking()
                          join cursor in db.ChannelReads.AsNoTracking()
                              on message.ChannelId equals cursor.ChannelId
                          where cursor.UserId == userId
                              && withCursor.Contains(message.ChannelId)
                              && message.Id > cursor.LastReadMessageId
                              && message.DeletedAt == null
                              && message.UserId != userId
                          select message;
            await AccumulateAsync(counted, userId, unread, mentions, ct);
        }

        if (withoutCursor.Count > 0)
        {
            var counted = db.Messages
                .AsNoTracking()
                .Where(m => withoutCursor.Contains(m.ChannelId) && m.DeletedAt == null && m.UserId != userId);
            await AccumulateAsync(counted, userId, unread, mentions, ct);
        }

        return ids
            .Select(id => new ReadStateRecord(
                id,
                unread.GetValueOrDefault(id),
                mentions.GetValueOrDefault(id),
                lastMessageIds.GetValueOrDefault(id)))
            .ToList();
    }

    // DMs are not part of the channel cap: they are opened by members, not created.
    public async Task<int> CountChannelsAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Channels.CountAsync(c => c.Kind != ChannelKind.Dm, ct);
    }

    public async Task<int> CountCategoriesAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Categories.CountAsync(ct);
    }

    public async Task<int> CountOverridesAsync(long channelId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.ChannelOverrides.CountAsync(o => o.ChannelId == channelId, ct);
    }

    // Null only on a database that was never seeded, which Program.cs rules out before anything
    // listens.
    public async Task<long?> GeneralChannelIdAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Server
            .AsNoTracking()
            .Where(s => s.Id == Data.Server.RowId)
            .Select(s => s.GeneralChannelId)
            .FirstOrDefaultAsync(ct);
    }

    private static async Task AccumulateAsync(
        IQueryable<Data.Message> counted,
        long userId,
        Dictionary<long, long> unread,
        Dictionary<long, long> mentions,
        CancellationToken ct)
    {
        var unreadRows = await counted
            .GroupBy(m => m.ChannelId)
            .Select(g => new { ChannelId = g.Key, Count = g.LongCount() })
            .ToListAsync(ct);
        foreach (var row in unreadRows)
        {
            unread[row.ChannelId] = row.Count;
        }

        // @here is live-only and never counted; @everyone counts like a direct mention.
        var mentionRows = await counted
            .Where(m => m.MentionIds.Contains(userId) || m.MentionEveryone)
            .GroupBy(m => m.ChannelId)
            .Select(g => new { ChannelId = g.Key, Count = g.LongCount() })
            .ToListAsync(ct);
        foreach (var row in mentionRows)
        {
            mentions[row.ChannelId] = row.Count;
        }
    }

    private static async Task<ChannelRecord?> FindDmAsync(AppDbContext db, long low, long high, CancellationToken ct)
    {
        var channel = await db.Channels
            .AsNoTracking()
            .FirstOrDefaultAsync(c => c.Kind == ChannelKind.Dm && c.DmLow == low && c.DmHigh == high, ct);
        return channel is null ? null : ToRecord(channel);
    }

    private static async Task<long> NewestMessageIdAsync(AppDbContext db, long channelId, CancellationToken ct)
        => await db.Messages.Where(m => m.ChannelId == channelId).MaxAsync(m => (long?)m.Id, ct) ?? 0;

    private static bool IsUniqueViolation(DbUpdateException exception)
        => exception.InnerException is PostgresException { SqlState: PostgresErrorCodes.UniqueViolation };

    private static ChannelRecord ToRecord(Data.Channel channel) => new(
        channel.Id,
        channel.Kind,
        channel.Name,
        channel.Topic,
        channel.CategoryId,
        channel.Position,
        channel.DmLow,
        channel.DmHigh);

    private static CategoryRecord ToRecord(Data.Category category) => new(category.Id, category.Name, category.Position);

    private static OverrideRecord ToRecord(Data.ChannelOverride row) => new(
        row.ChannelId,
        row.TargetKind,
        row.TargetId,
        Perms.Clean((ulong)row.Allow) & Perms.ChannelScoped,
        Perms.Clean((ulong)row.Deny) & Perms.ChannelScoped);

    private async Task<List<long>> RemoveOverridesAsync(OverrideTarget kind, long targetId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var channelIds = await db.ChannelOverrides
            .AsNoTracking()
            .Where(o => o.TargetKind == kind && o.TargetId == targetId)
            .Select(o => o.ChannelId)
            .Distinct()
            .ToListAsync(ct);
        if (channelIds.Count == 0)
        {
            return [];
        }

        await db.ChannelOverrides
            .Where(o => o.TargetKind == kind && o.TargetId == targetId)
            .ExecuteDeleteAsync(ct);
        return channelIds;
    }

    private async Task<List<string>> FilesOfAsync(IQueryable<Data.Attachment> query, CancellationToken ct)
    {
        var rows = await query
            .AsNoTracking()
            .OrderBy(a => a.Id)
            .Select(a => new { a.Id, a.ContentType })
            .ToListAsync(ct);
        return attachments.PathsFor(rows.Select(row => (row.Id, row.ContentType)));
    }
}

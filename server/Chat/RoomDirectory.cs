using Microsoft.EntityFrameworkCore;
using Npgsql;
using Vorcall.Server.Data;

namespace Vorcall.Server.Chat;

// The persisted facts about a room. Counters are per reader and live in RoomEntryRecord.
public sealed record RoomRecord(string Id, RoomKind Kind, string Name, long? CreatedBy);

public sealed record RoomEntryRecord(
    RoomRecord Room,
    IReadOnlyList<long> MemberIds,
    long Unread,
    long Mentions,
    long LastMessageId);

// Room is set only when Status is Created.
public sealed record CreateOutcome(CreateOutcome.Kind Status, RoomRecord? Room)
{
    public enum Kind
    {
        Created,
        InvalidName,
        Exists,
    }

    public static CreateOutcome InvalidName { get; } = new(Kind.InvalidName, null);

    public static CreateOutcome Exists { get; } = new(Kind.Exists, null);

    public static CreateOutcome Created(RoomRecord room) => new(Kind.Created, room);
}

// Room is set when Status is Opened or Existing.
public sealed record DmOutcome(DmOutcome.Kind Status, RoomRecord? Room)
{
    public enum Kind
    {
        Opened,
        Existing,
        UnknownUser,
        Self,
    }

    public static DmOutcome UnknownUser { get; } = new(Kind.UnknownUser, null);

    public static DmOutcome Self { get; } = new(Kind.Self, null);

    public static DmOutcome Opened(RoomRecord room) => new(Kind.Opened, room);

    public static DmOutcome Existing(RoomRecord room) => new(Kind.Existing, room);
}

// Every persistent room and membership write goes through here. The connection registry mirrors
// the result in memory for presence; nothing in this class touches that mirror.
public sealed class RoomDirectory(IDbContextFactory<AppDbContext> contexts, ILogger<RoomDirectory> logger)
{
    public async Task<IReadOnlyList<(RoomRecord Room, IReadOnlyList<long> MemberIds)>> LoadAllAsync()
    {
        await using var db = await contexts.CreateDbContextAsync();
        var rooms = await db.Rooms.AsNoTracking().OrderBy(r => r.Id).ToListAsync();
        var members = await LoadMembersAsync(db, rooms.Select(r => r.Id).ToList());

        return rooms
            .Select(room => (ToRecord(room), members.GetValueOrDefault(room.Id, [])))
            .ToList();
    }

    public async Task<CreateOutcome> CreateAsync(string name, long creatorId)
    {
        if (!RoomNames.TryNormalizeName(name, out var displayName) || !RoomNames.TrySlug(displayName, out var slug))
        {
            return CreateOutcome.InvalidName;
        }

        await using var db = await contexts.CreateDbContextAsync();

        // The room and its creator's membership land together or not at all: a room nobody is a
        // member of would be unreachable and unleavable.
        await using var transaction = await db.Database.BeginTransactionAsync();
        if (await db.Rooms.AnyAsync(r => r.Id == slug))
        {
            return CreateOutcome.Exists;
        }

        var now = DateTime.UtcNow;
        var room = new Room
        {
            Id = slug,
            Kind = RoomKind.Public,
            Name = displayName,
            CreatedBy = creatorId,
            CreatedAt = now,
        };

        db.Rooms.Add(room);
        db.RoomMembers.Add(new RoomMember { RoomId = slug, UserId = creatorId, JoinedAt = now, LastReadMessageId = 0 });
        try
        {
            await db.SaveChangesAsync();
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            // Two creations of the same name raced past the existence check.
            return CreateOutcome.Exists;
        }

        await transaction.CommitAsync();
        logger.LogDebug("Room {RoomId} created by {UserId}", slug, creatorId);
        return CreateOutcome.Created(ToRecord(room));
    }

    public async Task<DmOutcome> OpenDmAsync(long callerId, long otherId)
    {
        if (callerId == otherId)
        {
            return DmOutcome.Self;
        }

        await using var db = await contexts.CreateDbContextAsync();
        if (!await db.Users.AnyAsync(u => u.Id == otherId))
        {
            return DmOutcome.UnknownUser;
        }

        var roomId = RoomNames.DmRoomId(callerId, otherId);
        var existing = await db.Rooms.AsNoTracking().FirstOrDefaultAsync(r => r.Id == roomId);
        if (existing is not null)
        {
            return DmOutcome.Existing(ToRecord(existing));
        }

        var now = DateTime.UtcNow;
        var room = new Room
        {
            Id = roomId,
            Kind = RoomKind.Dm,
            Name = string.Empty,
            CreatedBy = callerId,
            CreatedAt = now,
        };

        await using var transaction = await db.Database.BeginTransactionAsync();
        db.Rooms.Add(room);
        db.RoomMembers.Add(new RoomMember { RoomId = roomId, UserId = callerId, JoinedAt = now, LastReadMessageId = 0 });
        db.RoomMembers.Add(new RoomMember { RoomId = roomId, UserId = otherId, JoinedAt = now, LastReadMessageId = 0 });
        try
        {
            await db.SaveChangesAsync();
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            // Both sides opened the same DM at once. The id is derived from the two user ids, so
            // the row that won is the one this caller wanted; a fresh context reads it, because
            // the failed insert left this transaction aborted.
            await transaction.RollbackAsync();
            await using var reread = await contexts.CreateDbContextAsync();
            var raced = await reread.Rooms.AsNoTracking().FirstOrDefaultAsync(r => r.Id == roomId);
            if (raced is null)
            {
                throw;
            }

            return DmOutcome.Existing(ToRecord(raced));
        }

        await transaction.CommitAsync();
        logger.LogDebug("DM {RoomId} opened by {UserId}", roomId, callerId);
        return DmOutcome.Opened(ToRecord(room));
    }

    // False means the user already was a member, which every caller treats as an idempotent resync.
    public async Task<bool> JoinAsync(string roomId, long userId)
    {
        await using var db = await contexts.CreateDbContextAsync();
        if (await db.RoomMembers.AnyAsync(m => m.RoomId == roomId && m.UserId == userId))
        {
            return false;
        }

        // Joining marks the room's history read: a new member is not owed every message it missed.
        var cursor = await NewestMessageIdAsync(db, roomId);
        db.RoomMembers.Add(new RoomMember
        {
            RoomId = roomId,
            UserId = userId,
            JoinedAt = DateTime.UtcNow,
            LastReadMessageId = cursor,
        });

        try
        {
            await db.SaveChangesAsync();
        }
        catch (DbUpdateException ex) when (IsUniqueViolation(ex))
        {
            return false;
        }

        logger.LogDebug("User {UserId} joined room {RoomId}", userId, roomId);
        return true;
    }

    public async Task<bool> LeaveAsync(string roomId, long userId)
    {
        await using var db = await contexts.CreateDbContextAsync();
        var removed = await db.RoomMembers
            .Where(m => m.RoomId == roomId && m.UserId == userId)
            .ExecuteDeleteAsync();
        if (removed == 0)
        {
            return false;
        }

        logger.LogDebug("User {UserId} left room {RoomId}", userId, roomId);
        return true;
    }

    public async Task MarkReadAsync(string roomId, long userId, long messageId)
    {
        await using var db = await contexts.CreateDbContextAsync();
        var capped = Math.Min(messageId, await NewestMessageIdAsync(db, roomId));

        // The cursor only moves forward, and only for a row that exists: a stale MarkRead from a
        // reconnecting client, and one from a non-member, are both no-ops.
        await db.RoomMembers
            .Where(m => m.RoomId == roomId && m.UserId == userId && m.LastReadMessageId < capped)
            .ExecuteUpdateAsync(setters => setters.SetProperty(m => m.LastReadMessageId, capped));
    }

    public async Task<IReadOnlyList<RoomEntryRecord>> EntriesForAsync(long userId)
    {
        await using var db = await contexts.CreateDbContextAsync();

        // The reader's own memberships decide which DMs are visible at all.
        var joined = await db.RoomMembers
            .AsNoTracking()
            .Where(m => m.UserId == userId)
            .Select(m => m.RoomId)
            .ToListAsync();

        var rooms = await db.Rooms
            .AsNoTracking()
            .Where(r => r.Kind == RoomKind.Public || joined.Contains(r.Id))
            .OrderBy(r => r.Id)
            .ToListAsync();

        var roomIds = rooms.Select(r => r.Id).ToList();
        var members = await LoadMembersAsync(db, roomIds);

        var lastMessageIds = await db.Messages
            .AsNoTracking()
            .Where(m => roomIds.Contains(m.RoomId))
            .GroupBy(m => m.RoomId)
            .Select(g => new { RoomId = g.Key, LastId = g.Max(m => m.Id) })
            .ToDictionaryAsync(x => x.RoomId, x => x.LastId);

        // Unread is everything above the reader's cursor that is neither a tombstone nor the
        // reader's own. A null user id is the history that predates accounts: not the reader's,
        // so it counts.
        var unreadQuery = from message in db.Messages.AsNoTracking()
                          join membership in db.RoomMembers.AsNoTracking()
                              on message.RoomId equals membership.RoomId
                          where membership.UserId == userId
                              && message.Id > membership.LastReadMessageId
                              && message.DeletedAt == null
                              && message.UserId != userId
                          select message;

        var unread = await unreadQuery
            .GroupBy(m => m.RoomId)
            .Select(g => new { RoomId = g.Key, Count = g.LongCount() })
            .ToDictionaryAsync(x => x.RoomId, x => x.Count);

        var mentions = await unreadQuery
            .Where(m => m.MentionIds.Contains(userId))
            .GroupBy(m => m.RoomId)
            .Select(g => new { RoomId = g.Key, Count = g.LongCount() })
            .ToDictionaryAsync(x => x.RoomId, x => x.Count);

        // A public room the reader has not joined has no membership row, so no counters: 0 and 0.
        return rooms
            .Select(room => new RoomEntryRecord(
                ToRecord(room),
                members.GetValueOrDefault(room.Id, []),
                unread.GetValueOrDefault(room.Id),
                mentions.GetValueOrDefault(room.Id),
                lastMessageIds.GetValueOrDefault(room.Id)))
            .ToList();
    }

    // Takes the caller's context so the membership is written inside the registration's own
    // transaction: an account either exists as a member of general or does not exist at all.
    public async Task EnsureGeneralMembershipAsync(AppDbContext db, long userId)
    {
        if (await db.RoomMembers.AnyAsync(m => m.RoomId == ConnectionRegistry.GeneralRoomId && m.UserId == userId))
        {
            return;
        }

        db.RoomMembers.Add(new RoomMember
        {
            RoomId = ConnectionRegistry.GeneralRoomId,
            UserId = userId,
            JoinedAt = DateTime.UtcNow,
            LastReadMessageId = await NewestMessageIdAsync(db, ConnectionRegistry.GeneralRoomId),
        });
    }

    private static async Task<Dictionary<string, IReadOnlyList<long>>> LoadMembersAsync(AppDbContext db, IReadOnlyList<string> roomIds)
    {
        var rows = await db.RoomMembers
            .AsNoTracking()
            .Where(m => roomIds.Contains(m.RoomId))
            .OrderBy(m => m.UserId)
            .Select(m => new { m.RoomId, m.UserId })
            .ToListAsync();

        return rows
            .GroupBy(m => m.RoomId)
            .ToDictionary(g => g.Key, g => (IReadOnlyList<long>)g.Select(m => m.UserId).ToArray());
    }

    private static async Task<long> NewestMessageIdAsync(AppDbContext db, string roomId)
        => await db.Messages.Where(m => m.RoomId == roomId).MaxAsync(m => (long?)m.Id) ?? 0;

    private static bool IsUniqueViolation(DbUpdateException exception)
        => exception.InnerException is PostgresException { SqlState: PostgresErrorCodes.UniqueViolation };

    private static RoomRecord ToRecord(Room room) => new(room.Id, room.Kind, room.Name, room.CreatedBy);
}

using Microsoft.EntityFrameworkCore;

namespace Vorcall.Server.Data;

// Brings a fresh database up to the one shape the server can run on: a server row, the everyone
// role, and a general text channel to put it in. An upgraded database gets all of this from the
// migration instead, which is why the server row's existence is the whole test.
public static class Seed
{
    public const string EveryoneRoleName = "everyone";

    // VIEW_CHANNEL | SEND_MESSAGES | ATTACH_FILES | ADD_REACTIONS | CONNECT | SPEAK |
    // SHARE_SCREEN | CHANGE_NICKNAME | SOUNDPAD — the everyone defaults of PROTOCOL.md § Roles and
    // permissions, as Permissions.Perms.EveryoneDefault and the migration's insert spell them.
    public const long EveryonePermissions = 3206912;

    public const string GeneralCategoryName = "General";

    public const string GeneralTextChannelName = "general";

    public const string GeneralVoiceChannelName = "General";

    public static async Task EnsureSeededAsync(AppDbContext db, CancellationToken ct)
    {
        if (await db.Server.AnyAsync(ct))
        {
            return;
        }

        // All of it or none: a server row pointing at a general channel that was never inserted
        // would leave the server with no channel it may not hide.
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        // A fresh install has no account to own the server yet; the CLI's server set-owner is how
        // the first one is named.
        var ownerId = await db.Users
            .OrderBy(u => u.Id)
            .Select(u => (long?)u.Id)
            .FirstOrDefaultAsync(ct);

        if (!await db.Roles.AnyAsync(r => r.IsEveryone, ct))
        {
            db.Roles.Add(new Role
            {
                Name = EveryoneRoleName,
                Position = 0,
                Permissions = EveryonePermissions,
                IsEveryone = true,
            });
        }

        var category = new Category { Name = GeneralCategoryName, Position = 0 };
        db.Categories.Add(category);

        // Saved in three steps because the rows carry ids rather than navigations: the category's
        // id has to exist before the channels name it, and the channel's before the server row.
        await db.SaveChangesAsync(ct);

        var now = DateTime.UtcNow;
        var text = new Channel
        {
            Kind = ChannelKind.Text,
            Name = GeneralTextChannelName,
            CategoryId = category.Id,
            Position = 0,
            CreatedAt = now,
        };
        var voice = new Channel
        {
            Kind = ChannelKind.Voice,
            Name = GeneralVoiceChannelName,
            CategoryId = category.Id,
            Position = 1,
            CreatedAt = now,
        };

        db.Channels.Add(text);
        db.Channels.Add(voice);
        await db.SaveChangesAsync(ct);

        db.Server.Add(new Server { Id = Server.RowId, OwnerId = ownerId, GeneralChannelId = text.Id });
        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
    }

    // Takes the caller's context so the cursors are written inside the registration's own
    // transaction, like the general membership they replace: an account that exists has read every
    // text channel up to the message that was newest when it registered.
    public static async Task EnsureReadRowsAsync(AppDbContext db, long userId, CancellationToken ct)
    {
        var known = await db.ChannelReads
            .Where(r => r.UserId == userId)
            .Select(r => r.ChannelId)
            .ToListAsync(ct);

        var channelIds = await db.Channels
            .Where(c => c.Kind == ChannelKind.Text && !known.Contains(c.Id))
            .Select(c => c.Id)
            .ToListAsync(ct);

        if (channelIds.Count == 0)
        {
            return;
        }

        var newest = await db.Messages
            .Where(m => channelIds.Contains(m.ChannelId))
            .GroupBy(m => m.ChannelId)
            .Select(g => new { ChannelId = g.Key, LastId = g.Max(m => m.Id) })
            .ToListAsync(ct);

        var cursors = newest.ToDictionary(n => n.ChannelId, n => n.LastId);
        foreach (var channelId in channelIds)
        {
            db.ChannelReads.Add(new ChannelRead
            {
                ChannelId = channelId,
                UserId = userId,

                // 0 for a channel with no messages, which is what "nothing to read" is.
                LastReadMessageId = cursors.GetValueOrDefault(channelId),
            });
        }
    }
}

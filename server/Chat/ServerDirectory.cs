using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Attachments;
using Vorcall.Server.Data;

namespace Vorcall.Server.Chat;

// The one server row. OwnerId and GeneralChannelId are null only between a fresh install and its
// first account, which the seeder and the admin CLI repair.
public sealed record ServerRecord(string Name, string Description, long? IconImageId, long? OwnerId, long? GeneralChannelId);

// The server row and the seeding that creates it. Program.cs seeds before anything listens, so
// every read after boot finds the row.
public sealed class ServerDirectory(IDbContextFactory<AppDbContext> contexts, ILogger<ServerDirectory> logger)
{
    public async Task<ServerRecord> LoadAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Server.AsNoTracking().FirstOrDefaultAsync(s => s.Id == Data.Server.RowId, ct);
        return ToRecord(Require(row));
    }

    // iconImageId is sentinel-coded like the frame's: null keeps the current icon, 0 clears it.
    // ReplacedIconId is the image the write left unreferenced, for the caller to delete.
    public async Task<(ServerRecord Server, long? ReplacedIconId)> UpdateAsync(
        string name,
        string description,
        long? iconImageId,
        CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var row = Require(await db.Server.FirstOrDefaultAsync(s => s.Id == Data.Server.RowId, ct));
        var previousIcon = row.IconImageId;

        row.Name = name;
        row.Description = description;
        if (iconImageId is { } icon)
        {
            row.IconImageId = icon == 0 ? null : icon;
        }

        await db.SaveChangesAsync(ct);

        long? replaced = null;
        if (previousIcon is { } previous
            && previous != row.IconImageId
            && !await ImageStore.IsReferencedAsync(db, previous, ct))
        {
            replaced = previous;
        }

        await transaction.CommitAsync(ct);
        return (ToRecord(row), replaced);
    }

    // The owner is the one bypass of every permission check, so a banned account may not hold it.
    public async Task<bool> SetOwnerAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        if (!await db.Users.AnyAsync(u => u.Id == userId, ct) || await db.Bans.AnyAsync(b => b.UserId == userId, ct))
        {
            return false;
        }

        var row = Require(await db.Server.FirstOrDefaultAsync(s => s.Id == Data.Server.RowId, ct));
        row.OwnerId = userId;
        await db.SaveChangesAsync(ct);
        logger.LogInformation("Server owner set to {UserId}", userId);
        return true;
    }

    // A fresh install seeds its server row before any account exists, so it has no owner. The
    // first account to register takes it: on a self-hosted server that account is the person who
    // deployed it and made the invite, and without this they would have to reach for the admin
    // CLI before they could do anything. Only ever fires while OwnerId is null.
    public async Task<bool> ClaimOwnerAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = Require(await db.Server.FirstOrDefaultAsync(s => s.Id == Data.Server.RowId, ct));
        if (row.OwnerId is not null)
        {
            return false;
        }

        row.OwnerId = userId;
        await db.SaveChangesAsync(ct);
        logger.LogInformation("Server had no owner; the first account to register ({UserId}) took it", userId);
        return true;
    }

    public async Task EnsureSeededAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await Seed.EnsureSeededAsync(db, ct);
    }

    private static Data.Server Require(Data.Server? row)
        => row ?? throw new InvalidOperationException("The server row is missing: the database was never seeded.");

    private static ServerRecord ToRecord(Data.Server row)
        => new(row.Name, row.Description, row.IconImageId, row.OwnerId, row.GeneralChannelId);
}

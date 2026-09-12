using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Auth;

// UsedBy and UsedByUsername are null while the invite is unused; UsedByUsername is also null when
// the account that used it has since been deleted. CreatedBy is null for an invite the admin CLI
// minted, which has no account behind it.
public sealed record InviteRecord(
    long Id,
    DateTime CreatedAt,
    DateTime ExpiresAt,
    long? UsedBy,
    string? UsedByUsername,
    long? CreatedBy);

// Minting, listing and revoking invites, shared by the REST endpoints and the admin CLI. Only the
// hash of a code is ever stored, and no method here logs a code.
public sealed class InviteService(IDbContextFactory<AppDbContext> contexts, ILogger<InviteService> logger)
{
    public const int MinDays = 1;

    public const int MaxDays = 365;

    public static bool IsValidDays(int days) => days is >= MinDays and <= MaxDays;

    // The returned code is plaintext and exists nowhere else: the caller shows it once. Days are
    // clamped to the allowed range, which the caller has already refused outside of.
    public async Task<(long Id, string Code, DateTime ExpiresAt)> CreateAsync(
        int days,
        long? createdBy,
        DateTime now,
        CancellationToken ct)
    {
        var code = Credentials.NewInviteCode();
        var expiresAt = now.AddDays(Math.Clamp(days, MinDays, MaxDays));

        await using var db = await contexts.CreateDbContextAsync(ct);
        var invite = new Data.Invite
        {
            CodeHash = Credentials.Sha256Hex(code),
            CreatedAt = now,
            ExpiresAt = expiresAt,
            CreatedBy = createdBy,
        };

        db.Invites.Add(invite);
        await db.SaveChangesAsync(ct);
        logger.LogInformation("Invite {InviteId} created by {UserId}", invite.Id, createdBy);
        return (invite.Id, code, expiresAt);
    }

    public async Task<List<InviteRecord>> ListAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var rows = await db.Invites
            .AsNoTracking()
            .OrderBy(i => i.Id)
            .Select(i => new
            {
                i.Id,
                i.CreatedAt,
                i.ExpiresAt,
                i.UsedByUserId,
                i.CreatedBy,
                Username = db.Users.Where(u => u.Id == i.UsedByUserId).Select(u => u.Username).FirstOrDefault(),
            })
            .ToListAsync(ct);

        return rows
            .Select(i => new InviteRecord(i.Id, i.CreatedAt, i.ExpiresAt, i.UsedByUserId, i.Username, i.CreatedBy))
            .ToList();
    }

    // Only an unused invite is revocable: a used row is the audit trail of the account it let in.
    // False therefore means "unknown or already used"; ExistsAsync tells the two apart.
    public async Task<bool> RevokeAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var deleted = await db.Invites.Where(i => i.Id == id && i.UsedAt == null).ExecuteDeleteAsync(ct);
        if (deleted == 0)
        {
            return false;
        }

        logger.LogInformation("Invite {InviteId} revoked", id);
        return true;
    }

    public async Task<bool> ExistsAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Invites.AnyAsync(i => i.Id == id, ct);
    }
}

using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Auth;

// Unknown is an id that names no invite at all. Used is the one state revoking refuses: that row is
// the audit trail of the account it admitted. Revoking an already-revoked invite is AlreadyRevoked,
// a success — the row is in the state the caller asked for and nothing was written twice.
public enum RevokeStatus
{
    Unknown,
    Used,
    AlreadyRevoked,
    Revoked,
}

// At is the stamp the status stands on, always set for Used (when the invite was claimed) and for
// AlreadyRevoked (when it had been revoked). Null for Unknown, and for Revoked, which happened at
// the caller's own now.
public readonly record struct RevokeOutcome(RevokeStatus Status, DateTime? At)
{
    public static RevokeOutcome Unknown { get; } = new(RevokeStatus.Unknown, null);

    public static RevokeOutcome Revoked { get; } = new(RevokeStatus.Revoked, null);

    public static RevokeOutcome Used(DateTime usedAt) => new(RevokeStatus.Used, usedAt);

    public static RevokeOutcome AlreadyRevoked(DateTime revokedAt) => new(RevokeStatus.AlreadyRevoked, revokedAt);
}

// UsedBy and UsedByUsername are null while the invite is unused; UsedByUsername is also null when
// the account that used it has since been deleted. CreatedBy is null for an invite the admin CLI
// minted, which has no account behind it. RevokedAt is null unless the invite was revoked.
public sealed record InviteRecord(
    long Id,
    DateTime CreatedAt,
    DateTime ExpiresAt,
    long? UsedBy,
    string? UsedByUsername,
    long? CreatedBy,
    DateTime? RevokedAt);

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
                i.RevokedAt,
                Username = db.Users.Where(u => u.Id == i.UsedByUserId).Select(u => u.Username).FirstOrDefault(),
            })
            .ToListAsync(ct);

        return rows
            .Select(i => new InviteRecord(i.Id, i.CreatedAt, i.ExpiresAt, i.UsedByUserId, i.Username, i.CreatedBy, i.RevokedAt))
            .ToList();
    }

    // Revoking marks the row; it never deletes one. The list therefore keeps saying that an invite
    // existed and ended without admitting anybody, and the code it stands for can never register
    // again (AccountService.RegisterAsync refuses a revoked row like an unknown one).
    public async Task<RevokeOutcome> RevokeAsync(long id, DateTime now, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var row = await db.Invites
            .AsNoTracking()
            .Where(i => i.Id == id)
            .Select(i => new { i.UsedAt, i.RevokedAt })
            .FirstOrDefaultAsync(ct);
        if (row is null)
        {
            return RevokeOutcome.Unknown;
        }

        if (row.UsedAt is { } usedAt)
        {
            return RevokeOutcome.Used(usedAt);
        }

        if (row.RevokedAt is { } revokedAt)
        {
            return RevokeOutcome.AlreadyRevoked(revokedAt);
        }

        // Conditional rather than a write on the row just read, like the claim in
        // AccountService.RegisterAsync: a registration may have consumed the invite in between, and
        // that claim wins. The Used stamp reported in that case is this call's now rather than the
        // registration's, which it is within the width of that race of.
        var marked = await db.Invites
            .Where(i => i.Id == id && i.UsedAt == null && i.RevokedAt == null)
            .ExecuteUpdateAsync(setters => setters.SetProperty(i => i.RevokedAt, (DateTime?)now), ct);
        if (marked == 0)
        {
            return RevokeOutcome.Used(now);
        }

        logger.LogInformation("Invite {InviteId} revoked", id);
        return RevokeOutcome.Revoked;
    }
}

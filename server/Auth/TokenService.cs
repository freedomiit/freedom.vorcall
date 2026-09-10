using System.Globalization;
using Microsoft.EntityFrameworkCore;
using Microsoft.IdentityModel.JsonWebTokens;
using Microsoft.IdentityModel.Tokens;
using Vorcall.Server.Data;

namespace Vorcall.Server.Auth;

public enum RefreshStatus
{
    Invalid,
    Reused,
    Rotated,
}

// User and RefreshToken are set only when Status is Rotated.
public readonly record struct RefreshOutcome(RefreshStatus Status, User? User, string? RefreshToken)
{
    public static RefreshOutcome Invalid { get; } = new(RefreshStatus.Invalid, null, null);

    public static RefreshOutcome Reused { get; } = new(RefreshStatus.Reused, null, null);

    public static RefreshOutcome Rotated(User user, string refreshToken) => new(RefreshStatus.Rotated, user, refreshToken);
}

// Mints access tokens and owns the refresh-token lifecycle. No method here ever logs a token or
// a token hash.
public sealed class TokenService(
    IDbContextFactory<AppDbContext> contextFactory,
    JwtOptions options,
    ILogger<TokenService> logger)
{
    // The caller's clock fills every time claim, so one request stamps one instant.
    private static readonly JsonWebTokenHandler Handler = new() { SetDefaultTimesOnTokenCreation = false };

    public string IssueAccessToken(User user, DateTime now) => Handler.CreateToken(new SecurityTokenDescriptor
    {
        Issuer = JwtOptions.Issuer,
        Audience = JwtOptions.Audience,
        IssuedAt = now,
        NotBefore = now,
        Expires = now + JwtOptions.AccessTokenLifetime,
        Claims = new Dictionary<string, object>
        {
            ["sub"] = user.Id.ToString(CultureInfo.InvariantCulture),
            ["name"] = user.Username,
            ["jti"] = Guid.NewGuid().ToString("N"),
        },
        SigningCredentials = new SigningCredentials(options.Key, SecurityAlgorithms.HmacSha256),
    });

    // Adds the row but does not save it: the caller decides which transaction it belongs to.
    public string IssueRefreshToken(AppDbContext db, long userId, Guid familyId, DateTime now)
    {
        var plaintext = Credentials.NewRefreshToken();
        db.RefreshTokens.Add(new RefreshToken
        {
            UserId = userId,
            TokenHash = Credentials.Sha256Hex(plaintext),
            FamilyId = familyId,
            CreatedAt = now,
            LastUsedAt = now,
            ExpiresAt = now + JwtOptions.RefreshTokenLifetime,
        });

        return plaintext;
    }

    public async Task<RefreshOutcome> RefreshAsync(string plaintext, DateTime now)
    {
        var hash = Credentials.Sha256Hex(plaintext);
        await using var db = await contextFactory.CreateDbContextAsync();

        // One transaction around the lookup and a conditional claim on the presented row. Two
        // READ COMMITTED refreshes of the same token both read RevokedAt as null, so the read
        // decides nothing: only the one whose UPDATE matches may rotate, and the loser is a
        // replay like any other.
        await using var transaction = await db.Database.BeginTransactionAsync();

        var presented = await db.RefreshTokens.Include(t => t.User).FirstOrDefaultAsync(t => t.TokenHash == hash);
        if (presented is null || presented.ExpiresAt <= now)
        {
            return RefreshOutcome.Invalid;
        }

        var claimed = 0;
        if (presented.RevokedAt is null)
        {
            claimed = await db.RefreshTokens
                .Where(t => t.Id == presented.Id && t.RevokedAt == null)
                .ExecuteUpdateAsync(setters => setters
                    .SetProperty(t => t.RevokedAt, (DateTime?)now)
                    .SetProperty(t => t.LastUsedAt, now));
        }

        // Already revoked when read, or claimed by someone else in between: either way the
        // token was presented twice and the whole family goes down.
        if (claimed == 0)
        {
            await db.RefreshTokens
                .Where(t => t.FamilyId == presented.FamilyId && t.RevokedAt == null)
                .ExecuteUpdateAsync(setters => setters.SetProperty(t => t.RevokedAt, (DateTime?)now));
            await transaction.CommitAsync();
            logger.LogWarning("Refresh token reuse detected for user {UserId}, family revoked", presented.UserId);
            return RefreshOutcome.Reused;
        }

        // The claim wrote straight to the row, so the tracked copy is stale: bring it in step
        // instead of saving the old values back over what the UPDATE just wrote.
        presented.LastUsedAt = now;
        presented.RevokedAt = now;

        var successor = Credentials.NewRefreshToken();
        var row = new RefreshToken
        {
            UserId = presented.UserId,
            TokenHash = Credentials.Sha256Hex(successor),
            FamilyId = presented.FamilyId,
            CreatedAt = now,
            LastUsedAt = now,
            ExpiresAt = now + JwtOptions.RefreshTokenLifetime,
        };

        db.RefreshTokens.Add(row);

        // The successor needs its id before the presented row can point at it.
        await db.SaveChangesAsync();

        await db.RefreshTokens
            .Where(t => t.Id == presented.Id)
            .ExecuteUpdateAsync(setters => setters.SetProperty(t => t.ReplacedById, (long?)row.Id));
        await transaction.CommitAsync();
        return RefreshOutcome.Rotated(presented.User, successor);
    }

    public async Task RevokeAsync(string plaintext, DateTime now)
    {
        var hash = Credentials.Sha256Hex(plaintext);
        await using var db = await contextFactory.CreateDbContextAsync();

        // Logout ends the whole chain rather than the row presented: a client that logs out
        // with a token its own refresh loop has already rotated must not leave the successor
        // live. An unknown hash is not an error, because logout is idempotent.
        var familyId = await db.RefreshTokens
            .Where(t => t.TokenHash == hash)
            .Select(t => (Guid?)t.FamilyId)
            .FirstOrDefaultAsync();
        if (familyId is not { } family)
        {
            return;
        }

        await db.RefreshTokens
            .Where(t => t.FamilyId == family && t.RevokedAt == null)
            .ExecuteUpdateAsync(setters => setters.SetProperty(t => t.RevokedAt, (DateTime?)now));
    }

    public Task RevokeAllAsync(AppDbContext db, long userId, long? keepTokenId, DateTime now)
        => db.RefreshTokens
            .Where(t => t.UserId == userId && t.RevokedAt == null && (keepTokenId == null || t.Id != keepTokenId))
            .ExecuteUpdateAsync(setters => setters.SetProperty(t => t.RevokedAt, (DateTime?)now));
}

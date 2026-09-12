using System.Security.Cryptography;
using Microsoft.AspNetCore.Identity;
using Microsoft.EntityFrameworkCore;
using Npgsql;
using Vorcall.Server.Chat;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Auth;

public enum RegisterStatus
{
    InvalidUsername,
    InvalidPassword,
    InvalidInvite,
    InviteUnusable,
    UsernameTaken,
    Registered,
}

public enum LoginStatus
{
    Locked,
    Invalid,
    Disabled,
    LoggedIn,
}

public enum ChangePasswordOutcome
{
    WrongPassword,
    InvalidPassword,
    Changed,
}

// Tokens is set only when Status is Registered.
public readonly record struct RegisterOutcome(RegisterStatus Status, TokenResponse? Tokens)
{
    public static RegisterOutcome Rejected(RegisterStatus status) => new(status, null);

    public static RegisterOutcome Registered(TokenResponse tokens) => new(RegisterStatus.Registered, tokens);
}

// Tokens is set only when Status is LoggedIn, RetryAfterSeconds only when it is Locked.
public readonly record struct LoginOutcome(LoginStatus Status, TokenResponse? Tokens, int RetryAfterSeconds)
{
    public static LoginOutcome Invalid { get; } = new(LoginStatus.Invalid, null, 0);

    public static LoginOutcome Disabled { get; } = new(LoginStatus.Disabled, null, 0);

    public static LoginOutcome LockedFor(int retryAfterSeconds) => new(LoginStatus.Locked, null, retryAfterSeconds);

    public static LoginOutcome LoggedIn(TokenResponse tokens) => new(LoginStatus.LoggedIn, tokens, 0);
}

public sealed class AccountService(
    IDbContextFactory<AppDbContext> contextFactory,
    IPasswordHasher<User> hasher,
    TokenService tokens,
    LoginThrottle throttle,
    RoomDirectory rooms,
    ILogger<AccountService> logger)
{
    private static readonly User HashingSubject = new();

    // Verified whenever the username does not exist, so a miss costs the same as a wrong
    // password and the answer time does not leak which accounts are real.
    private readonly string _dummyHash = hasher.HashPassword(
        HashingSubject,
        Convert.ToBase64String(RandomNumberGenerator.GetBytes(32)));

    public async Task<RegisterOutcome> RegisterAsync(string? username, string? password, string? inviteCode, DateTime now)
    {
        if (!Credentials.TryNormalizeUsername(username, out var name, out var normalized))
        {
            return RegisterOutcome.Rejected(RegisterStatus.InvalidUsername);
        }

        if (!Credentials.IsValidPassword(password))
        {
            return RegisterOutcome.Rejected(RegisterStatus.InvalidPassword);
        }

        if (!Credentials.TryNormalizeInviteCode(inviteCode, out var code))
        {
            return RegisterOutcome.Rejected(RegisterStatus.InvalidInvite);
        }

        var codeHash = Credentials.Sha256Hex(code);
        await using var db = await contextFactory.CreateDbContextAsync();

        // Everything below is one transaction so a taken username leaves the invite unused:
        // the rollback undoes the claim as well as the insert.
        await using var transaction = await db.Database.BeginTransactionAsync();

        var invite = await db.Invites.AsNoTracking().FirstOrDefaultAsync(i => i.CodeHash == codeHash);
        if (invite is null || invite.UsedAt is not null || invite.RevokedAt is not null || invite.ExpiresAt <= now)
        {
            return RegisterOutcome.Rejected(RegisterStatus.InviteUnusable);
        }

        if (await db.Users.AnyAsync(u => u.UsernameNormalized == normalized))
        {
            return RegisterOutcome.Rejected(RegisterStatus.UsernameTaken);
        }

        var user = new User { Username = name, UsernameNormalized = normalized, CreatedAt = now };
        user.PasswordHash = hasher.HashPassword(user, password);
        db.Users.Add(user);
        try
        {
            await db.SaveChangesAsync();
        }
        catch (DbUpdateException ex) when (ex.InnerException is PostgresException { SqlState: PostgresErrorCodes.UniqueViolation })
        {
            // Two registrations for the same name raced past the existence check.
            return RegisterOutcome.Rejected(RegisterStatus.UsernameTaken);
        }

        // Inside the same transaction as the user row: an account that exists is always a member
        // of general, which is the one room nobody may leave.
        await rooms.EnsureGeneralMembershipAsync(db, user.Id);

        // Conditional claim rather than a write on the row we read: another registration may
        // have consumed the invite between the two statements.
        var claimed = await db.Invites
            .Where(i => i.Id == invite.Id && i.UsedAt == null && i.RevokedAt == null)
            .ExecuteUpdateAsync(setters => setters
                .SetProperty(i => i.UsedAt, (DateTime?)now)
                .SetProperty(i => i.UsedByUserId, (long?)user.Id));
        if (claimed == 0)
        {
            return RegisterOutcome.Rejected(RegisterStatus.InviteUnusable);
        }

        var refreshToken = tokens.IssueRefreshToken(db, user.Id, Guid.NewGuid(), now);
        await db.SaveChangesAsync();
        await transaction.CommitAsync();

        logger.LogInformation("User {UserId} registered", user.Id);
        return RegisterOutcome.Registered(BuildTokens(user, refreshToken, now));
    }

    public async Task<LoginOutcome> LoginAsync(string? username, string? password, DateTime now)
    {
        var presented = password ?? string.Empty;
        if (!Credentials.TryNormalizeUsername(username, out _, out var normalized))
        {
            // A malformed username costs one verification like everything else, and leaves no
            // throttle entry for an attacker to fill memory with.
            hasher.VerifyHashedPassword(HashingSubject, _dummyHash, presented);
            return LoginOutcome.Invalid;
        }

        if (throttle.IsLocked(normalized, now, out var retryAfterSeconds))
        {
            return LoginOutcome.LockedFor(retryAfterSeconds);
        }

        await using var db = await contextFactory.CreateDbContextAsync();
        var user = await db.Users.FirstOrDefaultAsync(u => u.UsernameNormalized == normalized);
        if (user is null)
        {
            hasher.VerifyHashedPassword(HashingSubject, _dummyHash, presented);
            throttle.RecordFailure(normalized, now);
            return LoginOutcome.Invalid;
        }

        var verification = hasher.VerifyHashedPassword(user, user.PasswordHash, presented);
        if (verification == PasswordVerificationResult.Failed)
        {
            throttle.RecordFailure(normalized, now);
            return LoginOutcome.Invalid;
        }

        // After the verification, not before: a banned account has to cost exactly what a live
        // one does, or the answer time says which names are banned.
        if (user.DisabledAt is not null)
        {
            return LoginOutcome.Disabled;
        }

        if (verification == PasswordVerificationResult.SuccessRehashNeeded)
        {
            user.PasswordHash = hasher.HashPassword(user, presented);
        }

        throttle.RecordSuccess(normalized);
        var refreshToken = tokens.IssueRefreshToken(db, user.Id, Guid.NewGuid(), now);
        await db.SaveChangesAsync();
        return LoginOutcome.LoggedIn(BuildTokens(user, refreshToken, now));
    }

    // Null covers every failure: an unknown, expired, revoked or replayed token all answer 401.
    public async Task<TokenResponse?> RefreshAsync(string? plaintext, DateTime now)
    {
        if (string.IsNullOrEmpty(plaintext))
        {
            return null;
        }

        var outcome = await tokens.RefreshAsync(plaintext, now);
        return outcome is { Status: RefreshStatus.Rotated, User: { } user, RefreshToken: { } refreshToken }
            ? BuildTokens(user, refreshToken, now)
            : null;
    }

    public Task LogoutAsync(string? plaintext, DateTime now)
        => string.IsNullOrEmpty(plaintext) ? Task.CompletedTask : tokens.RevokeAsync(plaintext, now);

    public async Task<ChangePasswordOutcome> ChangePasswordAsync(
        long userId,
        string? current,
        string? next,
        string? refreshPlaintext,
        DateTime now)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var user = await db.Users.FirstOrDefaultAsync(u => u.Id == userId);

        // A bearer for an account that no longer exists answers like a wrong password: 401.
        if (user is null || hasher.VerifyHashedPassword(user, user.PasswordHash, current ?? string.Empty) == PasswordVerificationResult.Failed)
        {
            return ChangePasswordOutcome.WrongPassword;
        }

        if (!Credentials.IsValidPassword(next))
        {
            return ChangePasswordOutcome.InvalidPassword;
        }

        user.PasswordHash = hasher.HashPassword(user, next);
        await db.SaveChangesAsync();

        long? keepTokenId = null;
        if (!string.IsNullOrEmpty(refreshPlaintext))
        {
            // Only the caller's own session survives, so a stolen token from another device dies
            // with the password it was taken under. The caller may present the predecessor of the
            // token it actually holds, because its refresh loop can rotate while this request is
            // in flight, so the family's live head is kept rather than the row presented.
            var hash = Credentials.Sha256Hex(refreshPlaintext);
            var familyId = await db.RefreshTokens
                .Where(t => t.TokenHash == hash && t.UserId == userId)
                .Select(t => (Guid?)t.FamilyId)
                .FirstOrDefaultAsync();
            if (familyId is { } family)
            {
                keepTokenId = await db.RefreshTokens
                    .Where(t => t.FamilyId == family && t.RevokedAt == null)
                    .Select(t => (long?)t.Id)
                    .FirstOrDefaultAsync();
            }
        }

        await tokens.RevokeAllAsync(db, userId, keepTokenId, now);
        logger.LogInformation("User {UserId} changed password; other sessions revoked", userId);
        return ChangePasswordOutcome.Changed;
    }

    public async Task<UserList> ListUsersAsync()
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var users = await db.Users
            .AsNoTracking()
            .OrderBy(u => u.UsernameNormalized)
            .Select(u => new { u.Id, u.Username })
            .ToListAsync();

        var list = new UserList();
        list.Users.AddRange(users.Select(u => new Member { UserId = u.Id, Username = u.Username }));
        return list;
    }

    private TokenResponse BuildTokens(User user, string refreshToken, DateTime now) => new()
    {
        AccessToken = tokens.IssueAccessToken(user, now),
        ExpiresIn = (uint)JwtOptions.AccessTokenLifetime.TotalSeconds,
        RefreshToken = refreshToken,
        UserId = user.Id,
        Username = user.Username,
    };
}

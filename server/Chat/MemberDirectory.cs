using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Data;

namespace Vorcall.Server.Chat;

// Everything shown about one account. Roles live in RoleDirectory and presence only in memory.
public sealed record MemberRecord(
    long UserId,
    string Username,
    string? Nickname,
    long? AvatarImageId,
    long? BannerImageId,
    string Description,
    int? AccentColor,
    bool ServerMuted,
    bool ServerDeafened);

// Username is empty only when the account behind the ban was deleted.
public sealed record BanRecord(long UserId, string Username, string Reason, long? BannedBy, DateTime BannedAt);

// Tombstoned is the (channel, message) pairs the ban turned into tombstones, for the caller's
// MessageDeleted broadcasts; OverrideChannels the channels whose override for the account the ban
// dropped, for its ChannelUpserted broadcasts; AttachmentFiles the paths whose rows went with the
// tombstones.
public sealed record BanOutcome(
    bool Found,
    bool AlreadyBanned,
    List<(long ChannelId, long MessageId)> Tombstoned,
    List<long> OverrideChannels,
    List<string> AttachmentFiles)
{
    public static BanOutcome NotFound() => new(false, false, [], [], []);

    public static BanOutcome Duplicate() => new(true, true, [], [], []);

    public static BanOutcome Banned(
        List<(long ChannelId, long MessageId)> tombstoned,
        List<long> overrideChannels,
        List<string> attachmentFiles)
        => new(true, false, tombstoned, overrideChannels, attachmentFiles);
}

// Profiles, nicknames, the persisted voice moderation flags, bans and kicks. Hierarchy and
// permission checks are the caller's; what lands here has already been allowed.
public sealed class MemberDirectory(
    IDbContextFactory<AppDbContext> contexts,
    TokenService tokens,
    AttachmentStore attachments,
    ILogger<MemberDirectory> logger)
{
    // Every account that is not banned: a banned one is no longer a member of the server.
    public async Task<List<MemberRecord>> LoadAllAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var users = await db.Users
            .AsNoTracking()
            .Where(u => !db.Bans.Any(b => b.UserId == u.Id))
            .OrderBy(u => u.UsernameNormalized)
            .ToListAsync(ct);
        return users.Select(ToRecord).ToList();
    }

    // Bans are not filtered here: an unban has to read the profile it is about to broadcast.
    public async Task<MemberRecord?> GetAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var user = await db.Users.AsNoTracking().FirstOrDefaultAsync(u => u.Id == userId, ct);
        return user is null ? null : ToRecord(user);
    }

    // PROTOCOL.md § Moderation: a banned account is not a member, so the read that decides whether
    // to announce one answers the ban in the same breath rather than as a second question.
    public async Task<MemberRecord?> GetIfNotBannedAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var user = await db.Users
            .AsNoTracking()
            .FirstOrDefaultAsync(u => u.Id == userId && !db.Bans.Any(b => b.UserId == u.Id), ct);
        return user is null ? null : ToRecord(user);
    }

    // Null or empty clears the nickname, which is what the wire's empty string means.
    public async Task<bool> SetNicknameAsync(long userId, string? nickname, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var user = await db.Users.FirstOrDefaultAsync(u => u.Id == userId, ct);
        if (user is null)
        {
            return false;
        }

        user.Nickname = string.IsNullOrEmpty(nickname) ? null : nickname;
        await db.SaveChangesAsync(ct);
        return true;
    }

    // Null means "keep this field" and 0 means "clear it" for the colour and the two image ids,
    // which is how the frame's sentinels arrive. ReplacedImageIds are the images the write left
    // unreferenced, for the caller to delete; an id something else still points at is not one of
    // them, and the image sweeper is the backstop either way.
    public async Task<(bool Ok, List<long> ReplacedImageIds)> UpdateProfileAsync(
        long userId,
        string? description,
        int? accentColor,
        long? avatarImageId,
        long? bannerImageId,
        CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var user = await db.Users.FirstOrDefaultAsync(u => u.Id == userId, ct);
        if (user is null)
        {
            return (false, []);
        }

        var previousAvatar = user.AvatarImageId;
        var previousBanner = user.BannerImageId;

        if (description is not null)
        {
            user.Description = description;
        }

        if (accentColor is { } accent)
        {
            user.AccentColor = accent == 0 ? null : accent;
        }

        if (avatarImageId is { } avatar)
        {
            user.AvatarImageId = avatar == 0 ? null : avatar;
        }

        if (bannerImageId is { } banner)
        {
            user.BannerImageId = banner == 0 ? null : banner;
        }

        await db.SaveChangesAsync(ct);

        var replaced = new List<long>();
        await CollectReplacedAsync(db, previousAvatar, user.AvatarImageId, replaced, ct);
        await CollectReplacedAsync(db, previousBanner, user.BannerImageId, replaced, ct);
        await transaction.CommitAsync(ct);
        return (true, replaced);
    }

    // Each flag is written only when the caller asked for it, like the frame's set_* companions.
    public async Task<bool> SetVoiceFlagsAsync(long userId, bool? muted, bool? deafened, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var user = await db.Users.FirstOrDefaultAsync(u => u.Id == userId, ct);
        if (user is null)
        {
            return false;
        }

        if (muted is { } isMuted)
        {
            user.ServerMuted = isMuted;
        }

        if (deafened is { } isDeafened)
        {
            user.ServerDeafened = isDeafened;
        }

        await db.SaveChangesAsync(ct);
        return true;
    }

    public async Task<bool> IsBannedAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Bans.AnyAsync(b => b.UserId == userId, ct);
    }

    public async Task<List<BanRecord>> ListBansAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var rows = await db.Bans
            .AsNoTracking()
            .OrderBy(b => b.BannedAt)
            .ThenBy(b => b.UserId)
            .Select(b => new
            {
                b.UserId,
                b.Reason,
                b.BannedBy,
                b.BannedAt,
                Username = db.Users.Where(u => u.Id == b.UserId).Select(u => u.Username).FirstOrDefault(),
            })
            .ToListAsync(ct);

        return rows
            .Select(b => new BanRecord(b.UserId, b.Username ?? string.Empty, b.Reason, b.BannedBy, b.BannedAt))
            .ToList();
    }

    // The ban row, the tombstones and the token revocation are one transaction: a banned account
    // whose messages survived, or whose sessions stayed live, is the ban not having happened.
    public async Task<BanOutcome> BanAsync(long userId, long actorId, string reason, DateTime now, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        if (!await db.Users.AnyAsync(u => u.Id == userId, ct))
        {
            return BanOutcome.NotFound();
        }

        if (await db.Bans.AnyAsync(b => b.UserId == userId, ct))
        {
            return BanOutcome.Duplicate();
        }

        db.Bans.Add(new Data.Ban
        {
            UserId = userId,
            BannedBy = actorId,
            Reason = reason,
            BannedAt = now,
        });
        await db.SaveChangesAsync(ct);

        var tombstoned = await db.Messages
            .AsNoTracking()
            .Where(m => m.UserId == userId && m.DeletedAt == null)
            .OrderBy(m => m.Id)
            .Select(m => new { m.ChannelId, m.Id })
            .ToListAsync(ct);

        var files = new List<string>();
        if (tombstoned.Count > 0)
        {
            var messageIds = tombstoned.Select(m => m.Id).ToList();
            var linked = messageIds.Select(id => (long?)id).ToList();

            var attachmentRows = await db.Attachments
                .AsNoTracking()
                .Where(a => linked.Contains(a.MessageId))
                .OrderBy(a => a.Id)
                .Select(a => new { a.Id, a.ContentType })
                .ToListAsync(ct);
            files = attachments.PathsFor(attachmentRows.Select(row => (row.Id, row.ContentType)));

            // The rows survive as tombstones: their ids, authors and channels are what replies to
            // them still resolve against.
            await db.Messages
                .Where(m => messageIds.Contains(m.Id))
                .ExecuteUpdateAsync(
                    setters => setters
                        .SetProperty(m => m.Text, string.Empty)
                        .SetProperty(m => m.MentionIds, Array.Empty<long>())
                        .SetProperty(m => m.MentionEveryone, false)
                        .SetProperty(m => m.MentionHere, false)
                        .SetProperty(m => m.DeletedAt, (DateTime?)now),
                    ct);
            await db.Reactions.Where(r => messageIds.Contains(r.MessageId)).ExecuteDeleteAsync(ct);
            await db.Attachments.Where(a => linked.Contains(a.MessageId)).ExecuteDeleteAsync(ct);
            await db.StreamedFiles.Where(s => linked.Contains(s.MessageId)).ExecuteDeleteAsync(ct);
        }

        // PROTOCOL.md § Moderation puts the per-member overrides in this transaction too: an
        // override naming an account that is no longer a member would resolve for it again on an
        // unban.
        var overrideChannels = await db.ChannelOverrides
            .AsNoTracking()
            .Where(o => o.TargetKind == OverrideTarget.User && o.TargetId == userId)
            .Select(o => o.ChannelId)
            .Distinct()
            .ToListAsync(ct);
        if (overrideChannels.Count > 0)
        {
            await db.ChannelOverrides
                .Where(o => o.TargetKind == OverrideTarget.User && o.TargetId == userId)
                .ExecuteDeleteAsync(ct);
        }

        await tokens.RevokeAllAsync(db, userId, keepTokenId: null, now);
        await transaction.CommitAsync(ct);

        logger.LogInformation(
            "User {UserId} banned by {ActorId}; {MessageCount} messages tombstoned, {OverrideCount} channel overrides dropped",
            userId,
            actorId,
            tombstoned.Count,
            overrideChannels.Count);
        return BanOutcome.Banned(tombstoned.Select(m => (m.ChannelId, m.Id)).ToList(), overrideChannels, files);
    }

    // Messages stay tombstones: only the gate on signing in is lifted.
    public async Task<bool> UnbanAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        var removed = await db.Bans.Where(b => b.UserId == userId).ExecuteDeleteAsync(ct);
        if (removed == 0)
        {
            return false;
        }

        logger.LogInformation("User {UserId} unbanned", userId);
        return true;
    }

    // A kick takes the sessions and nothing else: the account may sign in again at once.
    public async Task KickAsync(long userId, DateTime now, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await tokens.RevokeAllAsync(db, userId, keepTokenId: null, now);
        logger.LogInformation("User {UserId} kicked; sessions revoked", userId);
    }

    private static async Task CollectReplacedAsync(
        AppDbContext db,
        long? previous,
        long? current,
        List<long> replaced,
        CancellationToken ct)
    {
        if (previous is not { } id || previous == current || replaced.Contains(id))
        {
            return;
        }

        if (!await ImageStore.IsReferencedAsync(db, id, ct))
        {
            replaced.Add(id);
        }
    }

    private static MemberRecord ToRecord(User user) => new(
        user.Id,
        user.Username,
        user.Nickname,
        user.AvatarImageId,
        user.BannerImageId,
        user.Description,
        user.AccentColor,
        user.ServerMuted,
        user.ServerDeafened);
}

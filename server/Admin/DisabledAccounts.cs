using System.Collections.Concurrent;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Admin;

// Every bearer-authenticated request asks whether the account behind the token is disabled, which
// is one query per request unless the answer is remembered. The cache is short-lived so a lock
// lands within half a minute even if nothing invalidates it; the CLI's refresh-account call
// makes it immediate.
public sealed class DisabledAccounts(IDbContextFactory<AppDbContext> contexts)
{
    public static readonly TimeSpan Ttl = TimeSpan.FromSeconds(30);

    private readonly ConcurrentDictionary<long, Entry> _entries = new();

    public async Task<bool> IsDisabledAsync(long userId, CancellationToken ct = default)
    {
        var now = DateTime.UtcNow;
        if (_entries.TryGetValue(userId, out var cached) && now - cached.CheckedAt < Ttl)
        {
            return cached.Disabled;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        var disabled = await db.Users.AnyAsync(u => u.Id == userId && u.DisabledAt != null, ct);

        // Two requests for the same account may both miss and both write; the value is the same
        // either way, so the last one in wins with nothing to reconcile.
        _entries[userId] = new Entry(disabled, now);
        return disabled;
    }

    public void Invalidate(long userId) => _entries.TryRemove(userId, out _);

    private readonly record struct Entry(bool Disabled, DateTime CheckedAt);
}

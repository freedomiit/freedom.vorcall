using System.Collections.Concurrent;

namespace Vorcall.Server.Auth;

// Per-username lockout on top of the per-IP rate limiter. In memory on purpose: the deployment
// is a single process, and a lockout that resets on restart is worth more than a database write
// per failed login.
public sealed class LoginThrottle
{
    private const int FailuresBeforeLock = 5;
    private const int FirstLockSeconds = 60;
    private const int MaxLockSeconds = 900;

    private static readonly TimeSpan EntryTtl = TimeSpan.FromHours(1);

    private readonly ConcurrentDictionary<string, Entry> _entries = new(StringComparer.Ordinal);

    public bool IsLocked(string key, DateTime now, out int retryAfterSeconds)
    {
        Prune(now);
        retryAfterSeconds = 0;
        if (!_entries.TryGetValue(key, out var entry) || entry.LockedUntil <= now)
        {
            return false;
        }

        retryAfterSeconds = Math.Max(1, (int)Math.Ceiling((entry.LockedUntil - now).TotalSeconds));
        return true;
    }

    public void RecordFailure(string key, DateTime now)
    {
        Prune(now);

        // Entries are immutable, so the update delegate is safe to run more than once under
        // contention; only the winning value is stored.
        _entries.AddOrUpdate(key, _ => Advance(Entry.Fresh, now), (_, existing) => Advance(existing, now));
    }

    public void RecordSuccess(string key) => _entries.TryRemove(key, out _);

    private static Entry Advance(Entry entry, DateTime now)
    {
        var failures = entry.Failures + 1;
        if (failures < FailuresBeforeLock)
        {
            return entry with { Failures = failures, Touched = now };
        }

        // Each lock doubles the previous one, and the counter restarts so the next lock needs
        // another five failures.
        var lockSeconds = entry.LockSeconds == 0 ? FirstLockSeconds : Math.Min(entry.LockSeconds * 2, MaxLockSeconds);
        return new Entry(0, now.AddSeconds(lockSeconds), lockSeconds, now);
    }

    private void Prune(DateTime now)
    {
        foreach (var (key, entry) in _entries)
        {
            if (now - entry.Touched > EntryTtl)
            {
                _entries.TryRemove(key, out _);
            }
        }
    }

    private sealed record Entry(int Failures, DateTime LockedUntil, int LockSeconds, DateTime Touched)
    {
        public static Entry Fresh { get; } = new(0, DateTime.MinValue, 0, DateTime.MinValue);
    }
}

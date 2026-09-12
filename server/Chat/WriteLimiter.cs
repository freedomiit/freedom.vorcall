using System.Diagnostics;
using System.Globalization;
using Vorcall.Server.Voice;

namespace Vorcall.Server.Chat;

// How many write frames one account may send. Both values have a usable default, so absent keys
// are fine; a key that is present but unusable fails the boot like the other Vorcall options.
public sealed record ChatLimits
{
    public const int DefaultBurst = 20;
    public const double DefaultPerSecond = 2;

    private ChatLimits(int burst, double perSecond)
    {
        Burst = burst;
        PerSecond = perSecond;
    }

    // The depth of the bucket: how many writes in a row a quiet connection may fire.
    public int Burst { get; }

    // The sustained rate the bucket refills at.
    public double PerSecond { get; }

    public static ChatLimits FromConfiguration(IConfiguration configuration)
        => new(
            ParseBurst(configuration["Vorcall:MessageBurst"]),
            ParsePerSecond(configuration["Vorcall:MessagesPerSecond"]));

    private static int ParseBurst(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return DefaultBurst;
        }

        if (!int.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var burst) || burst < 1)
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:MessageBurst' (expected a count of at least 1).");
        }

        return burst;
    }

    private static double ParsePerSecond(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return DefaultPerSecond;
        }

        if (!double.TryParse(configured.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture, out var perSecond)
            || !double.IsFinite(perSecond)
            || perSecond <= 0)
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:MessagesPerSecond' (expected a rate greater than 0).");
        }

        return perSecond;
    }
}

// The write ceiling of one live connection. Not thread-safe, like the bucket it wraps: only the
// connection's own receive loop calls it, and a replaced session gets a fresh one.
public sealed class WriteLimiter(ChatLimits limits)
{
    // Read by the metrics endpoint; every connection's receive loop writes it.
    public static long RejectedTotal;

    private readonly TokenBucket _bucket = new(limits.PerSecond, limits.Burst);

    public bool TryTake() => _bucket.TryTake(Stopwatch.GetTimestamp());
}

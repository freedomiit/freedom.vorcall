using System.Globalization;

namespace Vorcall.Server.Streams;

// The streamed-file proxy's switch and ceilings. Every value has a usable default, so an absent key
// is fine; a key that is present but unusable is not, and fails the boot like the other Vorcall
// options.
public sealed record StreamOptions
{
    public const int DefaultSenderTimeoutSeconds = 30;
    public const int DefaultMaxTransfersPerOwner = 8;

    public const int MaxPerMessage = 4;

    // Per offer. A streamed file is whatever was too large to be an attachment, so the ceiling is
    // only where a size stops being a file and starts being a client's mistake. long because
    // 1 << 40 overflows a signed 32-bit int.
    public const long MaxFileBytes = 1L << 40;

    private const int SenderTimeoutFloor = 1;
    private const int SenderTimeoutCeiling = 600;
    private const int TransfersFloor = 1;
    private const int TransfersCeiling = 64;

    private StreamOptions(bool enabled, TimeSpan senderTimeout, int maxTransfersPerOwner)
    {
        Enabled = enabled;
        SenderTimeout = senderTimeout;
        MaxTransfersPerOwner = maxTransfersPerOwner;
    }

    // The kill switch: with it off the endpoints are not mapped at all, and no SendMessage can
    // name a streamed file because none can be offered.
    public bool Enabled { get; }

    // How long a reader's GET waits for the owning client to push or decline before answering 504.
    public TimeSpan SenderTimeout { get; }

    // How many transfers one account may be asked to serve at once; a reader past that is refused
    // rather than the owner being asked for one more range.
    public int MaxTransfersPerOwner { get; }

    public static StreamOptions FromConfiguration(IConfiguration configuration)
        => new(
            ParseEnabled(configuration["Vorcall:StreamsEnabled"], "Vorcall:StreamsEnabled"),
            TimeSpan.FromSeconds(ParseRange(
                configuration["Vorcall:StreamSenderTimeoutSeconds"],
                "Vorcall:StreamSenderTimeoutSeconds",
                DefaultSenderTimeoutSeconds,
                SenderTimeoutFloor,
                SenderTimeoutCeiling)),
            ParseRange(
                configuration["Vorcall:StreamMaxTransfersPerOwner"],
                "Vorcall:StreamMaxTransfersPerOwner",
                DefaultMaxTransfersPerOwner,
                TransfersFloor,
                TransfersCeiling));

    private static bool ParseEnabled(string? configured, string key)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return true;
        }

        if (!bool.TryParse(configured.Trim(), out var enabled))
        {
            throw new InvalidOperationException($"Invalid configuration '{key}' (expected true or false).");
        }

        return enabled;
    }

    private static int ParseRange(string? configured, string key, int fallback, int minimum, int maximum)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return fallback;
        }

        if (!int.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var value) || value < minimum || value > maximum)
        {
            throw new InvalidOperationException($"Invalid configuration '{key}' (expected a value between {minimum} and {maximum}).");
        }

        return value;
    }
}

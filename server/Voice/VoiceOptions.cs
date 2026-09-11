using System.Globalization;

namespace Vorcall.Server.Voice;

// The relay's UDP endpoint and the screen share's ceilings. Every value has a usable default, so
// an absent key is fine; a key that is present but unusable is not, and fails the boot like the
// other Vorcall options.
public sealed record VoiceOptions
{
    public const int DefaultPort = 5005;
    public const int DefaultShareMaxKbps = 30_000;
    public const int DefaultMaxSharersPerRoom = 3;

    private const int ShareKbpsFloor = 1_000;
    private const int ShareKbpsCeiling = 200_000;
    private const int SharersFloor = 1;
    private const int SharersCeiling = 16;

    private VoiceOptions(bool enabled, int port, string host, bool shareEnabled, int shareMaxKbps, int maxSharersPerRoom)
    {
        Enabled = enabled;
        Port = port;
        Host = host;
        ShareEnabled = shareEnabled;
        ShareMaxKbps = shareMaxKbps;
        MaxSharersPerRoom = maxSharersPerRoom;
    }

    public bool Enabled { get; }

    public int Port { get; }

    // Empty is the normal single-host deployment: the client then sends media to the host it
    // already reached over the WebSocket.
    public string Host { get; }

    // The kill switch for screen sharing: voice keeps working without it.
    public bool ShareEnabled { get; }

    // The byte budget one sharer's video and share audio may spend, per session.
    public int ShareMaxKbps { get; }

    public int MaxSharersPerRoom { get; }

    public static VoiceOptions FromConfiguration(IConfiguration configuration)
        => new(
            ParseEnabled(configuration["Vorcall:VoiceEnabled"], "Vorcall:VoiceEnabled"),
            ParsePort(configuration["Vorcall:VoicePort"]),
            configuration["Vorcall:VoiceHost"]?.Trim() ?? string.Empty,
            ParseEnabled(configuration["Vorcall:ShareEnabled"], "Vorcall:ShareEnabled"),
            ParseRange(configuration["Vorcall:ShareMaxKbps"], "Vorcall:ShareMaxKbps", DefaultShareMaxKbps, ShareKbpsFloor, ShareKbpsCeiling),
            ParseRange(configuration["Vorcall:MaxSharersPerRoom"], "Vorcall:MaxSharersPerRoom", DefaultMaxSharersPerRoom, SharersFloor, SharersCeiling));

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

    private static int ParsePort(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return DefaultPort;
        }

        if (!int.TryParse(configured.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var port) || port is < 1 or > 65535)
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:VoicePort' (expected a port between 1 and 65535).");
        }

        return port;
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

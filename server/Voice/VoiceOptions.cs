using System.Globalization;

namespace Vorcall.Server.Voice;

// The relay's UDP endpoint. Every value has a usable default, so an absent key is fine; a key
// that is present but unusable is not, and fails the boot like the other Vorcall options.
public sealed record VoiceOptions
{
    public const int DefaultPort = 5005;

    private VoiceOptions(bool enabled, int port, string host)
    {
        Enabled = enabled;
        Port = port;
        Host = host;
    }

    public bool Enabled { get; }

    public int Port { get; }

    // Empty is the normal single-host deployment: the client then sends media to the host it
    // already reached over the WebSocket.
    public string Host { get; }

    public static VoiceOptions FromConfiguration(IConfiguration configuration)
        => new(
            ParseEnabled(configuration["Vorcall:VoiceEnabled"]),
            ParsePort(configuration["Vorcall:VoicePort"]),
            configuration["Vorcall:VoiceHost"]?.Trim() ?? string.Empty);

    private static bool ParseEnabled(string? configured)
    {
        if (string.IsNullOrWhiteSpace(configured))
        {
            return true;
        }

        if (!bool.TryParse(configured.Trim(), out var enabled))
        {
            throw new InvalidOperationException("Invalid configuration 'Vorcall:VoiceEnabled' (expected true or false).");
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
}

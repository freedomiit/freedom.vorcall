using System.Globalization;

namespace Vorcall.Server.Voice;

// The relay's UDP endpoint and the ceilings of the two video streams, a screen share and a camera.
// Every value has a usable default, so an absent key is fine; a key that is present but unusable is
// not, and fails the boot like the other Vorcall options.
public sealed record VoiceOptions
{
    public const int DefaultPort = 5005;
    public const int DefaultShareMaxKbps = 30_000;
    public const int DefaultShareAudioMaxKbps = 256;
    public const int DefaultMaxSharersPerRoom = 3;
    public const int DefaultCameraMaxKbps = 4_000;
    public const int DefaultMaxCamerasPerRoom = 8;
    public const int DefaultMaxWatchedCameras = 4;

    private const int ShareKbpsFloor = 1_000;
    private const int ShareKbpsCeiling = 200_000;
    private const int ShareAudioKbpsFloor = 64;
    private const int ShareAudioKbpsCeiling = 1_000;
    private const int SharersFloor = 1;
    private const int SharersCeiling = 16;
    private const int CameraKbpsFloor = 500;
    private const int CameraKbpsCeiling = 50_000;
    private const int CamerasFloor = 1;
    private const int CamerasCeiling = 16;
    private const int WatchedCamerasFloor = 1;
    private const int WatchedCamerasCeiling = 8;

    private VoiceOptions(
        bool enabled,
        int port,
        string host,
        bool shareEnabled,
        int shareMaxKbps,
        int shareAudioMaxKbps,
        int maxSharersPerRoom,
        bool cameraEnabled,
        int cameraMaxKbps,
        int maxCamerasPerRoom,
        int maxWatchedCameras)
    {
        Enabled = enabled;
        Port = port;
        Host = host;
        ShareEnabled = shareEnabled;
        ShareMaxKbps = shareMaxKbps;
        ShareAudioMaxKbps = shareAudioMaxKbps;
        MaxSharersPerRoom = maxSharersPerRoom;
        CameraEnabled = cameraEnabled;
        CameraMaxKbps = cameraMaxKbps;
        MaxCamerasPerRoom = maxCamerasPerRoom;
        MaxWatchedCameras = maxWatchedCameras;
    }

    public bool Enabled { get; }

    public int Port { get; }

    // Empty is the normal single-host deployment: the client then sends media to the host it
    // already reached over the WebSocket.
    public string Host { get; }

    // The kill switch for screen sharing: voice keeps working without it.
    public bool ShareEnabled { get; }

    // The byte budget one sharer's video may spend, per session.
    public int ShareMaxKbps { get; }

    // Share audio's own budget, separate from the video one so a burst of fragments cannot starve
    // the audio that goes with it.
    public int ShareAudioMaxKbps { get; }

    public int MaxSharersPerRoom { get; }

    // The kill switch for cameras, independent of the share one: either may run without the other.
    public bool CameraEnabled { get; }

    // The byte budget one camera may spend, per session.
    public int CameraMaxKbps { get; }

    public int MaxCamerasPerRoom { get; }

    // How many cameras one viewer may receive at once.
    public int MaxWatchedCameras { get; }

    public static VoiceOptions FromConfiguration(IConfiguration configuration)
        => new(
            ParseEnabled(configuration["Vorcall:VoiceEnabled"], "Vorcall:VoiceEnabled"),
            ParsePort(configuration["Vorcall:VoicePort"]),
            configuration["Vorcall:VoiceHost"]?.Trim() ?? string.Empty,
            ParseEnabled(configuration["Vorcall:ShareEnabled"], "Vorcall:ShareEnabled"),
            ParseRange(configuration["Vorcall:ShareMaxKbps"], "Vorcall:ShareMaxKbps", DefaultShareMaxKbps, ShareKbpsFloor, ShareKbpsCeiling),
            ParseRange(configuration["Vorcall:ShareAudioMaxKbps"], "Vorcall:ShareAudioMaxKbps", DefaultShareAudioMaxKbps, ShareAudioKbpsFloor, ShareAudioKbpsCeiling),
            ParseRange(configuration["Vorcall:MaxSharersPerRoom"], "Vorcall:MaxSharersPerRoom", DefaultMaxSharersPerRoom, SharersFloor, SharersCeiling),
            ParseEnabled(configuration["Vorcall:CameraEnabled"], "Vorcall:CameraEnabled"),
            ParseRange(configuration["Vorcall:CameraMaxKbps"], "Vorcall:CameraMaxKbps", DefaultCameraMaxKbps, CameraKbpsFloor, CameraKbpsCeiling),
            ParseRange(configuration["Vorcall:MaxCamerasPerRoom"], "Vorcall:MaxCamerasPerRoom", DefaultMaxCamerasPerRoom, CamerasFloor, CamerasCeiling),
            ParseRange(configuration["Vorcall:MaxWatchedCameras"], "Vorcall:MaxWatchedCameras", DefaultMaxWatchedCameras, WatchedCamerasFloor, WatchedCamerasCeiling));

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

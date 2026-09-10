using System.Text.Json;

namespace Vorcall.Server.Updates;

// Reads <ReleasesDir>/manifest.json and its detached signature <ReleasesDir>/manifest.sig. Both
// files are re-read on every call: the release workflow drops a new pair into the mount under a
// running server, and at a few hundred bytes any cache would only buy staleness.
public sealed class UpdateManifestStore(UpdatesOptions options, ILogger<UpdateManifestStore> logger)
{
    private const string ManifestFileName = "manifest.json";
    private const string SignatureFileName = "manifest.sig";
    private const int SignatureHexLength = 128;

    private static readonly JsonSerializerOptions ManifestJson = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    };

    public string ReleasesDir => options.ReleasesDir;

    // Null means "no release to offer": the directory, the manifest or the signature is missing
    // or unreadable, the signature is not a 128-hex Ed25519 signature, or the manifest does not
    // parse. Never throws, and never logs the contents.
    public LoadedManifest? TryLoad()
    {
        byte[] bytes;
        string signature;
        try
        {
            bytes = File.ReadAllBytes(Path.Combine(options.ReleasesDir, ManifestFileName));
            signature = File.ReadAllText(Path.Combine(options.ReleasesDir, SignatureFileName)).Trim();
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or NotSupportedException or ArgumentException)
        {
            // The normal state before the first release, so this is not a warning.
            logger.LogDebug("No readable release manifest in {ReleasesDir} ({Reason})", options.ReleasesDir, ex.GetType().Name);
            return null;
        }

        // The client verifies the signature itself, but the endpoint hands this string to Kestrel
        // as a response header, where anything outside ASCII throws; and a truncated or
        // half-written .sig file has to read as "no release", not as a 500.
        if (!IsHexSignature(signature))
        {
            logger.LogWarning("Release signature in {ReleasesDir} is not a 128-hex Ed25519 signature", options.ReleasesDir);
            return null;
        }

        UpdateManifest? manifest;
        try
        {
            manifest = JsonSerializer.Deserialize<UpdateManifest>(bytes, ManifestJson);
        }
        catch (Exception ex) when (ex is JsonException or NotSupportedException)
        {
            logger.LogWarning("Release manifest in {ReleasesDir} is not valid JSON", options.ReleasesDir);
            return null;
        }

        if (manifest is null || manifest.Platforms is null)
        {
            logger.LogWarning("Release manifest in {ReleasesDir} lists no platforms", options.ReleasesDir);
            return null;
        }

        return new LoadedManifest(bytes, signature, manifest);
    }

    private static bool IsHexSignature(string value)
        => value.Length == SignatureHexLength && value.All(char.IsAsciiHexDigit);
}

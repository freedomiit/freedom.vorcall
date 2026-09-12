using System.Globalization;
using System.Security.Cryptography;
using System.Text;

namespace Vorcall.Server.Tests.Infrastructure;

// The release a test host serves under /api/updates. The manifest is written as literal bytes
// because the endpoint must hand back exactly those: the detached signature covers them, so a
// re-serialised body would be a body no client can verify.
internal static class Releases
{
    public const string Version = "9.9.9";
    public const string Platform = "linux-x86_64";
    public const string AssetName = "vorcall-linux-x86_64";

    // Never verified by the server, only shape-checked: 128 hexadecimal characters.
    public const string Signature =
        "1111111111111111111111111111111111111111111111111111111111111111"
        + "2222222222222222222222222222222222222222222222222222222222222222";

    public static readonly byte[] AssetBytes = "a release build would be here\n"u8.ToArray();

    public static readonly string AssetSha256 = Convert.ToHexStringLower(SHA256.HashData(AssetBytes));

    public static readonly byte[] ManifestBytes = Encoding.UTF8.GetBytes(
        "{\"version\":\"" + Version + "\","
        + "\"notes\":\"integration suite\","
        + "\"published_at\":\"2026-01-01T00:00:00Z\","
        + "\"min_version\":\"0.0.1\","
        + "\"platforms\":{\"" + Platform + "\":{"
        + "\"path\":\"" + AssetName + "\","
        + "\"sha256\":\"" + AssetSha256 + "\","
        + "\"size\":" + AssetBytes.Length.ToString(CultureInfo.InvariantCulture)
        + "}}}");

    public static void Publish(string releasesDir)
    {
        File.WriteAllBytes(Path.Combine(releasesDir, "manifest.json"), ManifestBytes);
        File.WriteAllText(Path.Combine(releasesDir, "manifest.sig"), Signature + "\n");
        var assetDir = Directory.CreateDirectory(Path.Combine(releasesDir, Version));
        File.WriteAllBytes(Path.Combine(assetDir.FullName, AssetName), AssetBytes);
    }
}

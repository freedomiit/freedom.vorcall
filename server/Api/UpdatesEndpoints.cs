using Microsoft.Net.Http.Headers;
using Vorcall.Server.Updates;

namespace Vorcall.Server.Api;

// Under /api so the pre-shared key middleware covers them, and behind a bearer token on top:
// releases are for members, not for the open internet.
public static class UpdatesEndpoints
{
    private const string SignatureHeader = "X-Vorcall-Manifest-Signature";
    private const string AssetContentType = "application/octet-stream";

    public static void Map(WebApplication app)
    {
        app.MapGet("/api/updates/manifest", GetManifest).RequireAuthorization();
        app.MapGet("/api/updates/{version}/{file}", GetAsset).RequireAuthorization();
    }

    // GET /api/updates/manifest -> manifest.json as it sits on disk, its detached signature in a
    // header. The bytes are never re-serialised: the signature covers exactly these.
    private static IResult GetManifest(HttpResponse response, UpdateManifestStore manifests)
    {
        if (manifests.TryLoad() is not { } loaded)
        {
            return Results.NotFound();
        }

        response.Headers[SignatureHeader] = loaded.Signature;
        response.Headers.CacheControl = "no-store";
        return Results.Bytes(loaded.Bytes, "application/json");
    }

    // GET /api/updates/{version}/{file} -> the client build the current manifest publishes.
    private static IResult GetAsset(string version, string file, UpdateManifestStore manifests)
    {
        if (manifests.TryLoad() is not { } loaded || !string.Equals(version, loaded.Manifest.Version, StringComparison.Ordinal))
        {
            return Results.NotFound();
        }

        // The name has to be one the manifest itself publishes, and a bare file name on top of
        // that: the filesystem is never consulted about what the request may address, so no
        // traversal, symlink or stray file under the mount is reachable.
        var asset = loaded.Manifest.Platforms.Values
            .FirstOrDefault(candidate => candidate is not null && string.Equals(candidate.Path, file, StringComparison.Ordinal));
        if (asset is null || !IsBareName(version) || !IsBareName(file))
        {
            return Results.NotFound();
        }

        // Rooted on purpose: Results.File resolves a relative path against wwwroot, which this
        // server does not have, while Vorcall:ReleasesDir is relative in development.
        var path = Path.GetFullPath(Path.Combine(manifests.ReleasesDir, version, file));
        if (!File.Exists(path))
        {
            return Results.NotFound();
        }

        // The entity tag is the manifest's own hash of the file, so a repeated or resumed download
        // revalidates against the release rather than against a timestamp. A hash that is not a
        // usable tag costs revalidation; it must not fail the download.
        var entityTag = EntityTagHeaderValue.TryParse($"\"{asset.Sha256}\"", out var parsed) ? parsed : null;
        return Results.File(
            path,
            contentType: AssetContentType,
            fileDownloadName: null,
            lastModified: File.GetLastWriteTimeUtc(path),
            entityTag: entityTag,
            enableRangeProcessing: true);
    }

    private static bool IsBareName(string value)
        => value.Length > 0
        && value is not ("." or "..")
        && !value.Contains('/')
        && !value.Contains('\\')
        && !value.Contains("..", StringComparison.Ordinal)
        && string.Equals(Path.GetFileName(value), value, StringComparison.Ordinal);
}
